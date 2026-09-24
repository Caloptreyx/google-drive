//! The background worker that pushes queued backups into the user's Google Drive.
//!
//! Three properties shape this design:
//!
//! * Extensions compile into the Panel, not Wings, so the archive is fetched with the
//!   panel's own `download_url()` rather than being uploaded from the node. That URL is
//!   browser-facing, so it is rebased onto the node's own address first - see
//!   [`rebase_onto_node`].
//! * Background tasks get no shutdown hook, so the database is the source of truth for
//!   in-flight work - the staged file path is derived from the row, and Google's
//!   resumable session URI is persisted, so a restart continues instead of restarting.
//! * reqwest sends streaming bodies with chunked transfer encoding, but Google requires
//!   a `Content-Length` on the data `PUT`. Every chunk is therefore a concrete buffer,
//!   which gives reqwest an exact size to advertise.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use futures_util::StreamExt;
use shared::{
    State,
    models::{ByUuid, server_backup::ServerBackup},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::{
    google::{self, UploadStatus},
    models::{GDriveConnection, GDrivePush},
    settings, tokens,
};

/// Stop consuming the queue once a push has burned this many attempts.
const MAX_ATTEMPTS: i32 = 5;
/// Pushes worked through per tick.
const BATCH: usize = 4;
/// Chunk size for the resumable `PUT`. Google requires a multiple of 256 KiB.
const CHUNK: usize = 8 * 1024 * 1024;
/// Wait between ticks when everything is healthy.
const IDLE_TICK: Duration = Duration::from_secs(5);
/// Wait between ticks after something failed, so a broken push doesn't spin.
const FAILURE_TICK: Duration = Duration::from_secs(30);
/// Attempts for an individual chunk before the whole push is failed.
const CHUNK_ATTEMPTS: u32 = 4;
/// How often one attempt may replace an invalidated session. A replacement session
/// restarts the transfer from byte zero, so an upload Google keeps throwing away is
/// failed rather than looped - each restart would re-send everything before it.
const SESSION_RESTARTS: u32 = 2;
/// Access tokens live an hour, which a large archive can outlive: how often one attempt
/// may fetch a new one after Google rejects the token mid-transfer.
const TOKEN_REFRESHES: u32 = 3;
/// Bound on the stored error text so the status list stays readable.
const MAX_ERROR_LEN: usize = 1000;

/// Registered as a background task; loops until the process goes away.
pub async fn run(state: State) -> Result<(), anyhow::Error> {
    // Always sleep before returning: `add_task` re-invokes immediately on `Err`, so a
    // bare `?` here would turn a failing database into a hot loop.
    let outcome = match tick(&state).await {
        Ok(true) => {
            tokio::time::sleep(FAILURE_TICK).await;
            Ok(())
        }
        Ok(false) => {
            tokio::time::sleep(IDLE_TICK).await;
            Ok(())
        }
        Err(err) => {
            tokio::time::sleep(FAILURE_TICK).await;
            Err(err)
        }
    };

    // The reconcile sweep rides this loop rather than its own task: same cadence, same
    // primary-instance guarantee, and it inherits the backoff a failing tick already
    // applies. Errors are swallowed on purpose - a broken sweep must never take the
    // push worker down with it; the next round just tries again.
    static TICKS: AtomicU64 = AtomicU64::new(0);
    if TICKS
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(SWEEP_EVERY)
        && let Err(err) = crate::reconcile::sweep(&state).await
    {
        tracing::warn!("google drive reconcile sweep failed: {err:#}");
    }

    outcome
}

/// Worker ticks between reconcile passes. At the 5 second idle tick that is about a
/// minute; under failure backoff the sweep simply waits longer, which is when nobody
/// wants extra database work anyway.
const SWEEP_EVERY: u64 = 12;

/// Claims and works a batch of pushes. Returns whether any individual push failed.
async fn tick(state: &State) -> Result<bool, anyhow::Error> {
    let mut any_failed = false;

    for _ in 0..BATCH {
        let push = match GDrivePush::claim_next(&state.database, MAX_ATTEMPTS).await? {
            Some(push) => push,
            None => break,
        };

        match push_backup(state, &push).await {
            Ok(PushOutcome {
                file_id,
                bytes,
                server_uuid,
            }) => {
                if !GDrivePush::mark_completed(
                    &state.database,
                    push.backup_uuid,
                    push.user_uuid,
                    &file_id,
                    bytes,
                )
                .await?
                {
                    // The row is gone, which means `DELETE /push/…` landed while the
                    // final chunk was in flight and Google committed anyway. Nothing
                    // owns the file now, so discard it - otherwise "Cancel" would leave
                    // a copy behind that no record anywhere knows about.
                    //
                    // Best effort: if the trash itself fails the file stays, and the
                    // Files table is where it can still be found and removed. Losing a
                    // clean-up is better than losing the ability to see the file at all.
                    if let Err(err) = trash_file(state, &push, &file_id).await {
                        tracing::warn!(
                            backup = %push.backup_uuid,
                            user = %push.user_uuid,
                            file = %file_id,
                            "failed to trash the upload from a cancelled push: {err:#}"
                        );
                    }

                    tracing::info!(
                        backup = %push.backup_uuid,
                        user = %push.user_uuid,
                        "push finished after being cancelled; discarded the upload"
                    );

                    remove_staged(&push).await;
                    continue;
                }

                GDriveConnection::touch(&state.database, push.user_uuid).await?;

                // Re-queueing a finished push deliberately keeps `drive_file_id` (see
                // `GDrivePush::enqueue`), so the row still points at the copy this upload
                // has just replaced. Trashing the old one here is what stops every re-push
                // from piling up a duplicate in Drive. Best effort: the fresh upload has
                // landed either way, and a leftover file is a wart rather than a loss.
                if let Some(previous) = push.drive_file_id.as_ref()
                    && previous.as_str() != file_id
                    && let Err(err) = trash_file(state, &push, previous).await
                {
                    tracing::warn!(
                        backup = %push.backup_uuid,
                        user = %push.user_uuid,
                        "failed to trash the superseded google drive file: {err:#}"
                    );
                }

                tracing::info!(
                    backup = %push.backup_uuid,
                    user = %push.user_uuid,
                    bytes,
                    "pushed backup to google drive"
                );

                remove_staged(&push).await;

                // Retention runs last, after the row says `completed`: a prune that
                // failed must not turn a finished upload into a failed one, and a push
                // that was cancelled out from under us already had its file trashed
                // above. Best effort, logged inside.
                if let Err(err) = enforce_retention(state, &push, server_uuid, &file_id).await {
                    tracing::warn!(
                        backup = %push.backup_uuid,
                        user = %push.user_uuid,
                        "google drive retention prune failed: {err:#}"
                    );
                }
            }
            Err(err) => {
                // The row being gone means someone cancelled the push while it ran.
                // Nothing to mark, no reason to back off, and the staged archive would
                // otherwise sit in temp forever waiting on a queue entry that no longer
                // exists.
                if !GDrivePush::exists(&state.database, push.backup_uuid, push.user_uuid).await? {
                    tracing::info!(
                        backup = %push.backup_uuid,
                        user = %push.user_uuid,
                        "google drive push cancelled while running"
                    );

                    remove_staged(&push).await;
                    continue;
                }

                any_failed = true;

                let message = truncate(&format!("{err:#}"));
                tracing::error!(
                    backup = %push.backup_uuid,
                    user = %push.user_uuid,
                    attempts = push.attempts,
                    "google drive push failed: {message}"
                );

                GDrivePush::mark_failed(
                    &state.database,
                    push.backup_uuid,
                    push.user_uuid,
                    &message,
                )
                .await?;

                // Out of attempts: drop the staged archive so it doesn't sit in temp
                // forever waiting on a push that will never run again.
                if push.attempts >= MAX_ATTEMPTS {
                    remove_staged(&push).await;
                }
            }
        }
    }

    Ok(any_failed)
}

/// What one finished push produced: the Drive file id, its size, and which server's
/// retention bound it now counts against.
struct PushOutcome {
    file_id: String,
    bytes: i64,
    server_uuid: uuid::Uuid,
}

async fn push_backup(state: &State, push: &GDrivePush) -> Result<PushOutcome, anyhow::Error> {
    let connection = GDriveConnection::by_user_uuid(&state.database, push.user_uuid)
        .await?
        .ok_or_else(|| anyhow!("the Google Drive account is no longer linked"))?;

    let client = tokens::client()?;
    // Refreshed in place below: an upload can outlive the hour an access token is good
    // for, and a 401 on a chunk means the *token* is stale, not that the session died.
    let mut access_token = tokens::access_token(&client, state, &connection).await?;

    let backup = ServerBackup::by_uuid(&state.database, push.backup_uuid).await?;
    if backup.deleted.is_some() {
        bail!("the backup has been deleted");
    }
    if backup.deleting.is_some() {
        bail!("the backup is being deleted");
    }
    if backup.completed.is_none() {
        bail!("the backup has not finished yet");
    }
    if !backup.successful {
        bail!("the backup did not complete successfully");
    }

    let server = backup
        .server
        .as_ref()
        .ok_or_else(|| anyhow!("the backup is no longer attached to a server"))?
        .fetch_cached(&state.database)
        .await?;
    let node = backup.node.fetch_cached(&state.database).await?;

    let source_url = backup
        .download_url(
            state,
            &server.owner,
            &node,
            wings_api::StreamableArchiveFormat::TarGz,
        )
        .await?;
    let source_url = rebase_onto_node(&source_url, &node);

    let (final_path, part_path) = staging_paths(push);
    // Checked once more before the expensive part: between the claim and here the row may
    // already have been cancelled, and there is no point downloading the archive for it.
    if !GDrivePush::exists(&state.database, push.backup_uuid, push.user_uuid).await? {
        bail!("the upload was cancelled");
    }
    let total = stage(&source_url, &final_path, &part_path)
        .await
        .context("staging the backup archive for upload")?;

    let file_name = drive_file_name(&server.name, &backup.name);

    // Flat folder or a subfolder named after this server, per the operator's switch.
    // Resolved after staging on purpose: it is one cheap `files.list` (or a create),
    // and doing it first would open Drive sessions for uploads that can't be staged.
    let destination_folder =
        resolve_destination_folder(state, &client, &access_token, &connection, &server.name)
            .await
            .context("resolving this server's folder in google drive")?;

    // A session was opened against one particular total. If the staged archive no longer
    // matches it - temp cleaned out, or re-downloaded at a different size - that session
    // describes bytes Google will never see, so drop it and start over.
    let mut carried_session = push.session_uri.clone();
    if carried_session.is_some() && push.total_bytes != total as i64 {
        GDrivePush::clear_session(&state.database, push.backup_uuid, push.user_uuid).await?;
        carried_session = None;
    }

    // Resume if a session survived the last attempt, otherwise open a new one.
    let (mut session_uri, mut offset) = match carried_session {
        Some(uri) => {
            match google::query_upload_status(&client, &access_token, &uri, total).await? {
                UploadStatus::Complete(file) => {
                    remove_staged(push).await;
                    return Ok(PushOutcome {
                        file_id: file.id.to_string(),
                        bytes: total as i64,
                        server_uuid: server.uuid,
                    });
                }
                UploadStatus::Expired => {
                    GDrivePush::clear_session(&state.database, push.backup_uuid, push.user_uuid)
                        .await?;
                    (
                        open_session(
                            state,
                            &client,
                            &access_token,
                            push,
                            &file_name,
                            &destination_folder,
                            total,
                            server.uuid,
                        )
                        .await?,
                        0,
                    )
                }
                UploadStatus::Committed(n) if n >= total => {
                    // Contradictory: Google claims everything but never confirmed completion.
                    // Discard the session and send it again rather than trust the number.
                    GDrivePush::clear_session(&state.database, push.backup_uuid, push.user_uuid)
                        .await?;
                    (
                        open_session(
                            state,
                            &client,
                            &access_token,
                            push,
                            &file_name,
                            &destination_folder,
                            total,
                            server.uuid,
                        )
                        .await?,
                        0,
                    )
                }
                UploadStatus::Committed(n) => {
                    GDrivePush::set_progress(
                        &state.database,
                        push.backup_uuid,
                        push.user_uuid,
                        n as i64,
                    )
                    .await?;
                    (uri.to_string(), n)
                }
            }
        }
        None => (
            open_session(
                state,
                &client,
                &access_token,
                push,
                &file_name,
                &destination_folder,
                total,
                server.uuid,
            )
            .await?,
            0,
        ),
    };

    let mut file = tokio::fs::File::open(&final_path)
        .await
        .context("opening the staged archive")?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let mut stalls = 0u32;
    let mut restarts = 0u32;
    let mut token_refreshes = 0u32;
    while offset < total {
        // Re-read before every chunk, not once at the start: this loop is where a push
        // actually spends its time, so it is the only place a cancel can land in time.
        // The row disappearing means `DELETE /push/<uuid>` was called - the resumable
        // session is abandoned, and Google assembles nothing from the bytes already sent.
        if !GDrivePush::exists(&state.database, push.backup_uuid, push.user_uuid).await? {
            bail!("the upload was cancelled");
        }

        let length = CHUNK.min((total - offset) as usize);
        let mut buffer = vec![0u8; length];
        file.read_exact(&mut buffer)
            .await
            .context("reading the staged archive")?;

        match put_chunk(&client, &session_uri, &access_token, &buffer, offset, total).await? {
            Chunk::Complete(file) => {
                // Not marked here: `push_backup` uploads and reports, `tick` records.
                // One place owns writing the outcome, so the cancelled-row case is
                // handled once rather than at two call sites that can disagree.
                return Ok(PushOutcome {
                    file_id: file.id.to_string(),
                    bytes: total as i64,
                    server_uuid: server.uuid,
                });
            }
            Chunk::Unauthorized => {
                // The session is untouched by a 401 - only the token is stale - so take
                // a new one and send this same chunk again rather than discard bytes
                // Google has already committed.
                if token_refreshes >= TOKEN_REFRESHES {
                    bail!(
                        "google still rejects the upload authorization after {TOKEN_REFRESHES} fresh access tokens"
                    );
                }
                token_refreshes += 1;

                tracing::warn!(
                    backup = %push.backup_uuid,
                    user = %push.user_uuid,
                    "google rejected the upload token; refreshing it ({token_refreshes}/{TOKEN_REFRESHES})"
                );

                access_token = tokens::access_token(&client, state, &connection)
                    .await
                    .context("fetching a fresh access token after google rejected the upload")?;
                continue;
            }
            Chunk::Expired { status, body } => {
                // Google threw the session away (400 malformed, 404/410 gone). The staged
                // archive is still on disk, so replace the session and send it again -
                // but bound it: a fresh session restarts at byte zero, and an upload this
                // keeps discarding would otherwise re-send forever without ever landing.
                if restarts >= SESSION_RESTARTS {
                    bail!(
                        "google invalidated the upload session {SESSION_RESTARTS} times over (last: status {status}: {body})"
                    );
                }
                restarts += 1;

                tracing::warn!(
                    backup = %push.backup_uuid,
                    user = %push.user_uuid,
                    "google invalidated the upload session (status {status}); opening a fresh one ({restarts}/{SESSION_RESTARTS}): {body}"
                );

                GDrivePush::clear_session(&state.database, push.backup_uuid, push.user_uuid)
                    .await?;
                session_uri = open_session(
                    state,
                    &client,
                    &access_token,
                    push,
                    &file_name,
                    &destination_folder,
                    total,
                    server.uuid,
                )
                .await
                .context("re-opening the resumable session after google invalidated it")?;

                // A replacement session only accepts bytes from the beginning; Google does
                // not let a new session join a transfer part-way through.
                offset = 0;
                stalls = 0;
                file.seek(std::io::SeekFrom::Start(0)).await?;
                GDrivePush::set_progress(&state.database, push.backup_uuid, push.user_uuid, 0)
                    .await?;
                continue;
            }
            Chunk::Advanced(committed) => {
                // Google confirms what it actually received, which can be short of what
                // we sent. No forward progress at all means the transfer is wedged.
                if committed <= offset {
                    stalls += 1;
                    if stalls >= CHUNK_ATTEMPTS {
                        bail!("google is not accepting upload data (no progress)");
                    }
                } else {
                    stalls = 0;
                }

                offset = committed;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
            }
        }

        GDrivePush::set_progress(
            &state.database,
            push.backup_uuid,
            push.user_uuid,
            offset as i64,
        )
        .await?;
    }

    // Every byte was sent but no completion response was seen (the last `308`, say).
    // The status query is the authoritative way to find out whether it landed.
    match google::query_upload_status(&client, &access_token, &session_uri, total).await? {
        UploadStatus::Complete(file) => Ok(PushOutcome {
            file_id: file.id.to_string(),
            bytes: total as i64,
            server_uuid: server.uuid,
        }),
        UploadStatus::Expired => {
            GDrivePush::clear_session(&state.database, push.backup_uuid, push.user_uuid).await?;
            Err(anyhow!(
                "the upload session expired before completion was confirmed"
            ))
        }
        UploadStatus::Committed(_) => Err(anyhow!(
            "google holds every byte but would not confirm the upload completed"
        )),
    }
}

#[derive(Debug)]
enum Chunk {
    Complete(google::DriveFile),
    Advanced(u64),
    /// Google rejected the bearer token. The session is unaffected by this - only the
    /// credential is stale - so the caller refreshes and repeats the same chunk.
    Unauthorized,
    /// The session itself is unusable (malformed request, gone, expired): only a new
    /// session can make progress. Carries Google's answer so failures stay diagnosable.
    Expired {
        status: u16,
        body: String,
    },
}

/// What one data `PUT` answer means for the transfer, before any retry runs.
///
/// Split out from [`put_chunk`] because this mapping is the whole decision about what a
/// failure costs: a `401` costs a token refresh, a `403` costs a pause, and only a dead
/// session costs the bytes already sent. Kept as data so it can be tested without a
/// network - the previous `400..=499 => Expired` lumped all three together and threw
/// away live sessions (and hid Google's status and message while doing it).
#[derive(Debug, PartialEq, Eq)]
enum ChunkReply {
    Complete,
    Advanced,
    Unauthorized,
    /// 403 `rateLimitExceeded` / 429: Google is asking us to slow down, and the session
    /// survives that.
    Throttled,
    /// 400 malformed request, 404 unknown session, 410 expired session.
    Expired,
    /// 5xx, and anything unrecognised: repeating the identical request may work.
    Transient,
}

fn classify_chunk(status: u16) -> ChunkReply {
    match status {
        200 | 201 => ChunkReply::Complete,
        308 => ChunkReply::Advanced,
        401 => ChunkReply::Unauthorized,
        403 | 429 => ChunkReply::Throttled,
        400..=499 => ChunkReply::Expired,
        _ => ChunkReply::Transient,
    }
}

/// The `Content-Range` value for one chunk.
///
/// `end` is the index of the **last byte** the chunk carries, because that is what the
/// header means: a chunk of `length` bytes starting at `start` covers `start..=start +
/// length - 1`. Computing it here keeps the arithmetic beside the header it feeds -
/// advertising `start + length` claims a byte the body does not contain, which Google
/// rejects with a malformed-range `400` (and which then looked like a dead session).
fn content_range(start: u64, length: usize, total: u64) -> String {
    format!("bytes {start}-{}/{total}", start + length as u64 - 1)
}

/// Send one chunk, retrying transient failures before giving up on the whole push.
async fn put_chunk(
    client: &reqwest::Client,
    session_uri: &str,
    access_token: &str,
    chunk: &[u8],
    start: u64,
    total: u64,
) -> Result<Chunk, anyhow::Error> {
    let range = content_range(start, chunk.len(), total);
    let mut attempt = 0u32;

    loop {
        attempt += 1;

        let response = client
            .put(session_uri)
            // Google checks the bearer token on the data `PUT` as well, so an upload that
            // outlives the token's hour would otherwise fail looking like a dead session.
            .bearer_auth(access_token)
            .header("Content-Range", range.as_str())
            // A concrete buffer gives reqwest an exact size, so it advertises a real
            // Content-Length instead of chunked transfer encoding.
            .body(chunk.to_vec())
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(err) if attempt < CHUNK_ATTEMPTS => {
                tracing::warn!("chunk upload attempt {attempt} failed: {err}");
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            Err(err) => {
                return Err(anyhow!(
                    "chunk upload failed after {attempt} attempts: {err}"
                ));
            }
        };

        let status = response.status().as_u16();
        match classify_chunk(status) {
            ChunkReply::Complete => {
                return Ok(Chunk::Complete(response.json::<google::DriveFile>().await?));
            }
            ChunkReply::Advanced => {
                return Ok(Chunk::Advanced(
                    google::committed_bytes(&response).min(total),
                ));
            }
            ChunkReply::Unauthorized => return Ok(Chunk::Unauthorized),
            ChunkReply::Expired => {
                // Google's reason matters here: it is all the diagnosis anyone gets when
                // a push fails, so it travels with the status instead of being dropped.
                let body = response.text().await.unwrap_or_default();
                return Ok(Chunk::Expired { status, body });
            }
            ChunkReply::Throttled | ChunkReply::Transient if attempt < CHUNK_ATTEMPTS => {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!("chunk upload attempt {attempt} returned {status}: {body}");
                tokio::time::sleep(backoff(attempt)).await;
            }
            ChunkReply::Throttled | ChunkReply::Transient => {
                let body = response.text().await.unwrap_or_default();
                bail!("chunk upload returned {status} after {attempt} attempts: {body}");
            }
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(5))
}

#[allow(clippy::too_many_arguments)]
async fn open_session(
    state: &State,
    client: &reqwest::Client,
    access_token: &str,
    push: &GDrivePush,
    file_name: &str,
    folder_id: &str,
    total: u64,
    server_uuid: uuid::Uuid,
) -> Result<String, anyhow::Error> {
    let uri = google::start_resumable_upload(
        client,
        access_token,
        file_name,
        folder_id,
        "application/gzip",
        total,
        &server_uuid.to_string(),
        &push.backup_uuid.to_string(),
    )
    .await?;

    GDrivePush::set_session(
        &state.database,
        push.backup_uuid,
        push.user_uuid,
        &uri,
        total as i64,
    )
    .await?;

    Ok(uri.to_string())
}

/// Move a browser-facing download URL onto the node's own address.
///
/// `download_url()` builds what a *browser* should fetch: it prefers the node's public
/// URL, which may be the panel's own `/wings-proxy/…` path. The panel only proxies that
/// path when `APP_ENABLE_WINGS_PROXY` is on - otherwise the request falls through to the
/// SPA and answers `200` with `index.html`, which staging would have uploaded as the
/// "archive" (and did, until restore downloaded it back). Staging runs inside the panel,
/// where the node's own address is the channel every other panel → Wings call already
/// uses, so a token-carrying Wings URL is rebased there; everything else - an S3
/// presign, a URL already pointing at the node - passes through untouched.
fn rebase_onto_node(source_url: &str, node: &shared::models::node::Node) -> String {
    match reqwest::Url::parse(source_url) {
        Ok(parsed) if is_wings_download_url(&parsed) => {
            let mut url = node.url("/download/backup");
            // The single-use backup-download JWT travels verbatim, query included.
            url.set_query(parsed.query());
            url.to_string()
        }
        _ => source_url.to_string(),
    }
}

/// Whether a download URL really is Wings' own download route, however it was dressed up
/// with a public or proxy prefix on the way out.
fn is_wings_download_url(url: &reqwest::Url) -> bool {
    url.path().ends_with("/download/backup")
        && url
            .query()
            .is_some_and(|query| query.split('&').any(|pair| pair.starts_with("token=")))
}

/// The two-byte gzip magic. We always ask Wings for a tar.gz, so staged bytes that do
/// not start with it are an error page in disguise.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// How much of the first chunk is kept for the failure message.
const PREVIEW_LEN: usize = 64;

/// A one-line, log-safe rendering of whatever bytes came back, so a failed push says
/// *what* was served (`<!DOCTYPE html>…`) rather than only that something was wrong.
fn printable_preview(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)".to_string();
    }

    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Whether a file on disk begins with the gzip magic.
async fn starts_with_gzip(path: &PathBuf) -> Result<bool, anyhow::Error> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut magic = [0u8; 2];
    let read = file.read(&mut magic).await?;

    Ok(read == 2 && magic == GZIP_MAGIC)
}

