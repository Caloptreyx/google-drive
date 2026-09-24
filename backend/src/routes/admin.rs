//! Operator-facing configuration, behind the admin session middleware.
//!
//! There is no generic "extension settings" endpoint in core - each extension ships its
//! own GET/PUT for its own struct (see the Settings docs), which is why this exists.

use utoipa_axum::{router::OpenApiRouter, routes};

/// Namespaced by the package name so it can't collide with core or another extension.
///
/// Deliberately *not* `/extensions/dev.caloptreyx.gdrive`, which is what the routing docs
/// suggest: core already owns `PATCH /api/admin/extensions/{extension}` (the call that
/// toggles an extension on and off), and a static segment next to that parameter wins -
/// so our own package name would answer `405` to its own toggle and the admin UI could
/// never switch us off. See `tests/route_mount_shape.rs`. The package name is unique by
/// construction, so using it as the top-level segment keeps the collision guarantee
/// without shadowing anything.
pub const BASE: &str = "/dev.caloptreyx.gdrive";

mod get {
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::GetPermissionManager,
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::settings::GDriveSettingsData;

    #[derive(ToSchema, Serialize)]
    pub struct Response {
        client_id: compact_str::CompactString,
        /// Whether a secret is stored. The secret itself never leaves the server: the
        /// form renders an empty password field meaning "leave unchanged".
        client_secret_set: bool,
        folder_name: compact_str::CompactString,
        /// Whether finished backups are queued automatically when they complete.
        auto_push: bool,
        /// Whether the Drive copy is trashed when the backup it came from is deleted.
        delete_copy: bool,
        /// Newest copies of each server to keep in Drive; `0` keeps everything.
        keep_copies: i64,
        /// Server-name globs that opt servers *in* to auto-push; empty means all.
        push_include: compact_str::CompactString,
        /// Server-name globs that opt servers *out* of auto-push; wins over include.
        push_exclude: compact_str::CompactString,
        /// Whether database-instance backups are pushed and listed like file backups.
        push_database_backups: bool,
        /// Whether each server's backups land in a subfolder named after it.
        folder_per_server: bool,
        /// Id of a folder inside a shared drive to nest the backup folder under;
        /// empty means My Drive. Applies at (re)connect time.
        shared_folder_id: compact_str::CompactString,
    }

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(state: GetState, permissions: GetPermissionManager) -> ApiResponseResult {
        permissions.has_admin_permission("extensions.read")?;

        let settings = state.settings.get().await?;
        let ext = settings
            .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
            .map(|ext| Response {
                client_id: ext.client_id.clone(),
                client_secret_set: !ext.client_secret.is_empty(),
                folder_name: ext.folder_name.clone(),
                auto_push: ext.auto_push,
                delete_copy: ext.delete_copy,
                keep_copies: ext.keep_copies,
                push_include: ext.push_include.clone(),
                push_exclude: ext.push_exclude.clone(),
                push_database_backups: ext.push_database_backups,
                folder_per_server: ext.folder_per_server,
                shared_folder_id: ext.shared_folder_id.clone(),
            })
            .unwrap_or_default();

        Ok(ApiResponse::new_serialized(ext))
    }

    impl Default for Response {
        fn default() -> Self {
            let defaults = GDriveSettingsData::default();

            Self {
                client_id: Default::default(),
                client_secret_set: false,
                folder_name: defaults.folder_name,
                auto_push: defaults.auto_push,
                delete_copy: defaults.delete_copy,
                keep_copies: defaults.keep_copies,
                push_include: defaults.push_include,
                push_exclude: defaults.push_exclude,
                push_database_backups: defaults.push_database_backups,
                folder_per_server: defaults.folder_per_server,
                shared_folder_id: defaults.shared_folder_id,
            }
        }
    }
}

