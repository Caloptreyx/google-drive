//! Row access for the extension's two tables.
//!
//! These use the runtime `sqlx::query(...)` API rather than the `sqlx::query!` macro on
//! purpose: the macro needs offline metadata in `.sqlx/`, which doesn't exist for an
//! extension's own tables until someone runs a live migration. Enum columns are bound as
//! `&str` through an explicit `CAST(...) AS <enum>` instead of deriving `sqlx::Type`, for
//! the same reason - no compile-time metadata required.

use compact_str::CompactString;
use shared::prelude::*;
use sqlx::Row;

pub const CONNECTION_COLUMNS: &str = "user_uuid, account_email, folder_id, folder_name, refresh_token, scope, connected_at, last_used_at, needs_reauth_at";

#[derive(Clone, Debug)]
pub struct GDriveConnection {
    /// Carried on the row itself so callers holding a connection - the token helper, for
    /// one - don't have to thread the user id alongside it.
    pub user_uuid: uuid::Uuid,
    pub account_email: CompactString,
    pub folder_id: CompactString,
    pub folder_name: CompactString,
    /// Encrypted at rest with the Panel's application key. Never returned to the frontend.
    pub refresh_token: CompactString,
    /// The scopes the user actually granted, echoed back by Google.
    pub scope: CompactString,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set when Google rejected the stored refresh token (`invalid_grant`): only the
    /// user reconnecting can clear it (or a later successful refresh - see
    /// [`GDriveConnection::clear_needs_reauth`]). While set, the worker stops claiming
    /// this user's pushes so no attempts are burned on a grant that cannot work.
    pub needs_reauth_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl GDriveConnection {
    fn map(row: &sqlx::postgres::PgRow) -> Result<Self, shared::database::DatabaseError> {
        Ok(Self {
            user_uuid: row.try_get("user_uuid")?,
            account_email: row.try_get::<String, _>("account_email")?.into(),
            folder_id: row.try_get::<String, _>("folder_id")?.into(),
            folder_name: row.try_get::<String, _>("folder_name")?.into(),
            refresh_token: row.try_get::<String, _>("refresh_token")?.into(),
            scope: row.try_get::<String, _>("scope")?.into(),
            connected_at: row.try_get("connected_at")?,
            last_used_at: row.try_get("last_used_at")?,
            needs_reauth_at: row.try_get("needs_reauth_at")?,
        })
    }

    pub async fn by_user_uuid(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
    ) -> Result<Option<Self>, shared::database::DatabaseError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {CONNECTION_COLUMNS} FROM gdrive_connections WHERE user_uuid = $1"
        )))
        .bind(user_uuid)
        .fetch_optional(database.read())
        .await?;

        row.try_map(|row| Self::map(&row))
    }

    /// Upsert: linking twice simply replaces the previous grant.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
        account_email: &str,
        folder_id: &str,
        folder_name: &str,
        refresh_token: &str,
        scope: &str,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            r#"
            INSERT INTO gdrive_connections
                (user_uuid, account_email, folder_id, folder_name, refresh_token, scope, connected_at, last_used_at)
            VALUES ($1, $2, $3, $4, $5, $6, now(), now())
            ON CONFLICT (user_uuid) DO UPDATE SET
                account_email = EXCLUDED.account_email,
                folder_id = EXCLUDED.folder_id,
                folder_name = EXCLUDED.folder_name,
                refresh_token = EXCLUDED.refresh_token,
                scope = EXCLUDED.scope,
                connected_at = now(),
                last_used_at = now(),
                -- Re-linking is exactly the act that revives a dead grant, so the flag
                -- comes down here rather than needing a separate clear call.
                needs_reauth_at = NULL
            "#,
        )
        .bind(user_uuid)
        .bind(account_email)
        .bind(folder_id)
        .bind(folder_name)
        .bind(refresh_token)
        .bind(scope)
        .execute(database.write())
        .await?;

        Ok(())
    }

    pub async fn delete(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query("DELETE FROM gdrive_connections WHERE user_uuid = $1")
            .bind(user_uuid)
            .execute(database.write())
            .await?;

        Ok(())
    }

    pub async fn touch(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query("UPDATE gdrive_connections SET last_used_at = now() WHERE user_uuid = $1")
            .bind(user_uuid)
            .execute(database.write())
            .await?;

        Ok(())
    }

    /// Persist a rotated refresh token. Google may hand back a different refresh token on
    /// any refresh grant, and keeping the stale one would silently break every later push.
    pub async fn update_refresh_token(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
        refresh_token: &str,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query("UPDATE gdrive_connections SET refresh_token = $2 WHERE user_uuid = $1")
            .bind(user_uuid)
            .bind(refresh_token)
            .execute(database.write())
            .await?;

        Ok(())
    }

    /// Flag the grant as dead after Google refused to refresh it. First strike only -
    /// a transient hiccup keeps the timestamp it already has rather than being masked.
    pub async fn mark_needs_reauth(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            "UPDATE gdrive_connections SET needs_reauth_at = now() WHERE user_uuid = $1 AND needs_reauth_at IS NULL",
        )
        .bind(user_uuid)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// Take the flag back down when a refresh succeeds after all: an `invalid_grant`
    /// Google later walks back (clock skew, a transient rejection) must not wedge the
    /// account forever behind a banner nothing can clear.
    pub async fn clear_needs_reauth(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query("UPDATE gdrive_connections SET needs_reauth_at = NULL WHERE user_uuid = $1")
            .bind(user_uuid)
            .execute(database.write())
            .await?;

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl PushStatus {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }

    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GDrivePush {
    pub backup_uuid: uuid::Uuid,
    pub user_uuid: uuid::Uuid,
    pub drive_file_id: Option<CompactString>,
    pub status: PushStatus,
    pub attempts: i32,
    pub bytes_sent: i64,
    pub total_bytes: i64,
    /// Google's resumable session URI, valid for a week. Storing it is what lets an
    /// upload interrupted by a panel restart continue rather than start over.
    pub session_uri: Option<CompactString>,
    pub last_error: Option<CompactString>,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// `status` is cast to text on the way out: `gdrive_push_status` is an extension-defined
/// enum, so sqlx has no `Decode` impl for it and decoding straight into a `String` would
/// fail at runtime. The cast costs nothing and keeps `PushStatus::parse` in charge.
pub const PUSH_COLUMNS: &str = "backup_uuid, user_uuid, drive_file_id, status::text AS status, attempts, bytes_sent, total_bytes, session_uri, last_error, requested_at, updated_at";

impl GDrivePush {
    fn map(row: &sqlx::postgres::PgRow) -> Result<Self, shared::database::DatabaseError> {
        Ok(Self {
            backup_uuid: row.try_get("backup_uuid")?,
            user_uuid: row.try_get("user_uuid")?,
            drive_file_id: row
                .try_get::<Option<String>, _>("drive_file_id")?
                .map(CompactString::from),
            status: PushStatus::parse(&row.try_get::<String, _>("status")?),
            attempts: row.try_get("attempts")?,
            bytes_sent: row.try_get("bytes_sent")?,
            total_bytes: row.try_get("total_bytes")?,
            session_uri: row
                .try_get::<Option<String>, _>("session_uri")?
                .map(CompactString::from),
            last_error: row
                .try_get::<Option<String>, _>("last_error")?
                .map(CompactString::from),
            requested_at: row.try_get("requested_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    /// Queue a backup for upload. Idempotent - re-queueing a row that is already
    /// `pending` or `running` leaves it alone, anything finished or given up on is put
    /// back at the front of the queue. That's what makes it safe to call from a UI
    /// button, a retry, and the automatic push listener without double-uploading.
    ///
    /// `drive_file_id` is deliberately never cleared: the row keeps pointing at the copy
    /// in Drive this upload is about to replace, and the worker deletes the old file once
    /// the new one has landed. Clearing it here would strand the previous copy as an
    /// orphan nobody could find again.
    pub async fn enqueue(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            r#"
            INSERT INTO gdrive_pushes (backup_uuid, user_uuid, status)
            VALUES ($1, $2, 'pending')
            ON CONFLICT (backup_uuid, user_uuid) DO UPDATE SET
                status = CASE WHEN gdrive_pushes.status IN ('completed', 'failed')
                              THEN 'pending'::gdrive_push_status
                              ELSE gdrive_pushes.status END,
                attempts = CASE WHEN gdrive_pushes.status IN ('completed', 'failed') THEN 0
                                ELSE gdrive_pushes.attempts END,
                last_error = CASE WHEN gdrive_pushes.status IN ('completed', 'failed') THEN NULL
                                  ELSE gdrive_pushes.last_error END,
                bytes_sent = CASE WHEN gdrive_pushes.status = 'completed' THEN 0
                                  ELSE gdrive_pushes.bytes_sent END,
                total_bytes = CASE WHEN gdrive_pushes.status = 'completed' THEN 0
                                   ELSE gdrive_pushes.total_bytes END,
                session_uri = CASE WHEN gdrive_pushes.status = 'completed' THEN NULL
                                   ELSE gdrive_pushes.session_uri END,
                requested_at = now(),
                updated_at = now()
            "#,
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// Every push made for one backup, one per user who asked for it.
    ///
    /// Used when a backup is retired: each row carries the file id in Drive that has to
    /// go with it. Backups are only ever soft-deleted, so these rows are still here by
    /// the time `DeletionCompleted` fires.
    pub async fn for_backup(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
    ) -> Result<Vec<Self>, shared::database::DatabaseError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {PUSH_COLUMNS} FROM gdrive_pushes WHERE backup_uuid = $1 ORDER BY requested_at"
        )))
        .bind(backup_uuid)
        .fetch_all(database.read())
        .await?;

        rows.iter().map(Self::map).collect()
    }

    /// One push, looked up by its composite key.
    pub async fn by_key(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<Option<Self>, shared::database::DatabaseError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {PUSH_COLUMNS} FROM gdrive_pushes WHERE backup_uuid = $1 AND user_uuid = $2"
        )))
        .bind(backup_uuid)
        .bind(user_uuid)
        .fetch_optional(database.read())
        .await?;

        row.try_map(|row| Self::map(&row))
    }

    /// Drop a push from the queue. Returns whether a row was actually removed, which is
    /// what the route turns into a 404.
    ///
    /// A `running` row goes too - that is the point. The worker re-reads the row before
    /// every chunk, so deleting it is how an upload that is already streaming is stopped:
    /// the session is simply abandoned, and Google never assembles a file from it.
    pub async fn delete(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<bool, shared::database::DatabaseError> {
        let result =
            sqlx::query("DELETE FROM gdrive_pushes WHERE backup_uuid = $1 AND user_uuid = $2")
                .bind(backup_uuid)
                .bind(user_uuid)
                .execute(database.write())
                .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Whether the push still exists, asked by the worker before it commits more bytes
    /// to a session nobody is waiting for any more.
    pub async fn exists(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<bool, shared::database::DatabaseError> {
        let found = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT backup_uuid FROM gdrive_pushes WHERE backup_uuid = $1 AND user_uuid = $2",
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .fetch_optional(database.read())
        .await?;

        Ok(found.is_some())
    }

    /// Record the resumable session Google handed us, plus the total we're about to send.
    pub async fn set_session(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        session_uri: &str,
        total_bytes: i64,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            r#"
            UPDATE gdrive_pushes
            SET session_uri = $3, total_bytes = $4, bytes_sent = 0, updated_at = now()
            WHERE backup_uuid = $1 AND user_uuid = $2
            "#,
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .bind(session_uri)
        .bind(total_bytes)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// Record how far Google has confirmed, so progress survives a restart.
    pub async fn set_progress(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        bytes_sent: i64,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            "UPDATE gdrive_pushes SET bytes_sent = $3, updated_at = now() WHERE backup_uuid = $1 AND user_uuid = $2",
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .bind(bytes_sent)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// Forget the session, which is what forces the next attempt to ask Google for a new
    /// one - the correct move whenever Google reports the old session as expired.
    pub async fn clear_session(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            "UPDATE gdrive_pushes SET session_uri = NULL, updated_at = now() WHERE backup_uuid = $1 AND user_uuid = $2",
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// Claim the next runnable push. Returns `None` when the queue is empty.
    ///
    /// The `FOR UPDATE SKIP LOCKED` is what stops two concurrent ticks claiming the same
    /// row. A `running` row older than 15 minutes is treated as abandoned - that's the
    /// recovery path for a push the panel was killed mid-stream on, since background
    /// tasks get no shutdown hook of their own. `attempts < max` is the give-up bound.
    ///
    /// The selector is matched on `(backup_uuid, user_uuid)` rather than `backup_uuid`
    /// alone: the key is composite because the owner and every subuser queue their *own*
    /// copy of a shared backup into their own Drive. Matching on the backup alone would
    /// claim - and burn an attempt on - every user's row for it while only ever returning
    /// one of them, so the rows this call didn't return would sit in `running` until the
    /// 15 minute reaper dragged them back, failing spuriously along the way.
    ///
    /// Rows owned by a connection flagged `needs_reauth_at` are skipped entirely: the
    /// grant is dead, every attempt would fail the same way, and burning all five only
    /// to end at `failed` hides the real fix (the user reconnecting) behind a generic
    /// error. The rows stay `pending` and resume the moment the flag comes down.
    pub async fn claim_next(
        database: &shared::database::Database,
        max_attempts: i32,
    ) -> Result<Option<Self>, shared::database::DatabaseError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"
            UPDATE gdrive_pushes
            SET status = 'running', attempts = attempts + 1, updated_at = now()
            WHERE (backup_uuid, user_uuid) = (
                SELECT backup_uuid, user_uuid FROM gdrive_pushes
                WHERE ((status IN ('pending', 'failed') AND attempts < $1)
                   OR (status = 'running' AND updated_at < now() - interval '15 minutes'))
                AND NOT EXISTS (
                    SELECT 1 FROM gdrive_connections
                    WHERE gdrive_connections.user_uuid = gdrive_pushes.user_uuid
                      AND gdrive_connections.needs_reauth_at IS NOT NULL
                )
                ORDER BY requested_at
                FOR UPDATE SKIP LOCKED
                LIMIT 1
            )
            RETURNING {PUSH_COLUMNS}
            "#
        )))
        .bind(max_attempts)
        .fetch_optional(database.write())
        .await?;

        row.try_map(|row| Self::map(&row))
    }

    /// Record a finished upload, reporting whether there was still a row to record it on.
    ///
    /// The `false` case is the last-mile race this whole feature has: a `DELETE /push/…`
    /// landing while the final chunk was in flight removes the row, so the update matches
    /// nothing and the file that just finished uploading belongs to nobody. The caller
    /// gets told, and trashes it - a `false` it ignored would be an orphan in the folder
    /// with no way to link it back to anything.
    pub async fn mark_completed(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        drive_file_id: &str,
        bytes_sent: i64,
    ) -> Result<bool, shared::database::DatabaseError> {
        let result = sqlx::query(
            r#"
            UPDATE gdrive_pushes
            SET status = 'completed', drive_file_id = $3, bytes_sent = $4,
                session_uri = NULL, last_error = NULL, updated_at = now()
            WHERE backup_uuid = $1 AND user_uuid = $2
            "#,
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .bind(drive_file_id)
        .bind(bytes_sent)
        .execute(database.write())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_failed(
        database: &shared::database::Database,
        backup_uuid: uuid::Uuid,
        user_uuid: uuid::Uuid,
        error: &str,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query(
            r#"
            UPDATE gdrive_pushes
            SET status = 'failed', last_error = $3, updated_at = now()
            WHERE backup_uuid = $1 AND user_uuid = $2
            "#,
        )
        .bind(backup_uuid)
        .bind(user_uuid)
        .bind(error)
        .execute(database.write())
        .await?;

        Ok(())
    }

    /// One page of the pushes queued for one user, joined onto their backups so the page
    /// can show what each row actually is instead of a bare UUID.
    ///
    /// Paginated with core's `COUNT(*) OVER()` shape rather than handed over whole: this
    /// list only ever grows, it is re-read every couple of seconds while an upload runs,
    /// and an unbounded payload there is a page that gets slower every month.
    pub async fn overview_by_user_uuid(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
        page: i64,
        per_page: i64,
        search: Option<&str>,
    ) -> Result<(i64, Vec<PushOverview>), shared::database::DatabaseError> {
        let offset = (page - 1) * per_page;

        let columns = PUSH_COLUMNS
            .split(", ")
            .map(|column| format!("gdrive_pushes.{column}"))
            .collect::<Vec<_>>()
            .join(", ");

        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"
            SELECT {columns},
                   server_backups.name AS backup_name,
                   server_backups.server_uuid AS server_uuid,
                   COUNT(*) OVER() AS total_count
            FROM gdrive_pushes
            LEFT JOIN server_backups ON server_backups.uuid = gdrive_pushes.backup_uuid
            WHERE gdrive_pushes.user_uuid = $1
              AND {search}
            ORDER BY gdrive_pushes.requested_at DESC
            LIMIT $3 OFFSET $4
            "#,
            search = search_sql(2, &["server_backups.name"]),
        )))
        .bind(user_uuid)
        .bind(search)
        .bind(per_page)
        .bind(offset)
        .fetch_all(database.read())
        .await?;

        let total = rows
            .first()
            .map_or(Ok(0), |row| row.try_get("total_count"))?;

        let data = rows
            .iter()
            .map(|row| {
                Ok(PushOverview {
                    push: Self::map(row)?,
                    backup_name: row
                        .try_get::<Option<String>, _>("backup_name")?
                        .map(CompactString::from),
                    server_uuid: row.try_get("server_uuid")?,
                })
            })
            .collect::<Result<Vec<_>, shared::database::DatabaseError>>()?;

        // The total travels as a plain number rather than inside `Pagination<T>`: that
        // type wants `Serialize` on the row, and these are internal row structs the API
        // deliberately does *not* hand back as-is - the route maps them onto view types.
        Ok((total, data))
    }
}

/// A queued push annotated with enough of the backup to render a human-readable row.
pub struct PushOverview {
    pub push: GDrivePush,
    pub backup_name: Option<CompactString>,
    pub server_uuid: Option<uuid::Uuid>,
}

/// A finished server backup sitting somewhere this user can reach - a server they own or
/// one they are a subuser of - along with that user's own push status for it.
///
/// Ownership is enforced here rather than by loading every server and checking it one at
/// a time: one query, and the `EXISTS` mirrors `Server::by_user_identifier`. The stricter
/// `gdrive.push` check still happens on the push route, because a subuser can hold it
/// through their role rather than their subuser row, and only the permission manager
/// knows that.
pub struct PushableBackup {
    pub backup_uuid: uuid::Uuid,
    pub server_uuid: uuid::Uuid,
    pub server_name: CompactString,
    pub backup_name: CompactString,
    pub bytes: i64,
    pub files: i64,
    pub created: chrono::NaiveDateTime,
    pub push_status: Option<CompactString>,
}

/// The `search` predicate group for the two paginated queries below.
///
/// Built here rather than by `shared::models::search_sql`, which only landed in 1.2.3
/// (commit `c155c2dc`) while this extension still installs onto 1.2.2 panels - and a
/// panel compiles an extension against *its own* copy of `shared`, so calling it would
/// not build there. Same shape as the inline predicates core used before that commit:
/// one NULL-tolerant ILIKE disjunct per column, so an unbound search matches every row,
/// and the group only ever references `param` rather than introducing one of its own.
fn search_sql(param: u8, text_columns: &[&str]) -> CompactString {
    let mut clauses = Vec::with_capacity(text_columns.len() + 1);
    clauses.push(compact_str::format_compact!("${param}::text IS NULL"));

    for column in text_columns {
        clauses.push(compact_str::format_compact!(
            "{column} ILIKE '%' || ${param} || '%'"
        ));
    }

    compact_str::format_compact!("({})", clauses.join(" OR "))
}

pub async fn pushable_backups(
    database: &shared::database::Database,
    user_uuid: uuid::Uuid,
    page: i64,
    per_page: i64,
    search: Option<&str>,
    include_database_backups: bool,
) -> Result<(i64, Vec<PushableBackup>), shared::database::DatabaseError> {
    let offset = (page - 1) * per_page;

    // `COUNT(*) OVER()` rather than a second count query: one round trip, and the window
    // is computed over the same predicate that selects the page so `total` can never
    // disagree with the rows themselves. The search matches either name - someone typing
    // "creative" is looking for the server, someone typing "2024-08" the backup.
    //
    // Database-instance backups join the list only while the switch is on - the same
    // gate the push route and auto-push apply, so the table never offers a backup that
    // queueing it would then refuse.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        SELECT server_backups.uuid AS backup_uuid,
               servers.uuid AS server_uuid,
               servers.name AS server_name,
               server_backups.name AS backup_name,
               server_backups.bytes AS bytes,
               server_backups.files AS files,
               server_backups.created AS created,
               gdrive_pushes.status::text AS push_status,
               COUNT(*) OVER() AS total_count
        FROM server_backups
        JOIN servers ON servers.uuid = server_backups.server_uuid
        LEFT JOIN gdrive_pushes
               ON gdrive_pushes.backup_uuid = server_backups.uuid
              AND gdrive_pushes.user_uuid = $1
        WHERE server_backups.deleted IS NULL
          AND server_backups.deleting IS NULL
          AND server_backups.completed IS NOT NULL
          AND server_backups.successful
          AND (
                server_backups.kind = 'SERVER'
                OR ($5::boolean AND server_backups.kind = 'DATABASE_INSTANCE')
          )
          AND (
                servers.owner_uuid = $1
                OR EXISTS (
                    SELECT 1 FROM server_subusers
                    WHERE server_subusers.server_uuid = servers.uuid
                      AND server_subusers.user_uuid = $1
                )
          )
          AND {search}
        ORDER BY server_backups.created DESC
        LIMIT $3 OFFSET $4
        "#,
        search = search_sql(2, &["server_backups.name", "servers.name"]),
    )))
    .bind(user_uuid)
    .bind(search)
    .bind(per_page)
    .bind(offset)
    .bind(include_database_backups)
    .fetch_all(database.read())
    .await?;

    let total = rows
        .first()
        .map_or(Ok(0), |row| row.try_get("total_count"))?;

    let data = rows
        .iter()
        .map(|row| {
            Ok(PushableBackup {
                backup_uuid: row.try_get("backup_uuid")?,
                server_uuid: row.try_get("server_uuid")?,
                server_name: row.try_get::<String, _>("server_name")?.into(),
                backup_name: row.try_get::<String, _>("backup_name")?.into(),
                bytes: row.try_get("bytes")?,
                files: row.try_get("files")?,
                created: row.try_get("created")?,
                push_status: row
                    .try_get::<Option<String>, _>("push_status")?
                    .map(CompactString::from),
            })
        })
        .collect::<Result<Vec<_>, shared::database::DatabaseError>>()?;

    Ok((total, data))
}

