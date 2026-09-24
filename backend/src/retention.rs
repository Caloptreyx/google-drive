//! Keeping only the newest *N* copies of each server's backups in Drive.
//!
//! Runs after every successful push rather than on a timer: a push is the moment new
//! files appear, it already holds a live token and the server's identity, and doing it
//! here means the bound holds continuously without a second worker. A push that fails
//! never prunes - trimming the copies you have while the new one didn't land is exactly
//! the wrong trade.
//!
//! Selection is by the `appProperties` server tag stamped on every upload (see
//! [`crate::google::list_server_files`]), so it works across flat and per-server folder
//! layouts alike, and files this extension never created are simply invisible to it.
//! Everything pruned goes to Drive's *trash*, recoverable for thirty days.

use shared::State;

use crate::{
    google::{self, FileMetadata},
    models::GDriveConnection,
    tokens,
};

/// Decide which of a server's Drive copies are over the bound.
///
/// `files` must arrive newest-first (as `list_server_files` orders them: `createdTime
/// desc`). Everything past the first `keep` is pruned; `keep <= 0` prunes nothing, which
/// is how "keep everything" is expressed everywhere else in the settings too.
///
/// Pure so the ordering rule - newest survive, oldest go - is testable without a
/// network, which is where this logic would otherwise be impossible to pin down.
pub fn files_to_prune(files: &[FileMetadata], keep: i64) -> Vec<String> {
    if keep <= 0 {
        return Vec::new();
    }

    files
        .iter()
        .skip(keep as usize)
        .map(|file| file.id.to_string())
        .collect()
}

/// Enforce the keep-last-N bound for one server, if the operator set one.
///
/// Best effort by design: called right after a push succeeded, and a prune that fails
/// must not turn a completed upload into a failed one. Failures are logged with the
/// ids involved so they stay diagnosable.
pub async fn enforce(
    state: &State,
    user_uuid: uuid::Uuid,
    server_uuid: uuid::Uuid,
    keep_copies: i64,
    just_uploaded: &str,
) -> Result<(), anyhow::Error> {
    if keep_copies <= 0 {
        return Ok(());
    }

    let connection = GDriveConnection::by_user_uuid(&state.database, user_uuid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the Google Drive account is no longer linked"))?;

    let client = tokens::client()?;
    let access_token = tokens::access_token(&client, state, &connection).await?;

    let files = google::list_server_files(&client, &access_token, &server_uuid.to_string()).await?;

    for file_id in files_to_prune(&files, keep_copies) {
        // Belt and braces: the file that just landed is the newest by construction, but
        // a clock skew or a createdTime tie must never prune the upload we just made.
        if file_id == just_uploaded {
            continue;
        }

        google::delete_file(&client, &access_token, &file_id).await?;

        tracing::info!(
            user = %user_uuid,
            server = %server_uuid,
            file = %file_id,
            keep = keep_copies,
            "pruned an old google drive copy past the retention bound"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str) -> FileMetadata {
        FileMetadata {
            id: id.into(),
            name: format!("{id}.tar.gz").into(),
            size: Some(1),
            created_time: None,
            modified_time: None,
            mime_type: None,
            app_properties: None,
        }
    }

    #[test]
    fn zero_keep_prunes_nothing() {
        let files = vec![file("new"), file("mid"), file("old")];
        assert!(files_to_prune(&files, 0).is_empty());
        assert!(files_to_prune(&files, -1).is_empty());
    }

    #[test]
    fn newest_survive_and_oldest_go() {
        // The listing arrives newest-first.
        let files = vec![file("newest"), file("mid"), file("old"), file("oldest")];

        assert_eq!(files_to_prune(&files, 2), vec!["old", "oldest"]);
    }

    #[test]
    fn keep_at_or_above_the_count_prunes_nothing() {
        let files = vec![file("a"), file("b")];
        assert!(files_to_prune(&files, 2).is_empty());
        assert!(files_to_prune(&files, 99).is_empty());
    }

    #[test]
    fn keep_one_leaves_exactly_the_newest() {
        let files = vec![file("newest"), file("older")];
        assert_eq!(files_to_prune(&files, 1), vec!["older"]);
    }
}
