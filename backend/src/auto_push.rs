//! Queuing a finished backup into the owner's Drive without anyone clicking anything.
//!
//! Hooked up from [`shared::extensions::Extension::initialize`] against `ServerBackup`'s
//! event emitter (the pattern the Events docs describe), so it runs once per completed
//! backup. Two things shape the code here:
//!
//! * It runs *after* the panel has already finished the backup, from a listener whose
//!   errors core logs and drops. Nothing it does may look like it can fail the backup,
//!   so every expected miss - feature switched off, account unlinked, backup already
//!   gone - returns `Ok(())` with no row queued rather than raising.
//! * It only ever queues the **server's owner**. A subuser's `gdrive.push` can arrive
//!   through their role, and resolving role grants is the permission manager's job -
//!   which an event listener has no handle on. Subusers keep pushing their own copies
//!   from the account page, exactly as before.

use shared::{
    State,
    models::server_backup::{ServerBackup, ServerBackupKind},
};

use crate::{
    models::{GDriveConnection, GDrivePush},
    rules::PushRules,
    settings,
};

/// React to a backup finishing. The listener has already filtered the event down to a
/// creation, so this decides whether *that* backup should be queued.
pub async fn on_backup_created(
    state: &State,
    backup: &ServerBackup,
    successful: bool,
) -> Result<(), anyhow::Error> {
    // Finished-ness is deliberately *not* read from `backup.completed`: the emitter
    // hands over the snapshot its middleware fetched when the request began, and the
    // completion `UPDATE … completed = NOW()` runs after that fetch - so every
    // `CreationCompleted` event carries `completed == None`, and trusting it here was
    // what made auto-push silently queue nothing. The row is asked instead, in
    // `owner_of_finished_backup` below, the same way the owner already was.
    if !successful {
        return Ok(());
    }

    let settings = settings::current(state).await?;

    if !settings.auto_push {
        return Ok(());
    }

    // Database backups ride the same switch as everything else now, but the operator
    // can still refuse them: queueing one while the switch is off would only build a
    // row the push route would have turned away.
    if backup.kind == ServerBackupKind::DatabaseInstance && !settings.push_database_backups {
        return Ok(());
    }

    // The relation the emitter hands over is whatever the emitting path happened to
    // populate, so the owner is read straight from the row instead of trusting it.
    let Some(target) = owner_and_server_of_finished_backup(state, backup.uuid).await? else {
        return Ok(());
    };

    // The include/exclude globs are a property of the *server*, so the name travels
    // with the owner out of the same query.
    if !PushRules::from_settings(&settings).allows(&target.server_name) {
        tracing::debug!(
            backup = %backup.uuid,
            server = %target.server_name,
            "skipping google drive auto-push: excluded by push rules"
        );
        return Ok(());
    }

    // Unlinked accounts queue nothing: the worker would claim the row, find no refresh
    // token, and burn all five attempts on a failure nobody can fix but the user.
    if GDriveConnection::by_user_uuid(&state.database, target.owner_uuid)
        .await?
        .is_none()
    {
        return Ok(());
    }

    GDrivePush::enqueue(&state.database, backup.uuid, target.owner_uuid).await?;

    tracing::info!(
        backup = %backup.uuid,
        user = %target.owner_uuid,
        "queued finished backup for automatic google drive push"
    );

    Ok(())
}

/// Who owns a finished backup and what the server is called, provided the backup row
/// itself says the backup is finished and still exists.
///
/// Both facts come from the row rather than the event for the same reason: the
/// emitter's snapshot predates the completion `UPDATE`, so `completed` on it is always
/// `None` at this moment. A `None` here means "nothing to queue" - unfinished, already
/// deleted, or gone - which is the safe answer for a listener whose errors are dropped.
async fn owner_and_server_of_finished_backup(
    state: &State,
    backup_uuid: uuid::Uuid,
) -> Result<Option<BackupTarget>, anyhow::Error> {
    // A plain runtime query rather than `query!`: `servers`/`server_backups` are core
    // tables this extension has no offline metadata for.
    let row = sqlx::query(
        "SELECT servers.owner_uuid, servers.name AS server_name
         FROM server_backups
         JOIN servers ON servers.uuid = server_backups.server_uuid
         WHERE server_backups.uuid = $1
           AND server_backups.completed IS NOT NULL
           AND server_backups.deleted IS NULL
           AND server_backups.deleting IS NULL",
    )
    .bind(backup_uuid)
    .fetch_optional(state.database.read())
    .await?;

    row.map(|row| {
        use sqlx::Row;

        Ok(BackupTarget {
            owner_uuid: row.try_get("owner_uuid")?,
            server_name: row.try_get::<String, _>("server_name")?.into(),
        })
    })
    .transpose()
}

struct BackupTarget {
    owner_uuid: uuid::Uuid,
    server_name: compact_str::CompactString,
}
