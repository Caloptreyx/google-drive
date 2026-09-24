//! User-facing API behind `add_client_api_router`, so every route here already has the
//! session middleware applied - no auth code of our own.
//!
//! Every leaf below declares its own path and they are registered flat by [`router`],
//! rather than answering `/` and being `nest`ed under a segment the way core wires its
//! routers. Flat is equivalent and keeps the single-purpose modules from each needing
//! its own `router()` shim.

use utoipa_axum::{router::OpenApiRouter, routes};

/// Base path for this extension's client routes. Namespaced by the package name so it
/// can't collide with core or another extension - an axum router merge collision panics
/// the process at startup.
pub const BASE: &str = "/extensions/dev.caloptreyx.gdrive";

mod status {
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::settings::GDriveSettingsData;

    #[derive(ToSchema, Serialize)]
    pub struct ConnectionView {
        account_email: compact_str::CompactString,
        folder_name: compact_str::CompactString,
        folder_id: compact_str::CompactString,
        scope: compact_str::CompactString,
        connected_at: chrono::DateTime<chrono::Utc>,
        last_used_at: Option<chrono::DateTime<chrono::Utc>>,
        /// Google refused this account's refresh token (`invalid_grant`): pushes are
        /// paused until the user reconnects. Drives the reconnect banner.
        needs_reauth: bool,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        configured: bool,
        connection: Option<ConnectionView>,
    }

    /// Whether the operator has configured Google credentials, and whether *this* user is
    /// linked.
    ///
    /// Deliberately not the push history: this is the call that decides whether the page
    /// polls, so it stays a fixed handful of rows no matter how much the user has pushed.
    /// The history lives at `GET /pushes`, which pages.
    #[utoipa::path(get, path = "/status", responses(
        (status = OK, body = inline(Response)),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.read")?;

        let settings = state.settings.get().await?;
        let configured = settings
            .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
            .map(|ext| ext.is_configured())
            .unwrap_or(false);
        drop(settings);

        let connection = crate::models::GDriveConnection::by_user_uuid(&state.database, user.uuid)
            .await?
            .map(|connection| ConnectionView {
                account_email: connection.account_email,
                folder_name: connection.folder_name,
                folder_id: connection.folder_id,
                scope: connection.scope,
                connected_at: connection.connected_at,
                last_used_at: connection.last_used_at,
                needs_reauth: connection.needs_reauth_at.is_some(),
            });

        ApiResponse::new_serialized(Response {
            configured,
            connection,
        })
        .ok()
    }
}

mod stats {
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::{google, tokens};

    #[derive(ToSchema, Serialize)]
    struct QuotaView {
        /// `null` for unlimited (Workspace) accounts - the badge shows usage alone.
        limit: Option<i64>,
        usage: Option<i64>,
        usage_in_drive: Option<i64>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        /// Uploads this user has completed, answered from the push table.
        completed_pushes: i64,
        /// Bytes those uploads sent.
        bytes: i64,
        /// Drive storage numbers, or `null` when Google wouldn't answer - the badge
        /// degrades to "unavailable" instead of failing the whole call.
        quota: Option<QuotaView>,
    }

    /// Stats and quota for the account page badge.
    ///
    /// Deliberately *not* folded into `/status`: status is polled while an upload runs,
    /// this is fetched on mount and refresh, and `about.get` has no business being on a
    /// polling loop. Panel-side stats always answer; only the quota half can degrade,
    /// and it does so per-field rather than as an error.
    #[utoipa::path(get, path = "/stats", responses(
        (status = OK, body = inline(Response)),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.read")?;

        let stats = crate::models::mirrored_stats(&state.database, user.uuid).await?;
        let quota = quota_for(&state, user.uuid).await;

        ApiResponse::new_serialized(Response {
            completed_pushes: stats.completed_pushes,
            bytes: stats.bytes,
            quota,
        })
        .ok()
    }

    /// The linked account's quota, or `None` for anything that can go wrong between
    /// here and Google - unlinked account, dead grant, rate limit. The badge reads
    /// `None` as "unavailable"; the stats around it must still answer.
    async fn quota_for(state: &shared::State, user_uuid: uuid::Uuid) -> Option<QuotaView> {
        let connection = crate::models::GDriveConnection::by_user_uuid(&state.database, user_uuid)
            .await
            .ok()??;

        let client = tokens::client().ok()?;

        // `about.get` sits on the `drive.file` scope's method list, so this never needs
        // a scope bump (and a re-consent prompt).
        let access_token = tokens::access_token(&client, state, &connection)
            .await
            .ok()?;

        match google::about(&client, &access_token).await {
            Ok(quota) => Some(QuotaView {
                limit: quota.limit,
                usage: quota.usage,
                usage_in_drive: quota.usage_in_drive,
            }),
            Err(err) => {
                tracing::warn!(user = %user_uuid, "gdrive quota lookup failed: {err:#}");
                None
            }
        }
    }
}

