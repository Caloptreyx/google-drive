//! The Google OAuth callback.
//!
//! Mounted on `add_auth_api_router`, so there is no session middleware here - the route
//! identifies the connecting user purely from the CSRF state cached when they clicked
//! Connect, cross-checked against an httpOnly cookie.

use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use axum::http::StatusCode;
    use shared::{
        GetState,
        response::{ApiResponse, ApiResponseResult},
    };
    use tower_cookies::{Cookie, Cookies, cookie::SameSite};

    use super::super::{CALLBACK_PATH, STATE_COOKIE, redirect_uri, state_cache_key};
    use crate::{google, settings::GDriveSettingsData};

    #[utoipa::path(get, path = "/callback", responses(
        (status = SEE_OTHER, body = String),
        (status = BAD_REQUEST, body = String),
        (status = NOT_FOUND, body = String),
    ))]
    pub async fn route(
        state: GetState,
        cookies: Cookies,
        axum::extract::Query(params): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> ApiResponseResult {
        let app_url = app_url(&state).await;

        // Read the cached user id *before* invalidating: invalidate is what makes the
        // state single-use, so it has to happen after we've claimed the value.
        let Some(returned_state) = params.get("state").cloned() else {
            return redirect(&app_url, "/account/google-drive?error=missing_state");
        };
        let cache_key = state_cache_key(&returned_state);

        let cached_user: Option<uuid::Uuid> = match state.cache.get::<uuid::Uuid>(&cache_key).await
        {
            Ok(value) => value,
            Err(err) => {
                tracing::error!("gdrive state cache read failed: {err:#}");
                return redirect(&app_url, "/account/google-drive?error=state_lookup");
            }
        };

        // Consume the cookie so it cannot be replayed, then compare it against the
        // `state` Google echoed back. Either mismatch means this callback is forged.
        let cookie_state = cookies.get(STATE_COOKIE).map(|c| c.value().to_owned());
        cookies.remove(
            Cookie::build((STATE_COOKIE, ""))
                .path(CALLBACK_PATH)
                .same_site(SameSite::Lax)
                .http_only(true)
                .build(),
        );

        if cookie_state.as_deref() != Some(returned_state.as_str()) || cached_user.is_none() {
            return redirect(&app_url, "/account/google-drive?error=csrf");
        }

        // Single-use: invalidate immediately so a replayed callback finds nothing.
        if let Err(err) = state.cache.invalidate(&cache_key).await {
            tracing::error!("gdrive state cache invalidate failed: {err:#}");
            return redirect(&app_url, "/account/google-drive?error=state_lookup");
        }

        let user_uuid = match cached_user {
            Some(user_uuid) => user_uuid,
            None => return redirect(&app_url, "/account/google-drive?error=unknown_user"),
        };

        // Google sends `error` instead of `code` when the user denies consent.
        if let Some(err) = params.get("error") {
            tracing::info!("gdrive oauth denied by user: {err}");
            return redirect(&app_url, "/account/google-drive?error=denied");
        }

        let Some(code) = params.get("code").cloned() else {
            return redirect(&app_url, "/account/google-drive?error=missing_code");
        };

        let (client_id, client_secret, shared_folder_id) = {
            let settings = state.settings.get().await?;
            match settings.get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive") {
                Ok(ext) if ext.is_configured() => (
                    ext.client_id.to_string(),
                    ext.client_secret.to_string(),
                    ext.shared_folder_id.clone(),
                ),
                _ => return redirect(&app_url, "/account/google-drive?error=not_configured"),
            }
        };

        let tokens = match google::token_request(
            &state.client,
            &client_id,
            &client_secret,
            &redirect_uri(&app_url),
            code,
        )
        .await
        {
            Ok(tokens) => tokens,
            Err(err) => {
                tracing::error!("gdrive token exchange failed: {err:#}");
                return redirect(&app_url, "/account/google-drive?error=token_exchange");
            }
        };

        let existing = crate::models::GDriveConnection::by_user_uuid(&state.database, user_uuid)
            .await
            .ok()
            .flatten();

        // Google only returns a refresh token on first consent for a given scope set, so
        // a re-link can come back without one. Keep the stored token in that case rather
        // than overwriting it with nothing and breaking every later refresh.
        let previous_refresh_token = match existing.as_ref() {
            Some(connection) => state
                .database
                .decrypt_base64(&connection.refresh_token)
                .await
                .ok()
                .map(|token| token.to_string()),
            None => None,
        };
        let refresh_token = tokens
            .refresh_token
            .as_ref()
            .map(|token| token.to_string())
            .or(previous_refresh_token);

        let Some(refresh_token) = refresh_token else {
            return redirect(&app_url, "/account/google-drive?error=no_refresh_token");
        };

        let userinfo = match google::userinfo(&state.client, &tokens.access_token).await {
            Ok(userinfo) => userinfo,
            Err(err) => {
                tracing::error!("gdrive userinfo failed: {err:#}");
                return redirect(&app_url, "/account/google-drive?error=userinfo");
            }
        };

        let account_email = userinfo
            .email
            .map(|email| email.to_string())
            .unwrap_or_else(|| "unknown@gmail.com".to_string());

        let folder_name = folder_name(&state).await;

        // Reuse the folder across re-links so the same account doesn't scatter backups
        // across a fresh folder every time someone reconnects.
        let folder_id = match existing {
            Some(existing) => existing.folder_id.to_string(),
            None => {
                // Where the backup root is born: the configured shared-folder id when
                // set - the pasted id of a folder inside a shared drive, which
                // `supportsAllDrives` on create_folder then handles - otherwise the
                // account's own My Drive. This applies from this connection onward; a
                // connection that already exists keeps pointing where it points (see
                // `shared_folder_id` in settings), so unlinking and reconnecting is
                // what moves an existing account under a shared drive.
                let parent = shared_folder_id.trim();
                let parent = if parent.is_empty() {
                    None
                } else {
                    Some(parent)
                };

                match google::create_folder(
                    &state.client,
                    &tokens.access_token,
                    &folder_name,
                    parent,
                )
                .await
                {
                    Ok(id) => id.to_string(),
                    Err(err) => {
                        tracing::error!("gdrive folder create failed: {err:#}");
                        return redirect(&app_url, "/account/google-drive?error=folder_create");
                    }
                }
            }
        };

        let scope = tokens
            .scope
            .as_ref()
            .map(|scope| scope.to_string())
            .unwrap_or_else(|| google::SCOPES.to_string());

        let encrypted_refresh_token = match state.database.encrypt_base64(refresh_token).await {
            Ok(value) => value,
            Err(err) => {
                tracing::error!("gdrive token encrypt failed: {err:#}");
                return redirect(&app_url, "/account/google-drive?error=encrypt");
            }
        };

        if let Err(err) = crate::models::GDriveConnection::upsert(
            &state.database,
            user_uuid,
            &account_email,
            &folder_id,
            &folder_name,
            &encrypted_refresh_token,
            &scope,
        )
        .await
        {
            tracing::error!("gdrive connection save failed: {err}");
            return redirect(&app_url, "/account/google-drive?error=save");
        }

        redirect(&app_url, "/account/google-drive?connected=1")
    }

    async fn app_url(state: &shared::State) -> String {
        state
            .settings
            .get()
            .await
            .map(|settings| settings.app.url.trim_end_matches('/').to_string())
            .unwrap_or_default()
    }

    async fn folder_name(state: &shared::State) -> String {
        state
            .settings
            .get()
            .await
            .ok()
            .and_then(|settings| {
                settings
                    .get_extension_settings::<GDriveSettingsData>("dev.caloptreyx.gdrive")
                    .ok()
                    .map(|settings| settings.folder_name.to_string())
            })
            .unwrap_or_else(|| "Calagopus Backups".to_string())
    }

    fn redirect(app_url: &str, path: &str) -> ApiResponseResult {
        ApiResponse::new(axum::body::Body::empty())
            .with_header("Location", format!("{app_url}{path}"))
            .with_status(StatusCode::SEE_OTHER)
            .ok()
    }
}

