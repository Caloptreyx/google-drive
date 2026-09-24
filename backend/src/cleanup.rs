//! Retiring the Drive copy when the backup it was made from is deleted.
//!
//! Listens for [`ServerBackupEvent::DeletionCompleted`] rather than installing a
//! before-delete hook, and the reason is worth recording: backups are **soft**-deleted -
//! `finish_deletion` writes `deleted = NOW()` and the row is never removed - so by the
//! time the event fires the `gdrive_pushes` rows are still there, file ids and all. A
//! hook running against a hard delete would have found them already cascade-removed.
//!
//! `successful` decides everything. Core emits the same event with `false` once a
//! deletion has burned through its retries, and a backup whose deletion gave up is
//! still a backup: cleaning up there would take out the only off-site copy of something
//! the panel still has.

use shared::{State, models::server_backup::ServerBackup};

use crate::{
    models::{GDrivePush, PushStatus},
    settings,
};

/// React to a backup finishing being deleted.
pub async fn on_backup_deleted(
    state: &State,
    backup: &ServerBackup,
    successful: bool,
) -> Result<(), anyhow::Error> {
    if !successful {
        return Ok(());
    }

    // No kind guard here: database backups can be pushed when the operator allows them,
    // so their rows carry file ids to retire exactly like file backups do. A row that
    // doesn't exist is handled by `for_backup` returning nothing.

    if !settings::current(state).await?.delete_copy {
        return Ok(());
    }

    for push in GDrivePush::for_backup(&state.database, backup.uuid).await? {
        // Anything still queued is cancelled by having its row removed, which is exactly
        // how a running upload is stopped - the worker re-reads the row before every
        // chunk and abandons the resumable session when it is gone.
        if push.status != PushStatus::Completed {
            GDrivePush::delete(&state.database, push.backup_uuid, push.user_uuid).await?;
            continue;
        }

        // Completed but with no file id means nothing ever landed in Drive, so the row
        // is just history for a backup that no longer exists.
        let Some(file_id) = push.drive_file_id.clone() else {
            GDrivePush::delete(&state.database, push.backup_uuid, push.user_uuid).await?;
            continue;
        };

        if let Err(err) = crate::upload::trash_file(state, &push, &file_id).await {
            // The row is kept deliberately. It is the only record of *which* file in
            // Drive belonged to this backup, and dropping it would turn a failed
            // cleanup into an orphan nothing can ever link back. It stays visible in
            // the Uploads table as a leftover, which is the honest picture, and the
            // file remains deletable by hand from the Files table above.
            tracing::warn!(
                backup = %push.backup_uuid,
                user = %push.user_uuid,
                file = %file_id,
                "failed to trash the google drive copy of a deleted backup: {err:#}"
            );

            continue;
        }

        GDrivePush::delete(&state.database, push.backup_uuid, push.user_uuid).await?;

        tracing::info!(
            backup = %push.backup_uuid,
            user = %push.user_uuid,
            file = %file_id,
            "trashed the google drive copy of a deleted backup"
        );
    }

    Ok(())
}