mod connect {
    use axum::http::StatusCode;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use tower_cookies::{Cookie, Cookies, cookie::SameSite};

    use crate::{
        google,
        routes::{CALLBACK_PATH, STATE_COOKIE, redirect_uri, state_cache_key},
        settings::GDriveSettingsData,
    };

    #[utoipa::path(get, path = "/connect", responses(
        (status = OK, body = inline(Response)),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        cookies: Cookies,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.connect")?;

        let secure;
        let callback;
        let client_id;

        {
            let settings = state.settings.get().await?;

            secure = settings.app.url.starts_with("https://");

            let ext = settings
                .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
                .map_err(|_| {
                    ApiResponse::error("google drive is not configured by the operator")
                        .with_status(StatusCode::PRECONDITION_FAILED)
                })?;

            if !ext.is_configured() {
                return ApiResponse::error("google drive is not configured by the operator")
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }

            client_id = ext.client_id.to_string();
            callback = redirect_uri(&settings.app.url);
        }

        // Random per-attempt state. The UUID doubles as the cache value, which is how
        // the unauthenticated callback works out whose account is being linked.
        let csrf_state = uuid::Uuid::new_v4().to_string();

        state
            .cache
            .set(&state_cache_key(&csrf_state), 10 * 60, &user.uuid)
            .await?;

        cookies.add(
            Cookie::build((STATE_COOKIE, csrf_state.clone()))
                .http_only(true)
                .same_site(SameSite::Lax)
                .secure(secure)
                .path(CALLBACK_PATH)
                .max_age(tower_cookies::cookie::time::Duration::minutes(10))
                .build(),
        );

        let url = google::authorize_url(&client_id, &callback, &csrf_state, Some(&user.email));

        ApiResponse::new_serialized(Response { url }).ok()
    }

    #[derive(serde::Serialize, utoipa::ToSchema)]
    struct Response {
        url: String,
    }
}

mod unlink {
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };

    #[utoipa::path(delete, path = "/connection", responses(
        (status = OK, body = inline(Response)),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.connect")?;

        crate::models::GDriveConnection::delete(&state.database, user.uuid).await?;

        ApiResponse::new_serialized(Response {}).ok()
    }

    #[derive(serde::Serialize, utoipa::ToSchema)]
    struct Response {}
}

mod backups {
    use axum::{extract::Query, http::StatusCode};
    use serde::Serialize;
    use shared::{
        ApiError, GetState,
        models::{
            Pagination, PaginationParamsWithSearch,
            user::{GetPermissionManager, GetUser},
        },
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    pub struct BackupView {
        backup_uuid: uuid::Uuid,
        server_uuid: uuid::Uuid,
        server_name: compact_str::CompactString,
        backup_name: compact_str::CompactString,
        bytes: i64,
        files: i64,
        created: chrono::NaiveDateTime,
        /// The user's own push status for this backup, if they've queued it before.
        push_status: Option<compact_str::CompactString>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        backups: Pagination<BackupView>,
    }

    /// Finished server backups this user can reach, newest first, with their own push
    /// status already attached so the page doesn't need a second round trip.
    ///
    /// Paged and searchable rather than capped. A hard limit was the honest bug in the
    /// first version: anyone with more backups than the cap simply could not reach or
    /// push the rest, and nothing on the page said so.
    #[utoipa::path(get, path = "/backups", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), params(
        ("page" = i64, Query, description = "The page number", example = "1"),
        ("per_page" = i64, Query, description = "The number of items per page", example = "25"),
        ("search" = Option<String>, Query, description = "Search term for items"),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        Query(params): Query<PaginationParamsWithSearch>,
    ) -> ApiResponseResult {
        if let Err(errors) = shared::utils::validate_data(&params) {
            return ApiResponse::new_serialized(ApiError::new_strings_value(errors))
                .with_status(StatusCode::BAD_REQUEST)
                .ok();
        }

        permissions.has_user_permission("gdrive.read")?;

        // Same read the status call makes: whether database backups show up as
        // pushable is decided by this switch, and the table must never offer a
        // backup the push route would then refuse.
        let include_database_backups = crate::routes::push_database_backups(&state).await;

        let (total, backups) = crate::models::pushable_backups(
            &state.database,
            user.uuid,
            params.page,
            params.per_page,
            params.search.as_deref(),
            include_database_backups,
        )
        .await?;

        ApiResponse::new_serialized(Response {
            backups: Pagination {
                total,
                per_page: params.per_page,
                page: params.page,
                data: backups
                    .into_iter()
                    .map(|backup| BackupView {
                        backup_uuid: backup.backup_uuid,
                        server_uuid: backup.server_uuid,
                        server_name: backup.server_name,
                        backup_name: backup.backup_name,
                        bytes: backup.bytes,
                        files: backup.files,
                        created: backup.created,
                        push_status: backup.push_status,
                    })
                    .collect(),
            },
        })
        .ok()
    }
}