/// The owner of a finished backup that no push row covers yet, with everything the
/// reconcile sweep needs to decide whether to queue it.
pub struct ReconcileCandidate {
    pub backup_uuid: uuid::Uuid,
    pub owner_uuid: uuid::Uuid,
    pub server_name: CompactString,
    /// `server_backups.kind::text` - `SERVER` or `DATABASE_INSTANCE`.
    pub kind: CompactString,
}

/// Finished backups in the lookback window whose owner is linked, whose push row is
/// missing entirely, and which are still within their owner's connection lifetime.
///
/// This is the catch-up path for events the listener never saw: the panel was
/// restarting when the completion event fired, the listener errored, or auto-push was
/// switched on after the fact. Deliberately bounded on three sides so it can never
/// become a backfill-the-world button:
///
/// * `completed >= now() - $interval` - only recent completions; a panel that was down
///   for an afternoon gets those afternoon's backups, not the entire archive history.
/// * `completed >= connected_at` - nothing that finished *before* the account was
///   linked: linking is not consent to upload what is already on disk.
/// * `LIMIT` set by the caller - a large backlog drains a batch at a time instead of
///   enqueuing thousands of uploads in one tick.
///
/// Rows whose connection is flagged `needs_reauth` are excluded by the join, matching
/// what `claim_next` would do anyway.
pub async fn reconcile_candidates(
    database: &shared::database::Database,
    lookback_hours: i32,
    limit: i64,
) -> Result<Vec<ReconcileCandidate>, shared::database::DatabaseError> {
    let rows = sqlx::query(
        r#"
        SELECT server_backups.uuid AS backup_uuid,
               servers.owner_uuid AS owner_uuid,
               servers.name AS server_name,
               server_backups.kind::text AS kind
        FROM server_backups
        JOIN servers ON servers.uuid = server_backups.server_uuid
        JOIN gdrive_connections
              ON gdrive_connections.user_uuid = servers.owner_uuid
             AND gdrive_connections.needs_reauth_at IS NULL
        LEFT JOIN gdrive_pushes
               ON gdrive_pushes.backup_uuid = server_backups.uuid
              AND gdrive_pushes.user_uuid = servers.owner_uuid
        WHERE server_backups.deleted IS NULL
          AND server_backups.deleting IS NULL
          AND server_backups.completed IS NOT NULL
          AND server_backups.successful
          AND server_backups.completed >= now() - make_interval(hours => $1)
          AND server_backups.completed >= gdrive_connections.connected_at
          AND gdrive_pushes.backup_uuid IS NULL
        ORDER BY server_backups.completed ASC
        LIMIT $2
        "#,
    )
    .bind(lookback_hours)
    .bind(limit)
    .fetch_all(database.read())
    .await?;

    rows.iter()
        .map(|row| {
            Ok(ReconcileCandidate {
                backup_uuid: row.try_get("backup_uuid")?,
                owner_uuid: row.try_get("owner_uuid")?,
                server_name: row.try_get::<String, _>("server_name")?.into(),
                kind: row.try_get::<String, _>("kind")?.into(),
            })
        })
        .collect()
}

