//! Keeping only the newest *N* copies of each server's backups in Drive and removing duplicates.
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

use std::collections::HashSet;
use shared::State;
use sqlx::Row;

use crate::{
    google::{self, BACKUP_PROPERTY, FileMetadata},
    models::GDriveConnection,
    settings, tokens,
};

/// Decide which of a server's Drive copies are duplicates or over the retention bound.
///
/// `files` must arrive newest-first (as `list_server_files` orders them: `createdTime desc`).
///
/// Deduplication rules:
/// 1. 0-byte files (aborted / incomplete upload sessions) are always pruned.
/// 2. If multiple files have the same `BACKUP_PROPERTY` (the backup UUID), only the newest
///    survives; all older duplicates are pruned.
/// 3. If files are untagged, duplicates with the same name are pruned.
///
/// Retention rules:
/// If `keep > 0`, only the newest `keep` unique backups are kept; anything older is pruned.
/// If `keep <= 0`, all unique backups are kept, but duplicates and 0-byte files are still pruned.
pub fn files_to_prune(files: &[FileMetadata], keep: i64) -> Vec<String> {
    let mut to_prune = Vec::new();
    let mut seen_backups = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut kept_count: usize = 0;

    for file in files {
        // 1. Any 0-byte file is an aborted / invalid upload: prune it
        if file.size == Some(0) {
            to_prune.push(file.id.to_string());
            continue;
        }

        // 2. Check for duplicate backup UUID
        let backup_uuid = file.app_properties.as_ref().and_then(|props| {
            props.get(BACKUP_PROPERTY).map(|s| s.as_str())
        });

        let is_duplicate = match backup_uuid {
            Some(uuid) => !seen_backups.insert(uuid.to_string()),
            None => !seen_names.insert(file.name.to_string()),
        };

        if is_duplicate {
            to_prune.push(file.id.to_string());
            continue;
        }

        // 3. Unique backup: check retention bound
        if keep > 0 {
            if kept_count >= keep as usize {
                to_prune.push(file.id.to_string());
            } else {
                kept_count += 1;
            }
        }
    }

    to_prune
}

/// Enforce retention and deduplication for one server.
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
) -> Result<usize, anyhow::Error> {
    let connection = GDriveConnection::by_user_uuid(&state.database, user_uuid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the Google Drive account is no longer linked"))?;

    let client = tokens::client()?;
    let access_token = tokens::access_token(&client, state, &connection).await?;

    let files = google::list_server_files(&client, &access_token, &server_uuid.to_string()).await?;

    let prunables = files_to_prune(&files, keep_copies);
    let mut pruned_count = 0;

    for file_id in prunables {
        // Belt and braces: the file that just landed is the newest by construction, but
        // a clock skew or a createdTime tie must never prune the upload we just made.
        if file_id == just_uploaded {
            continue;
        }

        if let Err(err) = google::delete_file(&client, &access_token, &file_id).await {
            tracing::warn!(
                user = %user_uuid,
                server = %server_uuid,
                file = %file_id,
                "failed to prune old or duplicate file: {err:#}"
            );
        } else {
            pruned_count += 1;
            tracing::info!(
                user = %user_uuid,
                server = %server_uuid,
                file = %file_id,
                keep = keep_copies,
                "pruned a duplicate or old google drive copy past retention bound"
            );
        }
    }

    Ok(pruned_count)
}

/// Clean up duplicates and enforce retention bounds for all servers belonging to a user.
pub async fn clean_user_backups(
    state: &State,
    user_uuid: uuid::Uuid,
) -> Result<usize, anyhow::Error> {
    let connection = GDriveConnection::by_user_uuid(&state.database, user_uuid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the Google Drive account is no longer linked"))?;

    let admin_keep = settings::current(state)
        .await
        .map(|s| s.keep_copies)
        .unwrap_or(0);

    let effective_keep = connection.keep_copies.unwrap_or(admin_keep);

    let server_rows = sqlx::query("SELECT uuid FROM servers WHERE owner_uuid = $1")
        .bind(user_uuid)
        .fetch_all(state.database.read())
        .await?;

    let mut total_pruned = 0;
    for row in server_rows {
        let server_uuid: uuid::Uuid = row.try_get("uuid")?;
        match enforce(state, user_uuid, server_uuid, effective_keep, "").await {
            Ok(count) => total_pruned += count,
            Err(err) => {
                tracing::warn!(
                    user = %user_uuid,
                    server = %server_uuid,
                    "failed to enforce retention for server: {err:#}"
                );
            }
        }
    }

    Ok(total_pruned)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use compact_str::CompactString;
    use super::*;

    fn file(id: &str) -> FileMetadata {
        FileMetadata {
            id: id.into(),
            name: format!("{id}.tar.gz").into(),
            size: Some(1024),
            created_time: None,
            modified_time: None,
            mime_type: None,
            app_properties: None,
        }
    }

    fn file_with_backup(id: &str, backup_uuid: &str, size: i64) -> FileMetadata {
        let mut props = BTreeMap::new();
        props.insert(CompactString::from(BACKUP_PROPERTY), CompactString::from(backup_uuid));
        FileMetadata {
            id: id.into(),
            name: format!("{backup_uuid}.tar.gz").into(),
            size: Some(size),
            created_time: None,
            modified_time: None,
            mime_type: None,
            app_properties: Some(props),
        }
    }

    #[test]
    fn zero_keep_prunes_nothing_when_unique() {
        let files = vec![file("new"), file("mid"), file("old")];
        assert!(files_to_prune(&files, 0).is_empty());
        assert!(files_to_prune(&files, -1).is_empty());
    }

    #[test]
    fn zero_byte_files_are_pruned() {
        let mut f1 = file("zero1");
        f1.size = Some(0);
        let f2 = file("valid");
        let mut f3 = file("zero2");
        f3.size = Some(0);

        let files = vec![f1, f2, f3];
        assert_eq!(files_to_prune(&files, 0), vec!["zero1", "zero2"]);
    }

    #[test]
    fn duplicate_backup_uuids_pruned_even_with_zero_keep() {
        let f1 = file_with_backup("newest_copy", "backup-1", 100);
        let f2 = file_with_backup("older_copy", "backup-1", 100);
        let f3 = file_with_backup("other_backup", "backup-2", 200);

        let files = vec![f1, f2, f3];
        // f2 is an older duplicate of backup-1
        assert_eq!(files_to_prune(&files, 0), vec!["older_copy"]);
    }

    #[test]
    fn untagged_duplicate_filenames_pruned() {
        let f1 = file("same_name");
        let f2 = file("same_name");
        let f3 = file("other");

        let files = vec![f1, f2, f3];
        assert_eq!(files_to_prune(&files, 0), vec!["same_name"]);
    }

    #[test]
    fn newest_survive_and_oldest_go() {
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

    #[test]
    fn duplicates_and_retention_combined() {
        // Suppose backup-1 has 2 copies (newest, old_copy), backup-2 has 1, backup-3 has 1
        let f1 = file_with_backup("b1_new", "backup-1", 100);
        let f2 = file_with_backup("b1_dup", "backup-1", 100);
        let f3 = file_with_backup("b2", "backup-2", 100);
        let f4 = file_with_backup("b3", "backup-3", 100);

        let files = vec![f1, f2, f3, f4];
        // keep = 2: b1_new is kept (1), b1_dup is duplicate (pruned), b2 is kept (2), b3 exceeds keep (pruned)
        assert_eq!(files_to_prune(&files, 2), vec!["b1_dup", "b3"]);
    }
}