/// Download the archive into a staging file and return its exact byte length.
///
/// The download only ever lands under a `.part` name and is renamed once complete, so
/// the final path existing *and starting with gzip* is the proof the archive is whole -
/// which is what lets a retry after a failed upload skip straight to uploading. A staged
/// file that is not gzip (a stale page from an earlier broken download URL) is deleted
/// rather than reused: re-uploading known-bad bytes is exactly the bug this checks for.
async fn stage(
    source_url: &str,
    final_path: &PathBuf,
    part_path: &PathBuf,
) -> Result<u64, anyhow::Error> {
    if let Ok(metadata) = tokio::fs::metadata(final_path).await
        && metadata.len() >= 2
        && starts_with_gzip(final_path).await?
    {
        return Ok(metadata.len());
    }
    let _ = tokio::fs::remove_file(final_path).await;

    if let Some(parent) = final_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Deliberately not `state.client`: its read timeout suits short API calls, not a
    // multi-gigabyte archive that must never be cut off mid-transfer.
    let client = crate::tokens::client()?;
    let response = client
        .get(source_url)
        .send()
        .await
        .context("requesting the backup archive")?
        .error_for_status()?;

    let mut stream = response.bytes_stream();
    let mut file = tokio::fs::File::create(part_path).await?;
    let mut total: u64 = 0;
    let mut magic = [0u8; 2];
    let mut magic_read = 0usize;
    let mut preview: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the backup archive")?;

        if magic_read < 2 {
            let take = (2 - magic_read).min(chunk.len());
            magic[magic_read..magic_read + take].copy_from_slice(&chunk[..take]);
            magic_read += take;
        }
        if preview.is_empty() && !chunk.is_empty() {
            preview.extend_from_slice(&chunk[..chunk.len().min(PREVIEW_LEN)]);
        }

        total += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    if total == 0 {
        let _ = tokio::fs::remove_file(part_path).await;
        bail!("the backup archive downloaded as 0 bytes");
    }

    // A download URL that fails loudly is easy to diagnose; one that answers `200` with
    // someone else's content is how the panel's own HTML page became a "backup" that
    // every check short of downloading it again called healthy.
    if magic_read < 2 || magic != GZIP_MAGIC {
        let _ = tokio::fs::remove_file(part_path).await;
        bail!(
            "the backup source did not return a gzip archive (starts with: {}); \
             the download URL served an error page instead of the backup",
            printable_preview(&preview)
        );
    }

    tokio::fs::rename(part_path, final_path).await?;

    Ok(total)
}