/// Mirrored totals for one user - the account page's stats line, answered from the push
/// table rather than by paging Drive.
pub struct MirroredStats {
    pub completed_pushes: i64,
    pub bytes: i64,
}

pub async fn mirrored_stats(
    database: &shared::database::Database,
    user_uuid: uuid::Uuid,
) -> Result<MirroredStats, shared::database::DatabaseError> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS completed_pushes,
               COALESCE(SUM(bytes_sent), 0)::bigint AS bytes
        FROM gdrive_pushes
        WHERE user_uuid = $1 AND status = 'completed'
        "#,
    )
    .bind(user_uuid)
    .fetch_one(database.read())
    .await?;

    Ok(MirroredStats {
        completed_pushes: row.try_get("completed_pushes")?,
        bytes: row.try_get("bytes")?,
    })
}

/// The Drive file this user's finished push for a backup uploaded, if any.
///
/// The Backups table's "Restore from Drive" resolves through here: pushes are keyed per
/// user, so this can only ever name a file sitting in *this* caller's own Drive - the
/// server ownership rules still apply afterwards, through `by_server_uuid_uuid`.
pub async fn drive_file_for_backup(
    database: &shared::database::Database,
    backup_uuid: uuid::Uuid,
    user_uuid: uuid::Uuid,
) -> Result<Option<CompactString>, shared::database::DatabaseError> {
    let file_id: Option<String> = sqlx::query_scalar(
        "SELECT drive_file_id FROM gdrive_pushes
          WHERE backup_uuid = $1
            AND user_uuid = $2
            AND status = 'completed'
            AND drive_file_id IS NOT NULL
          ORDER BY updated_at DESC
          LIMIT 1",
    )
    .bind(backup_uuid)
    .bind(user_uuid)
    .fetch_optional(database.read())
    .await?;

    Ok(file_id.map(CompactString::from))
}

