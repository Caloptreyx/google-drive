//! Catching up on finished backups the completion listener never queued.
//!
//! The listener is the primary path, and it is deliberately fire-and-forget: its errors
//! are logged and dropped so nothing here can look like it can fail a backup. That same
//! property means a listener that ran while the panel was restarting, hit a transient
//! database error, or simply didn't exist yet (auto-push switched on after the fact)
//! leaves a finished backup with no push row, and nothing would ever retry it.
//!
//! This sweep is that retry. It runs from the push worker's idle loop, bounded on
//! every side - a lookback window, a per-run batch, the owner's own connection
//! lifetime, and the same push rules and switches the listener applies - so it repairs
//! gaps without ever becoming a backfill-the-whole-archive button. See
//! [`crate::models::reconcile_candidates`] for the exact shape of those bounds.

use shared::State;

use crate::{
    models::{GDriveConnection, GDrivePush, ReconcileCandidate},
    rules::PushRules,
    settings,
};

/// How far back a run looks for unqueued completions.
const LOOKBACK_HOURS: i32 = 48;
/// Backups queued per run. Anything older than this drains on subsequent runs, one
/// batch at a time, rather than enqueuing an entire backlog in one tick.
const BATCH: i64 = 10;

/// One reconcile pass. Called from the worker's loop; never fails the worker - a
/// broken sweep is logged, and the next tick tries again.
pub async fn sweep(state: &State) -> Result<(), anyhow::Error> {
    let settings = settings::current(state).await?;

    if !settings.auto_push || !settings.is_configured() {
        return Ok(());
    }

    let rules = PushRules::from_settings(&settings);
    let candidates =
        crate::models::reconcile_candidates(&state.database, LOOKBACK_HOURS, BATCH).await?;

    for candidate in candidates {
        queue_if_due(state, &settings, &rules, candidate).await?;
    }

    Ok(())
}

async fn queue_if_due(
    state: &State,
    settings: &settings::GDriveSettingsData,
    rules: &PushRules,
    candidate: ReconcileCandidate,
) -> Result<(), anyhow::Error> {
    // The SQL already joined on connection and finished-ness; these are the policy
    // gates, applied identically to the listener so the two paths can't disagree about
    // what "should be queued" means.
    if candidate.kind == "DATABASE_INSTANCE" && !settings.push_database_backups {
        return Ok(());
    }

    if !rules.allows(&candidate.server_name) {
        return Ok(());
    }

    // Re-checked here because the join saw the connection at query time and the sweep
    // may run a moment later; also covers a user who unlinked in between.
    if GDriveConnection::by_user_uuid(&state.database, candidate.owner_uuid)
        .await?
        .is_none()
    {
        return Ok(());
    }

    // Idempotent: an already-queued row is left alone by `enqueue`, so a candidate that
    // slipped in between the query and here costs nothing.
    GDrivePush::enqueue(&state.database, candidate.backup_uuid, candidate.owner_uuid).await?;

    tracing::info!(
        backup = %candidate.backup_uuid,
        user = %candidate.owner_uuid,
        server = %candidate.server_name,
        "reconciled a finished backup the listener missed; queued for google drive push"
    );

    Ok(())
}