/// The archive stream Wings pulls while restoring - the only extension route with no
/// session in sight, by necessity: Wings is not a panel user.
///
/// Authentication is entirely the single-use token in the path. It was minted under a
/// session by `POST /restore`, and [`GDriveRestoreToken::consume`] deletes the row as
/// part of answering - so a replay, a guess, or a token that outlived its fifteen
/// minutes all land on a 410 here and never reach Google. The bytes then stream
/// panel-side from Drive using the linking user's stored grant, which is why the files
/// can stay private: no link-sharing, no public URL, no interstitial.
mod archive {
    use axum::http::StatusCode;
    use shared::{
        GetState,
        response::{ApiResponse, ApiResponseResult},
    };

    use crate::{
        google,
        models::{GDriveConnection, GDriveRestoreToken},
        tokens,
    };

    #[utoipa::path(get, path = "/archive/{token}/backup.tar.gz", responses(
        (status = OK, body = String),
        (status = GONE, body = shared::ApiError),
        (status = PRECONDITION_FAILED, body = shared::ApiError),
        (status = BAD_GATEWAY, body = shared::ApiError),
    ), params(
        (
            "token" = uuid::Uuid, Path,
            description = "Single-use restore token minted by the restore route",
        ),
    ))]
    pub async fn route(
        state: GetState,
        axum::extract::Path(token): axum::extract::Path<uuid::Uuid>,
    ) -> ApiResponseResult {
        // Consume *before* anything else: whatever happens next, this token is spent.
        // Burning it even on a failure is deliberate - the restore that minted it is
        // already failing at that point, and a live token nothing will use is pure
        // liability. `DELETE ... RETURNING` also makes this the single-use gate under
        // concurrent fetches: exactly one caller wins the row.
        let Some(minted) = GDriveRestoreToken::consume(&state.database, token)
            .await
            .map_err(ApiResponse::from)?
        else {
            return ApiResponse::error("this restore link has expired or was already used")
                .with_status(StatusCode::GONE)
                .ok();
        };

        let connection = GDriveConnection::by_user_uuid(&state.database, minted.user_uuid)
            .await
            .map_err(ApiResponse::from)?
            .ok_or_else(|| {
                ApiResponse::error("the Google Drive account is no longer linked")
                    .with_status(StatusCode::GONE)
            })?;

        let client = tokens::client()?;

        // Surfaced *and* logged: Wings' S3-restore fetch never checks the status code,
        // so an error body here becomes a confusing extraction failure on the node -
        // the panel-side log is where this failure is actually diagnosable.
        let access_token = match tokens::access_token(&client, &state, &connection).await {
            Ok(access_token) => access_token,
            Err(err) => {
                tracing::error!("gdrive archive fetch could not mint an access token: {err:#}");
                return ApiResponse::error(format!(
                    "the stored Google grant could not be used: {err:#}"
                ))
                .with_status(StatusCode::PRECONDITION_FAILED)
                .ok();
            }
        };

        let response = match google::download_file(&client, &access_token, &minted.file_id).await {
            Ok(response) => response,
            Err(err) => {
                tracing::error!(
                    "gdrive archive fetch failed for backup {}: {err:#}",
                    minted.backup_uuid
                );
                return ApiResponse::error(format!(
                    "reading the archive from drive failed: {err:#}"
                ))
                .with_status(StatusCode::BAD_GATEWAY)
                .ok();
            }
        };

        let content_length = response
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        tracing::info!(
            "gdrive archive streamed for backup {} (file {})",
            minted.backup_uuid,
            minted.file_id
        );

        ApiResponse::new(axum::body::Body::from_stream(response.bytes_stream()))
            .with_header("Content-Type", "application/gzip")
            .with_header(
                "Content-Disposition",
                "attachment; filename=\"backup.tar.gz\"",
            )
            .with_optional_header("Content-Length", content_length)
            .ok()
    }
}

pub fn router(state: &shared::State) -> OpenApiRouter<shared::State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .routes(routes!(archive::route))
        .with_state(state.clone())
}