mod pushes {
    use axum::{extract::Query, http::StatusCode};
    use serde::Serialize;
    use shared::{
        ApiError, GetState,
        models::{
            Pagination, PaginationParamsWithSearch,
            user::{GetPermissionManager, GetUser},
        },
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    pub struct PushView {
        backup_uuid: uuid::Uuid,
        status: compact_str::CompactString,
        drive_file_id: Option<compact_str::CompactString>,
        attempts: i32,
        bytes_sent: i64,
        total_bytes: i64,
        last_error: Option<compact_str::CompactString>,
        requested_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        backup_name: Option<compact_str::CompactString>,
        server_uuid: Option<uuid::Uuid>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        pushes: Pagination<PushView>,
    }

    /// This user's push history, newest first - what the Uploads table renders.
    ///
    /// Split from `/status` so the poll that drives live progress stays a constant size
    /// while this one pages through a list that only ever grows.
    #[utoipa::path(get, path = "/pushes", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), params(
        ("page" = i64, Query, description = "The page number", example = "1"),
        ("per_page" = i64, Query, description = "The number of items per page", example = "25"),
        ("search" = Option<String>, Query, description = "Search term for items"),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        Query(params): Query<PaginationParamsWithSearch>,
    ) -> ApiResponseResult {
        if let Err(errors) = shared::utils::validate_data(&params) {
            return ApiResponse::new_serialized(ApiError::new_strings_value(errors))
                .with_status(StatusCode::BAD_REQUEST)
                .ok();
        }

        permissions.has_user_permission("gdrive.read")?;

        let (total, pushes) = crate::models::GDrivePush::overview_by_user_uuid(
            &state.database,
            user.uuid,
            params.page,
            params.per_page,
            params.search.as_deref(),
        )
        .await?;

        ApiResponse::new_serialized(Response {
            pushes: Pagination {
                total,
                per_page: params.per_page,
                page: params.page,
                data: pushes
                    .into_iter()
                    .map(|overview| PushView {
                        backup_uuid: overview.push.backup_uuid,
                        status: overview.push.status.as_str().into(),
                        drive_file_id: overview.push.drive_file_id,
                        attempts: overview.push.attempts,
                        bytes_sent: overview.push.bytes_sent,
                        total_bytes: overview.push.total_bytes,
                        last_error: overview.push.last_error,
                        requested_at: overview.push.requested_at,
                        updated_at: overview.push.updated_at,
                        backup_name: overview.backup_name,
                        server_uuid: overview.server_uuid,
                    })
                    .collect(),
            },
        })
        .ok()
    }
}

mod files {
    use axum::{extract::Query, http::StatusCode};
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::{google, tokens};

    #[derive(ToSchema, Serialize)]
    pub struct FileView {
        file_id: compact_str::CompactString,
        name: compact_str::CompactString,
        bytes: i64,
        mime_type: compact_str::CompactString,
        /// True for the per-server (and user-made) folders the table can be drilled
        /// into. Folders have no download and must never offer a delete button: the
        /// files inside them are still tracked by `gdrive_pushes`, and trashing the
        /// folder would orphan every one of those rows.
        is_folder: bool,
        created_time: Option<chrono::DateTime<chrono::Utc>>,
        modified_time: Option<chrono::DateTime<chrono::Utc>>,
        /// Built from the id rather than read from `webViewLink`: `files.list` omits that
        /// field unless it is explicitly asked for, and the URL it would produce is
        /// deterministic anyway.
        view_url: String,
        /// The server the file is stamped with, when the extension pushed it: read from
        /// `appProperties`, so a folder or a file someone dropped in by hand has none
        /// and gets no restore button.
        server_uuid: Option<uuid::Uuid>,
        /// The panel backup this file mirrors - the other half of the restore button.
        backup_uuid: Option<uuid::Uuid>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        files: Vec<FileView>,
        next_page_token: Option<compact_str::CompactString>,
    }

    #[derive(serde::Deserialize)]
    pub struct Params {
        /// Handed straight back to Google, so the browser can ask for the next page.
        #[serde(default)]
        page_token: Option<String>,
        /// Which folder to list: absent means the account's backup root. With
        /// folder-per-server on, the root answers with one subfolder per server and
        /// this is how the table drills into them.
        #[serde(default)]
        folder_id: Option<String>,
    }

    pub(super) const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
    /// How many hops up the chain the ownership check walks before giving up. The
    /// extension's own layout is at most two (root → server folder → file), so this
    /// only has to tolerate subfolders a user made by hand in the Drive UI.
    const MAX_TREE_DEPTH: usize = 4;