mod put {
    use garde::Validate;
    use serde::Deserialize;
    use shared::{
        GetState,
        models::{admin_activity::GetAdminActivityLogger, user::GetPermissionManager},
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::settings::GDriveSettingsData;

    #[derive(ToSchema, Validate, Deserialize)]
    pub struct Payload {
        /// The OAuth client ID from the Google Cloud console.
        #[garde(length(chars, min = 1, max = 512))]
        #[schema(min_length = 1, max_length = 512)]
        client_id: compact_str::CompactString,

        /// The OAuth client secret. Send an empty string to keep whatever is stored -
        /// that's what the form does when the admin doesn't retype it.
        #[garde(length(chars, max = 512))]
        #[schema(max_length = 512)]
        client_secret: compact_str::CompactString,

        /// Folder name backups are collected in inside Drive.
        #[garde(length(chars, min = 1, max = 255))]
        #[schema(min_length = 1, max_length = 255)]
        folder_name: compact_str::CompactString,

        /// Queue finished backups automatically when they complete.
        #[garde(skip)]
        auto_push: bool,

        /// Trash the Drive copy when the backup it came from is deleted.
        #[garde(skip)]
        delete_copy: bool,

        /// Newest copies of each server to keep in Drive; 0 keeps everything.
        #[garde(range(min = 0))]
        #[schema(minimum = 0)]
        keep_copies: i64,

        /// Server-name globs (newline/comma separated, `*` and `?`) that opt servers
        /// in to auto-push. Empty means every server.
        #[garde(length(chars, max = 4000))]
        #[schema(max_length = 4000)]
        push_include: compact_str::CompactString,

        /// Server-name globs that opt servers out; wins over include. Manual pushes
        /// never consult either list.
        #[garde(length(chars, max = 4000))]
        #[schema(max_length = 4000)]
        push_exclude: compact_str::CompactString,

        /// Push database-instance backups alongside file backups.
        #[garde(skip)]
        push_database_backups: bool,

        /// Give each server a subfolder of its own instead of one flat folder.
        #[garde(skip)]
        folder_per_server: bool,

        /// Id of a folder inside a shared drive to nest the backup folder under.
        #[garde(length(chars, max = 255))]
        #[schema(max_length = 255)]
        shared_folder_id: compact_str::CompactString,
    }

    #[derive(ToSchema, serde::Serialize)]
    struct Response {
        configured: bool,
    }

    #[utoipa::path(put, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = BAD_REQUEST, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ), request_body = inline(Payload))]
    pub async fn route(
        state: GetState,
        permissions: GetPermissionManager,
        activity_logger: GetAdminActivityLogger,
        shared::Payload(data): shared::Payload<Payload>,
    ) -> ApiResponseResult {
        if let Err(errors) = shared::utils::validate_data(&data) {
            return ApiResponse::new_serialized(shared::ApiError::new_strings_value(errors))
                .with_status(axum::http::StatusCode::BAD_REQUEST)
                .ok();
        }

        permissions.has_admin_permission("extensions.manage")?;

        // Captured before the payload moves into settings below, for the activity log.
        let shared_folder_set = !data.shared_folder_id.trim().is_empty();

        let configured = {
            let mut settings = state.settings.get_mut().await?;
            let ext = settings.find_mut_extension_settings::<GDriveSettingsData>()?;

            ext.client_id = data.client_id;
            ext.folder_name = data.folder_name;
            ext.auto_push = data.auto_push;
            ext.delete_copy = data.delete_copy;
            ext.keep_copies = data.keep_copies;
            ext.push_include = data.push_include;
            ext.push_exclude = data.push_exclude;
            ext.push_database_backups = data.push_database_backups;
            ext.folder_per_server = data.folder_per_server;
            ext.shared_folder_id = data.shared_folder_id;
            // An empty secret means "unchanged", not "cleared" - there's no way to
            // un-configure from the form, only to fill the secret in.
            if !data.client_secret.is_empty() {
                ext.client_secret = data.client_secret;
            }

            settings.save().await?;

            state
                .settings
                .get()
                .await?
                .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
                .map(|ext| ext.is_configured())
                .unwrap_or(false)
        };

        activity_logger
            .log(
                "extension:settings",
                serde_json::json!({
                    "package_name": "dev.caloptreyx.gdrive",
                    "configured": configured,
                    "auto_push": data.auto_push,
                    "delete_copy": data.delete_copy,
                    "keep_copies": data.keep_copies,
                    "push_database_backups": data.push_database_backups,
                    "folder_per_server": data.folder_per_server,
                    "folder_set": shared_folder_set,
                }),
            )
            .await;

        ApiResponse::new_serialized(Response { configured }).ok()
    }
}

mod post_test {
    use axum::http::StatusCode;
    use serde::Serialize;
    use shared::{
        GetState,
        models::user::GetPermissionManager,
        response::{ApiResponse, ApiResponseResult},
    };
    use utoipa::ToSchema;