/// Deterministic staging paths, so a retry (or a restart) finds the archive again.
fn staging_paths(push: &GDrivePush) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir()
        .join("calagopus-gdrive")
        .join(format!("{}-{}", push.backup_uuid, push.user_uuid));

    (base.with_extension("tar.gz"), base.with_extension("part"))
}

async fn remove_staged(push: &GDrivePush) {
    let (final_path, part_path) = staging_paths(push);
    let _ = tokio::fs::remove_file(final_path).await;
    let _ = tokio::fs::remove_file(part_path).await;
}

/// Send one of this user's files to Drive's trash, using their own connection.
///
/// Shared by the three moments a file has outlived its record: the copy a re-upload just
/// superseded, the copy a push uploaded after its row was cancelled out from under it, and
/// the copy left behind when the backup it came from was deleted.
pub(crate) async fn trash_file(
    state: &State,
    push: &GDrivePush,
    file_id: &str,
) -> Result<(), anyhow::Error> {
    let connection = GDriveConnection::by_user_uuid(&state.database, push.user_uuid)
        .await?
        .ok_or_else(|| anyhow!("the Google Drive account is no longer linked"))?;

    let client = tokens::client()?;
    let access_token = tokens::access_token(&client, state, &connection).await?;

    google::delete_file(&client, &access_token, file_id).await
}