    /// Whether `target` sits inside this user's backup folder, following parents upward
    /// instead of requiring the *direct* parent to be the root - with per-server folders
    /// an archive's parent is the server's subfolder, not the root itself.
    ///
    /// Shared with the delete route, which has the identical question to answer about a
    /// single file id. Bounded: an unbounded walk would happily climb someone else's
    /// tree all the way to their My Drive root, and the shared-app threat this check
    /// exists for is exactly a file id that belongs to another user.
    pub(super) async fn within_backup_tree(
        client: &reqwest::Client,
        access_token: &str,
        target: &str,
        root: &str,
    ) -> Result<bool, anyhow::Error> {
        let mut current = target.to_string();

        for _ in 0..=MAX_TREE_DEPTH {
            if current == root {
                return Ok(true);
            }

            let file = google::file_ref(client, access_token, &current).await?;
            let parents = file.parents.unwrap_or_default();

            if parents.iter().any(|parent| parent == root) {
                return Ok(true);
            }

            // Folders have a single parent; climbing it is the whole walk. Anything
            // with no parents is a Drive root - never *our* root, or the equality
            // check above would already have matched.
            match parents.into_iter().next() {
                Some(parent) => current = parent.to_string(),
                None => return Ok(false),
            }
        }

        Ok(false)
    }

    /// Page through what has already been pushed to the linked Drive.
    #[utoipa::path(get, path = "/files", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = shared::ApiError),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), params(
        (
            "page_token" = Option<String>, Query,
            description = "Continuation token returned as `next_page_token`",
        ),
        (
            "folder_id" = Option<String>, Query,
            description = "Folder to list; defaults to the account's backup root",
        ),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        Query(params): Query<Params>,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.read")?;

        let connection = crate::routes::require_connection(&state, user.uuid).await?;

        let client = tokens::client()?;
        let access_token = tokens::access_token(&client, &state, &connection).await?;

        // Drill-down target: validated as *this user's* before it is handed to Google,
        // for the same reason the delete route walks the chain - a folder id another
        // user picked out of the shared app's file space must not become a browsing
        // primitive.
        let listing_folder = match params.folder_id.as_deref() {
            Some(folder_id) if folder_id != connection.folder_id => {
                let inside = match within_backup_tree(
                    &client,
                    &access_token,
                    folder_id,
                    &connection.folder_id,
                )
                .await
                {
                    Ok(inside) => inside,
                    Err(err) => {
                        return ApiResponse::error(format!("google drive request failed: {err}"))
                            .with_status(StatusCode::PRECONDITION_FAILED)
                            .ok();
                    }
                };

                if !inside {
                    return ApiResponse::error("that folder is not in your backup folder")
                        .with_status(StatusCode::NOT_FOUND)
                        .ok();
                }

                folder_id.to_string()
            }
            Some(folder_id) => folder_id.to_string(),
            None => connection.folder_id.to_string(),
        };

        // Surfaced rather than propagated: a revoked grant or a rate limit is something
        // the person who clicked "browse" should see, not an internal error.
        let page = match google::list_files(
            &client,
            &access_token,
            &listing_folder,
            params.page_token.as_deref(),
        )
        .await
        {
            Ok(page) => page,
            Err(err) => {
                return ApiResponse::error(format!("google drive request failed: {err}"))
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }
        };

        let files = page
            .files
            .into_iter()
            .map(|file| {
                let mime_type = file
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into());
                let is_folder = mime_type.as_str() == FOLDER_MIME;
                let (server_uuid, backup_uuid) =
                    match google::backup_identity(file.app_properties.as_ref()) {
                        Some((server_uuid, backup_uuid)) => (Some(server_uuid), Some(backup_uuid)),
                        None => (None, None),
                    };

                FileView {
                    view_url: if is_folder {
                        format!("https://drive.google.com/drive/folders/{}", file.id)
                    } else {
                        format!("https://drive.google.com/file/d/{}/view", file.id)
                    },
                    bytes: file.size.unwrap_or(0),
                    mime_type,
                    file_id: file.id,
                    name: file.name,
                    is_folder,
                    created_time: file.created_time,
                    modified_time: file.modified_time,
                    server_uuid,
                    backup_uuid,
                }
            })
            .collect();

        ApiResponse::new_serialized(Response {
            files,
            next_page_token: page.next_page_token,
        })
        .ok()
    }
}