    use crate::{google, routes::redirect_uri, settings::GDriveSettingsData};

    #[derive(ToSchema, Serialize)]
    struct Response {
        /// Whether Google accepted the client id and secret. A `false` here with a
        /// `detail` naming the redirect URI is the whole diagnosis of the most common
        /// setup error - a mismatch between this panel's URL and the one registered on
        /// the OAuth client in the Google Cloud console.
        valid: bool,
        /// What Google's answer means, in plain words.
        detail: compact_str::CompactString,
        /// The exact redirect URI this panel sends, for pasting into the console.
        redirect_uri: compact_str::CompactString,
    }

    /// Exchange a throwaway authorization code with the stored credentials and report
    /// what Google's refusal says about them.
    ///
    /// No user account is involved: the code is a constant that can never be valid, so
    /// consent is never touched and no tokens come back. What differs between a broken
    /// and a working setup is *which* error Google returns, and `classify_probe` maps
    /// that vocabulary onto this response. Answering `valid: true` therefore means
    /// "these credentials and this redirect URI would work for a real link", nothing
    /// more.
    #[utoipa::path(post, path = "/test", responses(
        (status = OK, body = inline(Response)),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = FORBIDDEN, body = shared::ApiError),
    ))]
    pub async fn route(state: GetState, permissions: GetPermissionManager) -> ApiResponseResult {
        permissions.has_admin_permission("extensions.manage")?;

        let (client_id, client_secret, app_url) = {
            let settings = state.settings.get().await?;

            let ext = match settings
                .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
            {
                Ok(ext) if ext.is_configured() => ext,
                _ => {
                    return ApiResponse::error("google drive is not configured")
                        .with_status(StatusCode::PRECONDITION_FAILED)
                        .ok();
                }
            };

            (
                ext.client_id.to_string(),
                ext.client_secret.to_string(),
                settings.app.url.trim_end_matches('/').to_string(),
            )
        };

        let redirect_uri = redirect_uri(&app_url);

        let (probe, body) =
            google::probe_credentials(&state.client, &client_id, &client_secret, &redirect_uri)
                .await
                .map_err(|err| {
                    ApiResponse::error(format!("could not reach google: {err}"))
                        .with_status(StatusCode::PRECONDITION_FAILED)
                })?;

        let detail = match probe {
            google::CredentialProbe::Valid => {
                "Google accepted these credentials - the client id, secret and redirect URI \
                 would all work for a real link."
                    .to_string()
            }
            google::CredentialProbe::RedirectMismatch => format!(
                "Google rejected the redirect URI. This panel sends `{redirect_uri}` - that \
                 exact string must be listed under \"Authorised redirect URIs\" on the OAuth \
                 client in the Google Cloud console."
            ),
            google::CredentialProbe::BadClient => {
                "Google rejected the client id or secret. Check both against the OAuth \
                 client in the Google Cloud console (secrets can be rotated - an old one \
                 must be replaced here)."
                    .to_string()
            }
            google::CredentialProbe::Other => {
                format!("Google answered: {}", truncate(&body))
            }
        };

        let valid = probe.is_valid();

        tracing::info!(valid, "gdrive credential probe: {detail}");

        ApiResponse::new_serialized(Response {
            valid,
            detail: detail.into(),
            redirect_uri: redirect_uri.into(),
        })
        .ok()
    }

    /// Google's error bodies carry `error_description` and a JSON payload nobody should
    /// have to read raw in a badge - but the raw body is still the only full diagnosis
    /// when it is not one of the recognised shapes, so keep a readable slice of it.
    fn truncate(body: &str) -> String {
        const MAX: usize = 300;

        let body = body.trim();
        if body.len() <= MAX {
            return body.to_string();
        }

        let mut end = MAX;
        while !body.is_char_boundary(end) {
            end -= 1;
        }

        format!("{}…", &body[..end])
    }
}

pub fn router(state: &shared::State) -> OpenApiRouter<shared::State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(put::route))
        .routes(routes!(post_test::route))
        .with_state(state.clone())
}