/// Which Drive folder this server's backups land in: the linked account's backup root,
/// or - when the operator has folder-per-server on - a subfolder named after the server,
/// created on first push.
///
/// A single `files.list` decides it: an existing folder is reused across pushes, so the
/// common case costs one cheap read rather than a create-per-push (which would litter
/// duplicate folders the moment two pushes raced). The name goes through the same
/// separator sanitisation as file names, so a server called `a/b` and one called `a\b`
/// resolve to the same folder rather than one of them failing Drive's name rules.
async fn resolve_destination_folder(
    state: &State,
    client: &reqwest::Client,
    access_token: &str,
    connection: &GDriveConnection,
    server_name: &str,
) -> Result<String, anyhow::Error> {
    let settings = settings::current(state).await?;

    if !settings.folder_per_server {
        return Ok(connection.folder_id.to_string());
    }

    let folder_name = server_name.replace(['/', '\\'], "-");

    if let Some(existing) =
        google::find_folder(client, access_token, &connection.folder_id, &folder_name).await?
    {
        return Ok(existing.to_string());
    }

    Ok(google::create_folder(
        client,
        access_token,
        &folder_name,
        Some(&connection.folder_id),
    )
    .await?
    .to_string())
}

/// Apply the keep-last-N bound for the server this push just landed for.
///
/// Reads its own settings (a push already holds the worker for its whole duration, so
/// there is no batch to share a read across), no-ops on `0`, and passes the file that
/// was just uploaded so it can never prune itself.
async fn enforce_retention(
    state: &State,
    push: &GDrivePush,
    server_uuid: uuid::Uuid,
    just_uploaded: &str,
) -> Result<(), anyhow::Error> {
    let keep_copies = settings::current(state).await?.keep_copies;

    crate::retention::enforce(
        state,
        push.user_uuid,
        server_uuid,
        keep_copies,
        just_uploaded,
    )
    .await
}