/// Register a Drive archive as a panel backup row so the backups list can show it and
/// wings' restore-completion callback can find it.
///
/// The same shape as core's `backups s3 import`, minus the backup configuration: the
/// bytes never touch the panel, and no configuration claims storage the archive isn't
/// actually in. `disk = 'LOCAL'` and `upload_path = <drive file id>` are the closest
/// honest bookkeeping - core's own Restore/Download buttons compute their URLs from the
/// disk and configuration and will not work for these rows (the extension ships its own
/// restore instead), while `node_uuid` must be right or the completion callback 404s.
///
/// The completed `gdrive_pushes` row written in the same transaction is what tells the
/// reconcile sweep the archive is *already in Drive*: without it the sweep would see a
/// finished backup with no push, queue it, and fail every attempt (there is no backup
/// configuration to download from), forever.
#[allow(clippy::too_many_arguments)]
pub async fn import_drive_backup(
    database: &shared::database::Database,
    user_uuid: uuid::Uuid,
    server_uuid: uuid::Uuid,
    node_uuid: uuid::Uuid,
    backup_uuid: uuid::Uuid,
    drive_file_id: &str,
    name: &str,
    bytes: i64,
) -> Result<shared::models::server_backup::ServerBackup, shared::database::DatabaseError> {
    use shared::prelude::*;

    let mut transaction = database.write().begin().await?;

    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"
        INSERT INTO server_backups
            (uuid, server_uuid, node_uuid, backup_configuration_uuid, name,
             ignored_files, checksum, successful, bytes, disk, upload_path, completed, kind)
        VALUES ($1, $2, $3, NULL, $4, '{{}}', 'sha1:unknown', true, $5, 'LOCAL', $6, NOW(), 'SERVER')
        RETURNING {}
        "#,
        shared::models::server_backup::ServerBackup::columns_sql(None)
    )))
    .bind(backup_uuid)
    .bind(server_uuid)
    .bind(node_uuid)
    .bind(name)
    .bind(bytes)
    .bind(drive_file_id)
    .fetch_one(&mut *transaction)
    .await?;

    // The copy in Drive is this user's, and for stats the archive counts as mirrored -
    // it is bytes the extension is accountable for, whether they were uploaded just now
    // or registered from an existing file.
    sqlx::query(
        r#"
        INSERT INTO gdrive_pushes
            (backup_uuid, user_uuid, status, drive_file_id, attempts, bytes_sent, total_bytes)
        VALUES ($1, $2, 'completed', $3, 0, $4, $4)
        ON CONFLICT (backup_uuid, user_uuid) DO NOTHING
        "#,
    )
    .bind(backup_uuid)
    .bind(user_uuid)
    .bind(drive_file_id)
    .bind(bytes)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;

    shared::models::server_backup::ServerBackup::map(None, &row)
}