mod push {
    use axum::http::StatusCode;
    use garde::Validate;
    use serde::{Deserialize, Serialize};
    use shared::{
        GetState,
        models::{
            server::Server,
            server_backup::{ServerBackup, ServerBackupKind},
            user::{GetPermissionManager, GetUser},
        },
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    #[derive(ToSchema, Validate, Deserialize)]
    pub struct Payload {
        #[garde(skip)]
        server_uuid: uuid::Uuid,

        #[garde(skip)]
        backup_uuid: uuid::Uuid,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        queued: bool,
    }

    /// Queue one of the user's backups for upload to their linked Drive.
    ///
    /// The server is resolved through `Server::by_user_identifier` - the same call core's
    /// server-scoped middleware makes - so ownership and subuser rules are applied by
    /// core rather than re-implemented here, and `for_server` then lets the permission
    /// manager check `gdrive.push` against that specific server.
    #[utoipa::path(post, path = "/push", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = shared::ApiError),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = CONFLICT, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), request_body = inline(Payload))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        shared::Payload(data): shared::Payload<Payload>,
    ) -> ApiResponseResult {
        let server =
            match Server::by_user_identifier(&state.database, &user, &data.server_uuid.to_string())
                .await
            {
                Ok(Some(server)) => server,
                Ok(None) => {
                    return ApiResponse::error("server not found")
                        .with_status(StatusCode::NOT_FOUND)
                        .ok();
                }
                Err(err) => return Err(ApiResponse::from(err)),
            };

        permissions
            .for_server(&server)
            .has_server_permission("gdrive.push")?;

        let backup =
            match ServerBackup::by_server_uuid_uuid(&state.database, server.uuid, data.backup_uuid)
                .await
            {
                Ok(Some(backup)) if backup.deleted.is_none() => backup,
                Ok(_) => {
                    return ApiResponse::error("backup not found")
                        .with_status(StatusCode::NOT_FOUND)
                        .ok();
                }
                Err(err) => return Err(ApiResponse::from(err)),
            };

        if backup.deleting.is_some() {
            return ApiResponse::error("backup is being deleted")
                .with_status(StatusCode::CONFLICT)
                .ok();
        }
        if backup.completed.is_none() {
            return ApiResponse::error("backup has not completed yet")
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
        }
        if !backup.successful {
            return ApiResponse::error("backup failed and cannot be pushed")
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
        }
        // Database backups ride the same path but only while the operator allows them:
        // the switch the listing applies, so a row the table refused to show can't be
        // queued straight past it by hand. Any non-file kind is treated as
        // database-shaped - that is all `server_backups.kind` carries today.
        if backup.kind != ServerBackupKind::Server
            && !crate::routes::push_database_backups(&state).await
        {
            return ApiResponse::error(
                "pushing database backups is disabled in the Google Drive settings",
            )
            .with_status(StatusCode::PRECONDITION_FAILED)
            .ok();
        }

        crate::routes::require_connection(&state, user.uuid).await?;

        crate::models::GDrivePush::enqueue(&state.database, data.backup_uuid, user.uuid).await?;

        tracing::info!(
            "gdrive push queued for backup {} by user {}",
            data.backup_uuid,
            user.uuid
        );

        ApiResponse::new_serialized(Response { queued: true }).ok()
    }
}

mod delete_file {
    use axum::{extract::Path, http::StatusCode};
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::{google, tokens};

    #[derive(ToSchema, Serialize)]
    struct Response {
        deleted: bool,
    }

    /// Send one file in the linked Drive to the trash.
    ///
    /// Gated on `gdrive.manage` rather than `gdrive.read` because it destroys the only
    /// copy outside the panel. Ownership is a *chain* walk rather than a parent check:
    /// with folder-per-server an archive sits in its server's subfolder, so requiring
    /// the direct parent to be the root would refuse every file the new layout creates.
    /// Folders themselves are refused outright - trashing one takes every backup inside
    /// it while their `gdrive_pushes` rows still point at the files.
    #[utoipa::path(delete, path = "/files/{file_id}", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = shared::ApiError),
        (status = NOT_FOUND, body = shared::ApiError),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), params(
        (
            "file_id" = String, Path,
            description = "Drive file id to trash",
        ),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        Path(file_id): Path<String>,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.manage")?;

        let connection = crate::routes::require_connection(&state, user.uuid).await?;

        let client = tokens::client()?;
        let access_token = tokens::access_token(&client, &state, &connection).await?;

        let target = match google::file_ref(&client, &access_token, &file_id).await {
            Ok(target) => target,
            Err(err) => {
                return ApiResponse::error(format!("google drive request failed: {err}"))
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }
        };

        if target.mime_type.as_deref() == Some(super::files::FOLDER_MIME) {
            return ApiResponse::error(
                "folders cannot be deleted from here - delete the files inside them instead",
            )
            .with_status(StatusCode::BAD_REQUEST)
            .ok();
        }

        let inside = super::files::within_backup_tree(
            &client,
            &access_token,
            &file_id,
            &connection.folder_id,
        )
        .await
        .map_err(|err| {
            ApiResponse::error(format!("google drive request failed: {err}"))
                .with_status(StatusCode::PRECONDITION_FAILED)
        })?;

        if !inside {
            return ApiResponse::error("that file is not in your backup folder")
                .with_status(StatusCode::NOT_FOUND)
                .ok();
        }

        if let Err(err) = google::delete_file(&client, &access_token, &file_id).await {
            return ApiResponse::error(format!("google drive request failed: {err}"))
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
        }

        tracing::info!(user = %user.uuid, file = %file_id, "trashed google drive file");

        ApiResponse::new_serialized(Response { deleted: true }).ok()
    }
}

mod cancel {
    use axum::{extract::Path, http::StatusCode};
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::{GetPermissionManager, GetUser},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {
        cancelled: bool,
    }