/// Drive has no path separators in names, and a server name containing `/` would
/// otherwise be rejected outright.
fn drive_file_name(server_name: &str, backup_name: &str) -> String {
    let sanitize = |input: &str| input.replace(['/', '\\'], "-");

    format!(
        "{} - {}.tar.gz",
        sanitize(server_name),
        sanitize(backup_name)
    )
}

fn truncate(message: &str) -> String {
    if message.len() <= MAX_ERROR_LEN {
        return message.to_string();
    }

    let mut end = MAX_ERROR_LEN;
    while !message.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::{get, put},
    };

    use super::*;

    #[test]
    fn wings_download_urls_are_recognised_however_they_are_dressed() {
        let direct = reqwest::Url::parse(
            "http://127.0.0.1:8082/download/backup?token=abc.def.ghi&archive_format=tar_gz",
        )
        .unwrap();
        let proxied = reqwest::Url::parse(
            "https://panel.example/wings-proxy/00000000-0000-0000-0000-000000000000\
             /download/backup?token=x&archive_format=tar_gz",
        )
        .unwrap();
        let presigned = reqwest::Url::parse(
            "https://storage.googleapis.com/bucket/servers/a/b.tar.gz?X-Goog-Signature=abc",
        )
        .unwrap();
        let ordinary = reqwest::Url::parse("https://panel.example/backups/123").unwrap();

        assert!(is_wings_download_url(&direct), "the node's own URL");
        assert!(
            is_wings_download_url(&proxied),
            "the public URL wearing a wings-proxy prefix"
        );
        assert!(!is_wings_download_url(&presigned), "an S3 presign");
        assert!(!is_wings_download_url(&ordinary), "anything else");
    }

    #[test]
    fn the_failure_preview_shows_what_really_arrived() {
        assert_eq!(printable_preview(b"<!DOCTYPE html>"), "<!DOCTYPE html>");
        assert_eq!(
            printable_preview(b"<!DOCTYPE html>\n<html"),
            "<!DOCTYPE html>.<html"
        );
        assert_eq!(printable_preview(b""), "(empty)");
    }

    #[test]
    fn content_range_ends_on_the_last_byte_the_chunk_carries() {
        // The first full chunk: 8 MiB of bytes 0..=8388607.
        assert_eq!(
            content_range(0, 8 * 1024 * 1024, 10 * 1024 * 1024),
            "bytes 0-8388607/10485760"
        );
        // A middle chunk continues exactly where the previous one stopped: byte 8388608
        // is the first byte it carries, not the last.
        assert_eq!(
            content_range(8 * 1024 * 1024, 2 * 1024 * 1024, 10 * 1024 * 1024),
            "bytes 8388608-10485759/10485760"
        );
        // A file smaller than one chunk. The end must be total - 1: advertising byte
        // `total` claims a byte beyond the file, which Google rejects with a
        // malformed-range 400 - the failure that used to masquerade as a dead session.
        assert_eq!(content_range(0, 100, 100), "bytes 0-99/100");
    }

    #[test]
    fn replies_are_classified_by_what_they_cost_the_transfer() {
        use ChunkReply::*;

        assert_eq!(classify_chunk(200), Complete);
        assert_eq!(classify_chunk(201), Complete);
        assert_eq!(classify_chunk(308), Advanced);
        // Token trouble costs a refresh and nothing else: the session is still fine.
        assert_eq!(classify_chunk(401), Unauthorized);
        // Asking us to slow down (403 rate limit, 429) does not invalidate anything.
        assert_eq!(classify_chunk(403), Throttled);
        assert_eq!(classify_chunk(429), Throttled);
        // Malformed request or a session that is simply gone.
        assert_eq!(classify_chunk(400), Expired);
        assert_eq!(classify_chunk(404), Expired);
        assert_eq!(classify_chunk(410), Expired);
        // Server-side trouble: repeating the identical request may work.
        assert_eq!(classify_chunk(500), Transient);
        assert_eq!(classify_chunk(503), Transient);
    }

    /// Google's own rule for a data `PUT`: the inclusive range must describe exactly the
    /// bytes in the body. Anything it cannot parse counts as a mismatch, as it would to
    /// Google.
    fn range_matches_body(range: &str, body_len: usize) -> bool {
        let Some(spec) = range.strip_prefix("bytes ") else {
            return false;
        };
        let Some((span, _total)) = spec.split_once('/') else {
            return false;
        };
        let Some((start, end)) = span.split_once('-') else {
            return false;
        };
        let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
            return false;
        };

        end >= start && (end - start + 1) as usize == body_len
    }

    /// Requests the stub saw: (authorization, content-range, body length).
    type Seen = Arc<Mutex<Vec<(Option<String>, String, usize)>>>;

    /// A stand-in for Google that enforces the two rules the real API enforces: a bearer
    /// token on every data `PUT`, and a range that lines up with the bytes sent.
    async fn recording_upload(
        State(seen): State<Seen>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let range = headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();

        seen.lock()
            .unwrap()
            .push((authorization.clone(), range.clone(), body.len()));

        if authorization.is_none() {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
        if range_matches_body(&range, body.len()) {
            // Stored, but nothing reported as committed: a `308` with no `Range`.
            return (StatusCode::from_u16(308).unwrap(), "").into_response();
        }

        (
            StatusCode::BAD_REQUEST,
            "Failed to parse Content-Range header",
        )
            .into_response()
    }

    /// A stub that answers every data `PUT` the same way, whatever was sent.
    async fn stub_upload(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                Router::new().route(
                    "/upload",
                    put(move || {
                        let status = status;
                        async move { (StatusCode::from_u16(status).unwrap(), body).into_response() }
                    }),
                ),
            )
            .await;
        });

        (format!("http://{address}/upload"), handle)
    }

    /// Regression test for the two bugs that failed the very first real upload: chunks
    /// advertised a range one byte longer than the body they carried, and neither the
    /// data `PUT` nor the status query carried the bearer token Google checks.
    #[tokio::test]
    async fn data_puts_authorize_and_end_on_the_last_byte_they_send() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let capture = seen.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                Router::new()
                    .route("/upload", put(recording_upload))
                    .with_state(capture),
            )
            .await;
        });

        let client = reqwest::Client::new();
        // A first chunk and a middle one, against a server that answers `401` to a
        // missing token and `400` to a range that does not match the body.
        for (start, total) in [(0u64, 10_000u64), (4_096, 10_000u64)] {
            let chunk = vec![7u8; 4_096];
            let reply = put_chunk(
                &client,
                &format!("http://{address}/upload"),
                "test-token",
                &chunk,
                start,
                total,
            )
            .await
            .unwrap();

            assert!(
                matches!(reply, Chunk::Advanced(_)),
                "expected the chunk to be accepted, got {reply:?}"
            );
        }
        server.abort();

        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2, "both chunks should have been sent");
        assert_eq!(
            requests[0].0.as_deref(),
            Some("Bearer test-token"),
            "data PUTs must carry the bearer token"
        );
        assert_eq!(requests[0].1, "bytes 0-4095/10000");
        assert_eq!(requests[0].2, 4_096);
        assert_eq!(requests[1].1, "bytes 4096-8191/10000");
        assert!(
            requests
                .iter()
                .all(|(_, range, length)| range_matches_body(range, *length)),
            "every range must describe exactly the bytes sent: {requests:?}"
        );
    }

    /// A stale access token must come back as "refresh me", not "the session is dead" -
    /// treating it as expiry discarded a live session and burned a push attempt.
    #[tokio::test]
    async fn a_stale_token_reads_as_unauthorized_so_the_session_survives() {
        let (uri, server) = stub_upload(401, "Invalid Credentials").await;
        let client = reqwest::Client::new();
        let reply = put_chunk(&client, &uri, "stale", &[0u8; 16], 0, 16).await;
        server.abort();

        match reply.unwrap() {
            Chunk::Unauthorized => {}
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    /// Stub the backup download route with a fixed body.
    async fn stub_download(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                Router::new().route(
                    "/download/backup",
                    get(move || {
                        let body = body;
                        async move { (StatusCode::OK, Bytes::from_static(body)).into_response() }
                    }),
                ),
            )
            .await;
        });

        (format!("http://{address}/download/backup"), handle)
    }

    /// Fresh staging paths for a test, keyed by a uuid so parallel tests never collide.
    fn test_staging_paths() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir()
            .join("calagopus-gdrive-tests")
            .join(uuid::Uuid::new_v4().to_string());

        (base.with_extension("tar.gz"), base.with_extension("part"))
    }

    /// The regression that let the panel's own HTML page become a "backup": a download
    /// URL that answers `200` with an error page must fail the push - naming the bytes
    /// it got - and must never land in staging, where a retry would happily re-upload it.
    #[tokio::test]
    async fn staging_refuses_a_non_gzip_answer_and_names_what_it_got() {
        let (uri, server) = stub_download(b"<!DOCTYPE html>").await;
        let (final_path, part_path) = test_staging_paths();

        let error = stage(&uri, &final_path, &part_path).await.unwrap_err();
        server.abort();

        let message = format!("{error:#}");
        assert!(
            message.contains("did not return a gzip archive"),
            "message was {message:?}"
        );
        assert!(
            message.contains("<!DOCTYPE html>"),
            "message was {message:?}"
        );
        assert!(!final_path.exists(), "the bogus bytes must not be staged");
        assert!(
            !part_path.exists(),
            "the bogus bytes must not survive either"
        );
    }

    /// The happy path: gzip bytes stage once, and a repeat call finds the whole file
    /// again instead of re-downloading (the stub is already aborted by then).
    #[tokio::test]
    async fn staging_accepts_gzip_bytes_and_reuses_a_whole_stage() {
        const ARCHIVE: &[u8] = b"\x1f\x8b\x08fake-archive-tail";

        let (uri, server) = stub_download(ARCHIVE).await;
        let (final_path, part_path) = test_staging_paths();

        let total = stage(&uri, &final_path, &part_path).await.unwrap();
        assert_eq!(total, ARCHIVE.len() as u64);
        assert!(final_path.exists(), "the finished stage is the .tar.gz");
        server.abort();

        let again = stage(&uri, &final_path, &part_path).await.unwrap();
        assert_eq!(again, total, "a whole gzip stage must be reused as-is");
    }

    /// A staged file that is not gzip (left behind by an older, broken download) is
    /// dropped rather than trusted: existence alone must not certify a stage.
    #[tokio::test]
    async fn a_stale_non_gzip_stage_is_discarded_and_redownloaded() {
        let (final_path, part_path) = test_staging_paths();
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&final_path, b"<!DOCTYPE html>")
            .await
            .unwrap();

        let (uri, server) = stub_download(b"<!DOCTYPE html>").await;
        let error = stage(&uri, &final_path, &part_path).await.unwrap_err();
        server.abort();

        assert!(
            format!("{error:#}").contains("did not return a gzip archive"),
            "the stale page must not be reused, and the fresh download is no better"
        );
        assert!(!final_path.exists(), "the stale page must be gone");
    }

    /// When a session really is gone, Google's status and reason must survive into the
    /// reply - they are all the diagnosis anyone gets from a failed push.
    #[tokio::test]
    async fn an_invalidated_session_reports_its_status_and_google_s_reason() {
        let (uri, server) = stub_upload(410, "The upload session has expired.").await;
        let client = reqwest::Client::new();
        let reply = put_chunk(&client, &uri, "token", &[0u8; 16], 0, 16).await;
        server.abort();

        match reply.unwrap() {
            Chunk::Expired { status, body } => {
                assert_eq!(status, 410);
                assert!(body.contains("expired"), "body was {body:?}");
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }
}