/// Un-delete a soft-deleted backup row so a Drive copy that survived the deletion can
/// still be restored. The whole point of the Drive copy is that it outlives the panel's
/// reach, and a `deleted` row would otherwise make every restore of it a 404.
pub async fn resurrect_backup(
    database: &shared::database::Database,
    server_uuid: uuid::Uuid,
    backup_uuid: uuid::Uuid,
) -> Result<(), shared::database::DatabaseError> {
    sqlx::query(
        "UPDATE server_backups
            SET deleted = NULL, deleting = NULL
          WHERE uuid = $1 AND server_uuid = $2",
    )
    .bind(backup_uuid)
    .bind(server_uuid)
    .execute(database.write())
    .await?;

    Ok(())
}

pub const RESTORE_TOKEN_COLUMNS: &str = "token, user_uuid, backup_uuid, file_id";

/// A short-lived, single-use capability letting Wings fetch one archive through the
/// panel - the unauthenticated half of restore.
///
/// Minted while the restore route still has a session in hand, deleted on first fetch,
/// and expired after fifteen minutes: a token that leaks out of a Wings log or sits
/// unused because the restore never started is worthless. `file_id` travels on the row
/// rather than in the URL so the archive route never has to trust anything a caller
/// sends - the row is the entire authority. (The row also stores `server_uuid` and
/// `expires_at`, read only by SQL and by a human poking at the table.)
#[derive(Clone, Debug)]
pub struct GDriveRestoreToken {
    pub token: uuid::Uuid,
    pub user_uuid: uuid::Uuid,
    pub backup_uuid: uuid::Uuid,
    pub file_id: CompactString,
}