    /// Take a push back out of the queue, or stop one that is already uploading.
    ///
    /// Scoped to the calling user by the key itself, so this can only ever touch their
    /// own queue. A finished push is refused rather than removed: its copy in Drive is
    /// what the Files table above lets them delete.
    #[utoipa::path(delete, path = "/push/{backup_uuid}", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = shared::ApiError),
        (status = CONFLICT, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), params(
        (
            "backup_uuid" = uuid::Uuid, Path,
            description = "Backup whose upload should be cancelled",
        ),
    ))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        Path(backup_uuid): Path<uuid::Uuid>,
    ) -> ApiResponseResult {
        permissions.has_user_permission("gdrive.read")?;

        let push = match crate::models::GDrivePush::by_key(&state.database, backup_uuid, user.uuid)
            .await?
        {
            Some(push) => push,
            None => {
                return ApiResponse::error("no upload is queued for that backup")
                    .with_status(StatusCode::NOT_FOUND)
                    .ok();
            }
        };

        if push.status == crate::models::PushStatus::Completed {
            return ApiResponse::error(
                "that upload already finished - delete the file from Drive instead",
            )
            .with_status(StatusCode::CONFLICT)
            .ok();
        }

        // The worker notices the row is gone and abandons its session; Google never
        // assembles a file from the bytes it already has.
        crate::models::GDrivePush::delete(&state.database, backup_uuid, user.uuid).await?;

        tracing::info!(backup = %backup_uuid, user = %user.uuid, "cancelled google drive push");

        ApiResponse::new_serialized(Response { cancelled: true }).ok()
    }
}

