//! Turning a linked account into a usable Google access token.
//!
//! The push worker and the Drive browser both need the same three steps - read the
//! stored refresh token, mint an access token from it, and write the refresh token back
//! if Google rotated it. Rotation is the step that quietly breaks every later call when
//! it's forgotten, so it lives in one place rather than being copied per caller.

use anyhow::{Context, anyhow, bail};
use shared::State;

use crate::{IDENTIFIER, google, models::GDriveConnection, settings::GDriveSettingsData};

/// HTTP client for talking to Google.
///
/// Deliberately not `state.client`: that one carries a 60 second read timeout, which is
/// right for panel API calls and wrong for an upload that runs for minutes.
pub fn client() -> Result<reqwest::Client, anyhow::Error> {
    Ok(reqwest::ClientBuilder::new()
        .user_agent("calagopus-gdrive-extension")
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?)
}

/// The operator's OAuth credentials, or an error when Google Drive isn't set up.
pub async fn credentials(state: &State) -> Result<(String, String), anyhow::Error> {
    let settings = state.settings.get().await?;
    let ext = settings
        .get_extension_settings::<GDriveSettingsData>(IDENTIFIER)
        .map_err(|_| anyhow!("google drive is not configured"))?;

    if !ext.is_configured() {
        bail!("google drive is not configured");
    }

    Ok((ext.client_id.to_string(), ext.client_secret.to_string()))
}

/// Mint a current access token for an already-loaded connection.
///
/// The connection is passed in rather than fetched here because callers have usually
/// just fetched it to check the user is linked at all, and that check is what decides
/// whether an unlinked account is a 412 or an internal error.
///
/// This is also where a dead grant is *detected*: Google answers a revoked or expired
/// refresh token with `invalid_grant` in the error body, `google::post_form` preserves
/// that marker in its error text, and the connection gets flagged so the worker stops
/// claiming its pushes and the account page can tell the user to reconnect. A later
/// successful refresh takes the flag back down, so a transient rejection doesn't wedge
/// the account behind a banner nothing can clear.
pub async fn access_token(
    client: &reqwest::Client,
    state: &State,
    connection: &GDriveConnection,
) -> Result<String, anyhow::Error> {
    let (client_id, client_secret) = credentials(state).await?;

    let stored_refresh_token = state
        .database
        .decrypt_base64(&connection.refresh_token)
        .await
        .context("decrypting the stored refresh token")?
        .to_string();

    let tokens = match google::refresh_access_token(
        client,
        &client_id,
        &client_secret,
        &stored_refresh_token,
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(err) => {
            if format!("{err:#}").contains(google::INVALID_GRANT) {
                if let Err(flag_err) =
                    GDriveConnection::mark_needs_reauth(&state.database, connection.user_uuid).await
                {
                    tracing::error!("gdrive failed to flag connection for reconnect: {flag_err}");
                } else {
                    tracing::warn!(
                        user = %connection.user_uuid,
                        "google refused the stored refresh token; pushes paused until the user reconnects: {err:#}"
                    );
                }
            }

            return Err(err);
        }
    };

    // The grant worked, so whatever flag was up (an earlier rejection Google later
    // walked back) comes back down - but only when there was one, to avoid a write on
    // every single refresh.
    if connection.needs_reauth_at.is_some()
        && let Err(clear_err) =
            GDriveConnection::clear_needs_reauth(&state.database, connection.user_uuid).await
    {
        tracing::error!("gdrive failed to clear the reconnect flag: {clear_err}");
    }

    // Google may rotate the refresh token on any grant; keeping the old one would
    // silently break every later call that used it.
    if let Some(rotated) = tokens.refresh_token.as_ref() {
        let encrypted = state
            .database
            .encrypt_base64(rotated.to_string())
            .await
            .context("encrypting the rotated refresh token")?;
        GDriveConnection::update_refresh_token(&state.database, connection.user_uuid, &encrypted)
            .await?;
    }

    Ok(tokens.access_token.to_string())
}