impl GDriveRestoreToken {
    fn map(row: &sqlx::postgres::PgRow) -> Result<Self, shared::database::DatabaseError> {
        Ok(Self {
            token: row.try_get("token")?,
            user_uuid: row.try_get("user_uuid")?,
            backup_uuid: row.try_get("backup_uuid")?,
            file_id: row.try_get::<String, _>("file_id")?.into(),
        })
    }

    /// Issue the next token, sweeping lapsed ones on the way past.
    ///
    /// The sweep lives here rather than in a background task: restores are rare, so the
    /// only cost of tying cleanup to mints is a handful of dead rows between them, and
    /// it keeps the extension free of another ticking worker to reason about.
    pub async fn mint(
        database: &shared::database::Database,
        user_uuid: uuid::Uuid,
        backup_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        file_id: &str,
    ) -> Result<Self, shared::database::DatabaseError> {
        sqlx::query("DELETE FROM gdrive_restore_tokens WHERE expires_at < now()")
            .execute(database.write())
            .await?;

        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO gdrive_restore_tokens
                (token, user_uuid, backup_uuid, server_uuid, file_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, now() + interval '15 minutes')
             RETURNING {RESTORE_TOKEN_COLUMNS}",
        )))
        .bind(uuid::Uuid::new_v4())
        .bind(user_uuid)
        .bind(backup_uuid)
        .bind(server_uuid)
        .bind(file_id)
        .fetch_one(database.write())
        .await?;

        Self::map(&row)
    }

    /// Claim a token for its one fetch: the `DELETE ... RETURNING` is what makes this
    /// single-use even under concurrent requests - exactly one of them gets the row,
    /// and every other one (replay, guess, expired) finds nothing.
    pub async fn consume(
        database: &shared::database::Database,
        token: uuid::Uuid,
    ) -> Result<Option<Self>, shared::database::DatabaseError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM gdrive_restore_tokens
              WHERE token = $1 AND expires_at > now()
              RETURNING {RESTORE_TOKEN_COLUMNS}",
        )))
        .bind(token)
        .fetch_optional(database.write())
        .await?;

        row.try_map(|row| Self::map(&row))
    }

    /// Throw a minted token away after its restore failed to start, so it can never be
    /// used to fetch an archive nothing is extracting.
    pub async fn discard(
        database: &shared::database::Database,
        token: uuid::Uuid,
    ) -> Result<(), shared::database::DatabaseError> {
        sqlx::query("DELETE FROM gdrive_restore_tokens WHERE token = $1")
            .bind(token)
            .execute(database.write())
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::search_sql;

    #[test]
    fn a_blank_search_still_matches_every_row() {
        // The NULL guard leads the group, so binding `None` selects everything rather
        // than nothing - the same contract core's helper gives the rest of the panel.
        let sql = search_sql(2, &["server_backups.name"]);

        assert!(sql.starts_with("($2::text IS NULL"), "got: {sql}");
        assert!(
            sql.contains("server_backups.name ILIKE '%' || $2 || '%'"),
            "got: {sql}"
        );
        assert!(sql.ends_with(')'), "got: {sql}");
    }

    #[test]
    fn the_group_only_ever_references_the_parameter_it_was_given() {
        let sql = search_sql(3, &["server_backups.name", "servers.name"]);

        // One NULL guard and one ILIKE per column, OR'd into a single group...
        assert_eq!(sql.matches(" OR ").count(), 2, "got: {sql}");
        // ...and every parameter it mentions is `$3`, so the surrounding query's
        // numbering ($1 for the user, $3/$4 for the page) is left alone.
        assert_eq!(sql.chars().filter(|ch| *ch == '$').count(), 3, "got: {sql}");
        assert_eq!(sql.matches("$3").count(), 3, "got: {sql}");
    }
}