/// Each leaf declares its own path and is registered flat here - unlike core, where a
/// leaf answers `/` and its parent file nests the segment. Flat is equivalent and keeps
/// the single-purpose modules from each needing a four-line `router()` shim.
mod restore {
    use axum::http::StatusCode;
    use garde::Validate;
    use serde::{Deserialize, Serialize};
    use shared::{
        GetState,
        models::{
            server::{Server, ServerStatus},
            server_backup::{ServerBackup, ServerBackupEvent, ServerBackupKind},
            user::{GetPermissionManager, GetUser},
        },
        prelude::*,
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;
    use wings_api::BackupAdapter;

    use crate::{
        google,
        models::{
            GDriveRestoreToken, drive_file_for_backup, import_drive_backup, resurrect_backup,
        },
        tokens,
    };

    #[derive(ToSchema, Validate, Deserialize)]
    pub struct Payload {
        #[garde(skip)]
        server_uuid: uuid::Uuid,

        /// The panel backup to restore - the Backups table's entry point. The backup is
        /// resolved against `server_uuid`, so a uuid belonging to another server 404s.
        #[garde(skip)]
        backup_uuid: Option<uuid::Uuid>,

        /// A Drive file id instead - the Files table's entry point. Exactly one of the
        /// two must be given.
        #[garde(skip)]
        file_id: Option<String>,

        /// Wipe the server's directory before extracting, exactly as core's own restore
        /// option does. Defaults to off, again like core.
        #[garde(skip)]
        #[serde(default)]
        truncate_directory: bool,

        /// Apply the backup's recorded startup command/image - core's `restore_startup`,
        /// replicated because core's route is not the one running here.
        #[garde(skip)]
        #[serde(default)]
        restore_startup: bool,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        restored: bool,
    }

    /// Restore a server's files from a copy of its backup sitting in Google Drive.
    ///
    /// Core's own restore endpoint can never be pointed at Drive on 1.2.2: it computes
    /// the archive URL itself (an S3 presign, or `None` meaning "read your local file")
    /// and exposes no hook to override that, so this route replicates core's gates and
    /// status handshake and then drives Wings directly - with two deliberate choices:
    ///
    /// * `adapter: S3` + our own URL. Wings' S3 adapter accepts *any* download URL
    ///   without checking that the backup exists on disk, and parses the archive format
    ///   from the URL's last path segment - hence `/backup.tar.gz`. The URL leads to the
    ///   auth router's archive route, which streams the bytes panel-side from Drive
    ///   under the linking user's stored grant, so files stay private and no
    ///   link-sharing (or Google's large-file interstitial) is involved.
    /// * A Drive archive with no panel row gets one, through the same raw INSERT core's
    ///   `backups s3 import` uses - the backups list must know about the row before
    ///   Wings' completion callback arrives, and `node_uuid` has to be right for that
    ///   callback to find it.
    ///
    /// Database-instance backups are refused: their restore needs the instance status
    /// claims and db-agent checks that live on core's database route, which this
    /// feature does not take on.
    #[utoipa::path(post, path = "/restore", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = shared::ApiError),
        (status = NOT_FOUND, body = shared::ApiError),
        (status = CONFLICT, body = shared::ApiError),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = EXPECTATION_FAILED, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
        (status = INTERNAL_SERVER_ERROR, body = shared::ApiError),
    ), request_body = inline(Payload))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        user: GetUser,
        shared::Payload(data): shared::Payload<Payload>,
    ) -> ApiResponseResult {
        // Exactly one entry point, and neither alone: the Backups table hands a backup
        // uuid, the Files table a file id, and both together would be ambiguous about
        // which one the caller actually verified.
        if data.backup_uuid.is_some() == data.file_id.is_some() {
            return ApiResponse::error("give either backup_uuid or file_id, but not both")
                .with_status(StatusCode::BAD_REQUEST)
                .ok();
        }

        // Ownership and subuser rules stay core's problem, exactly as on push: resolved
        // through `by_user_identifier` rather than re-implemented here.
        let mut server =
            match Server::by_user_identifier(&state.database, &user, &data.server_uuid.to_string())
                .await
            {
                Ok(Some(server)) => server,
                Ok(None) => {
                    return ApiResponse::error("server not found")
                        .with_status(StatusCode::NOT_FOUND)
                        .ok();
                }
                Err(err) => return Err(ApiResponse::from(err)),
            };

        permissions
            .for_server(&server)
            .has_server_permission("gdrive.restore")?;

        let connection = crate::routes::require_connection(&state, user.uuid).await?;
        let client = tokens::client()?;
        let access_token = tokens::access_token(&client, &state, &connection).await?;

        // Resolve both halves while the session is still on the stack. The file-id path
        // re-verifies everything before believing it - untrashed, not a folder, inside
        // *this* caller's backup tree (the chain walk the delete route uses, because
        // `drive.file` only narrows to files one shared OAuth client created), stamped
        // for *this* server, and tagged with a backup at all.
        let (backup_uuid, file_id, props) = if let Some(backup_uuid) = data.backup_uuid {
            let file_id =
                match drive_file_for_backup(&state.database, backup_uuid, user.uuid).await? {
                    Some(file_id) => file_id,
                    None => {
                        return ApiResponse::error("no google drive copy of this backup")
                            .with_status(StatusCode::PRECONDITION_FAILED)
                            .ok();
                    }
                };

            (backup_uuid, file_id, None)
        } else {
            let requested = data.file_id.as_deref().expect("checked above");

            let props = match google::file_props(&client, &access_token, requested).await {
                Ok(props) => props,
                Err(err) => {
                    return ApiResponse::error(format!("google drive request failed: {err}"))
                        .with_status(StatusCode::PRECONDITION_FAILED)
                        .ok();
                }
            };

            if props.trashed.unwrap_or(false) {
                return ApiResponse::error("that file is in the Drive trash")
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }
            if props.mime_type.as_deref() == Some(super::files::FOLDER_MIME) {
                return ApiResponse::error(
                    "folders cannot be restored - restore a file from inside them",
                )
                .with_status(StatusCode::BAD_REQUEST)
                .ok();
            }

            let inside = super::files::within_backup_tree(
                &client,
                &access_token,
                &props.id,
                &connection.folder_id,
            )
            .await
            .map_err(|err| {
                ApiResponse::error(format!("google drive request failed: {err}"))
                    .with_status(StatusCode::PRECONDITION_FAILED)
            })?;

            if !inside {
                return ApiResponse::error("that file is not in your backup folder")
                    .with_status(StatusCode::NOT_FOUND)
                    .ok();
            }

            let Some((file_server_uuid, backup_uuid)) =
                google::backup_identity(props.app_properties.as_ref())
            else {
                return ApiResponse::error(
                    "that file carries no backup identity - only files pushed by this extension can be restored",
                )
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
            };

            if file_server_uuid != server.uuid {
                return ApiResponse::error("that file belongs to a different server")
                    .with_status(StatusCode::NOT_FOUND)
                    .ok();
            }

            let file_id = props.id.clone();
            (backup_uuid, file_id, Some(props))
        };

        let server_uuid = server.uuid;
        // The import below books the Drive copy against the caller's own push history.
        let user_uuid = user.uuid;

        tokio::spawn(async move {
            // --- The panel row ----------------------------------------------------
            // Resolved before the status claim: every gate here is a plain read, so a
            // refused restore never touches the server's status at all. The claim below
            // only happens once nothing can refuse anymore.
            let backup =
                match ServerBackup::by_server_uuid_uuid(&state.database, server_uuid, backup_uuid)
                    .await?
                {
                    Some(existing) => {
                        if existing.deleting.is_some() {
                            return ApiResponse::error("backup is being deleted")
                                .with_status(StatusCode::CONFLICT)
                                .ok();
                        }

                        if existing.deleted.is_some() {
                            // The Drive copy outlived the panel's deletion - which is what
                            // an off-site copy is for. Bring the row back rather than 404
                            // the one archive that survived.
                            resurrect_backup(&state.database, server_uuid, backup_uuid).await?;
                            tracing::info!(
                                server = %server_uuid,
                                backup = %backup_uuid,
                                "gdrive restore resurrected a deleted backup row"
                            );
                        }

                        existing
                    }
                    None => {
                        // Imported: the archive exists only in Drive. Register it now, or
                        // Wings' completion callback would arrive for a row nobody has
                        // heard of and 404.
                        let props = match props {
                            Some(props) => props,
                            None => google::file_props(&client, &access_token, &file_id).await?,
                        };

                        let imported = import_drive_backup(
                            &state.database,
                            user_uuid,
                            server_uuid,
                            server.node.uuid,
                            backup_uuid,
                            &file_id,
                            &props.name,
                            props.size.unwrap_or(0).max(0),
                        )
                        .await?;

                        tracing::info!(
                            server = %server_uuid,
                            backup = %backup_uuid,
                            file = %file_id,
                            "gdrive restore registered a drive-only archive as a backup row"
                        );

                        imported
                    }
                };

            if backup.completed.is_none() {
                return ApiResponse::error("backup has not completed yet")
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }
            if !backup.successful {
                return ApiResponse::error("backup failed and cannot be restored")
                    .with_status(StatusCode::PRECONDITION_FAILED)
                    .ok();
            }
            if backup.kind != ServerBackupKind::Server {
                return ApiResponse::error(
                    "database backups cannot be restored from Google Drive yet",
                )
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
            }

            // --- Core's status handshake ------------------------------------------
            // Claimed from `None` exactly like core: a server that is installing or
            // already restoring refuses with 417 instead of two restores racing. Every
            // early return from here on drops the transaction un-committed, which rolls
            // the claim back - only the success path at the bottom commits.
            let mut transaction = state.database.write().begin().await?;

            if !server
                .try_set_status(&mut *transaction, None, Some(ServerStatus::RestoringBackup))
                .await?
            {
                transaction.rollback().await?;

                return ApiResponse::error("server is not in a valid state to restore backup.")
                    .with_status(StatusCode::EXPECTATION_FAILED)
                    .ok();
            }

            if data.restore_startup
                && let Err(err) = backup
                    .restore_startup(&state, &mut transaction, &mut server)
                    .await
            {
                transaction.rollback().await?;
                tracing::error!(
                    server = %server_uuid,
                    backup = %backup_uuid,
                    "gdrive restore startup handling failed: {err:#}"
                );

                return ApiResponse::error("failed to apply the backup's startup settings")
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .ok();
            }

            // --- The fetch token --------------------------------------------------
            // Minted only now: a token that exists has a restore actually starting
            // behind it, and if Wings rejects the request below it is discarded again
            // so no stray capability outlives a restore that never ran.
            let settings = state.settings.get().await?;
            let app_url = settings.app.url.clone();
            drop(settings);

            let minted = GDriveRestoreToken::mint(
                &state.database,
                user.uuid,
                backup_uuid,
                server_uuid,
                &file_id,
            )
            .await?;
            let archive_url = crate::routes::restore_archive_url(&app_url, minted.token);

            // --- Wings ------------------------------------------------------------
            let wings = async {
                server
                    .node
                    .fetch_cached(&state.database)
                    .await?
                    .api_client(&state.database)
                    .await?
                    .post_servers_server_backup_backup_restore(
                        server_uuid,
                        backup_uuid,
                        &wings_api::servers_server_backup_backup_restore::post::RequestBody {
                            adapter: BackupAdapter::S3,
                            truncate_directory: data.truncate_directory,
                            download_url: Some(archive_url.as_str().into()),
                        },
                    )
                    .await?;

                anyhow::Ok(())
            }
            .await;

            if let Err(err) = wings {
                if let Err(discard) =
                    GDriveRestoreToken::discard(&state.database, minted.token).await
                {
                    tracing::warn!("gdrive restore token cleanup failed: {discard}");
                }
                transaction.rollback().await?;
                tracing::error!(
                    server = %server_uuid,
                    backup = %backup_uuid,
                    "gdrive restore rejected by the node: {err:#}"
                );

                return ApiResponse::error("failed to restore backup")
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .ok();
            }

            ServerBackup::get_event_emitter().emit(
                (*state).clone(),
                ServerBackupEvent::RestoreStarted {
                    backup: Box::new(backup),
                    server: Box::new(server),
                },
            );

            transaction.commit().await?;
            Server::invalidate_cached(&state.database, server_uuid).await;

            tracing::info!(
                server = %server_uuid,
                backup = %backup_uuid,
                user = %user.uuid,
                file = %file_id,
                truncate = data.truncate_directory,
                "gdrive restore started"
            );

            ApiResponse::new_serialized(Response { restored: true }).ok()
        })
        .await?
    }
}

pub fn router(state: &shared::State) -> OpenApiRouter<shared::State> {
    OpenApiRouter::new()
        .routes(routes!(status::route))
        .routes(routes!(stats::route))
        .routes(routes!(connect::route))
        .routes(routes!(unlink::route))
        .routes(routes!(backups::route))
        .routes(routes!(pushes::route))
        .routes(routes!(files::route))
        .routes(routes!(delete_file::route))
        .routes(routes!(push::route))
        .routes(routes!(restore::route))
        .routes(routes!(cancel::route))
        .with_state(state.clone())
}
