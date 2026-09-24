//! Extension routes: the OAuth callback (unauthenticated) plus the user-facing API.

pub mod admin;
pub mod auth;
pub mod client;

use axum::http::StatusCode;
use compact_str::CompactString;
use shared::response::ApiResponse;

use crate::{models::GDriveConnection, settings::GDriveSettingsData};

/// Whether database backups are allowed to be pushed and listed at all - the
/// operator's switch, read fresh per call rather than cached, so flipping it on the
/// Configure page takes effect on the very next request.
///
/// Defaults to *off* when settings can't be read: refusing is the safe failure mode
/// (nothing gets uploaded by accident), and the switch exists precisely so including
/// them is a choice.
pub async fn push_database_backups(state: &shared::State) -> bool {
    state
        .settings
        .get()
        .await
        .ok()
        .map(|settings| {
            settings
                .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
                .map(|ext| ext.push_database_backups)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// The caller's linked Drive account, or a 412 telling them to link one.
///
/// Every route that talks to Google starts here, and the failure has to read as "you
/// haven't linked an account" rather than as the 500 a `None` unwrapped inside the token
/// helper would turn into.
pub async fn require_connection(
    state: &shared::State,
    user_uuid: uuid::Uuid,
) -> Result<GDriveConnection, ApiResponse> {
    GDriveConnection::by_user_uuid(&state.database, user_uuid)
        .await
        .map_err(ApiResponse::from)?
        .ok_or_else(|| {
            ApiResponse::error("no google drive account linked")
                .with_status(StatusCode::PRECONDITION_FAILED)
        })
}

/// Cache key namespace for in-flight Drive connections.
///
/// The value is the user's UUID: the callback lands on an unauthenticated router, so
/// this is how it knows *whose* Drive is being linked. Same shape the Panel uses for
/// its own `oauth_state::<provider>::<state>` keys.
pub fn state_cache_key(state: &str) -> CompactString {
    compact_str::format_compact!("dev.caloptreyx.gdrive::state::{state}")
}

/// Cookie carrying the CSRF state across the Google redirect. Scoped to the callback
/// path so it doesn't leak onto unrelated requests.
pub const STATE_COOKIE: &str = "dev_caloptreyx_gdrive_state";

/// Path the cookie is scoped to. Must match where the callback route is mounted.
pub const CALLBACK_PATH: &str = "/api/auth/gdrive";

/// Where Google sends the browser back to.
pub fn redirect_uri(app_url: &str) -> String {
    format!(
        "{}{}/callback",
        app_url.trim_end_matches('/'),
        CALLBACK_PATH
    )
}

/// The URL Wings is told to fetch during a restore.
///
/// Two properties are load-bearing and neither is obvious:
///
/// * It lives under the *auth* router (`/api/auth/gdrive/...`), because Wings has no
///   session - authentication is the single-use token in the path, claimed by
///   `archive` on first fetch.
/// * It ends in `/backup.tar.gz`: Wings' S3 adapter parses the archive format from the
///   **last path segment** of the download URL and aborts the restore if the suffix
///   isn't one it knows. A URL without it fails before a single byte is read.
pub fn restore_archive_url(app_url: &str, token: uuid::Uuid) -> String {
    format!(
        "{}{CALLBACK_PATH}/archive/{}/backup.tar.gz",
        app_url.trim_end_matches('/'),
        token
    )
}

#[cfg(test)]
mod tests {
    use super::restore_archive_url;

    #[test]
    fn the_archive_url_sits_under_the_callback_path_and_ends_in_tar_gz() {
        let token = uuid::Uuid::nil();
        let url = restore_archive_url("https://panel.example.com", token);

        assert_eq!(
            url,
            format!("https://panel.example.com/api/auth/gdrive/archive/{token}/backup.tar.gz")
        );

        // A trailing slash in the configured app URL must not produce `//api/...`.
        assert_eq!(
            restore_archive_url("https://panel.example.com/", token),
            url
        );

        // Wings reads the archive format off the final path segment, so this suffix is
        // the whole reason the restore is accepted at all.
        assert!(url.ends_with("/backup.tar.gz"));
    }
}
