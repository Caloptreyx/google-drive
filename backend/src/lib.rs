use shared::{
    State,
    extensions::{
        Extension, ExtensionPermissionsBuilder, ExtensionRouteBuilder, ExtensionUpdateInfo,
        settings::ExtensionSettingsDeserializer,
    },
};

mod auto_push;
mod cleanup;
mod google;
mod models;
mod reconcile;
mod retention;
mod routes;
mod rules;
mod settings;
mod tokens;
mod updates;
mod upload;

pub use settings::{GDriveSettingsData, GDriveSettingsDeserializer};

/// Package identifier; used as the settings key and in route namespaces.
pub const IDENTIFIER: &str = "dev.caloptreyx.gdrive";

#[derive(Default)]
pub struct ExtensionStruct;

#[async_trait::async_trait]
impl Extension for ExtensionStruct {
    async fn initialize(&mut self, state: State) {
        let configured = state
            .settings
            .get()
            .await
            .ok()
            .and_then(|settings| {
                settings
                    .get_extension_settings::<GDriveSettingsData>(IDENTIFIER)
                    .ok()
                    .map(|settings| settings.is_configured())
            })
            .unwrap_or(false);

        tracing::info!(
            "google drive backups extension loaded (operator credentials: {})",
            if configured { "set" } else { "not set yet" }
        );

        // The two listeners behind "push backups automatically" and "trash the copy when
        // the backup goes". One registration dispatching on the variant rather than two,
        // so there is a single place to see which events this extension reacts to.
        //
        // Registered exactly once, here, at startup - the Events docs are explicit that
        // registering twice makes the code run twice for every event, and there is no
        // per-registration guard.
        use shared::models::{
            EventEmittingModel,
            server_backup::{ServerBackup, ServerBackupEvent},
        };

        ServerBackup::register_event_handler(|state, event| async move {
            match &*event {
                ServerBackupEvent::CreationCompleted { backup, successful } => {
                    crate::auto_push::on_backup_created(&state, backup, *successful).await
                }
                ServerBackupEvent::DeletionCompleted { backup, successful } => {
                    crate::cleanup::on_backup_deleted(&state, backup, *successful).await
                }
                // Restores are somebody else's business: a restore changes the live
                // files, not the archive, so neither listener has anything to do.
                _ => Ok(()),
            }
        });
    }

    async fn initialize_router(
        &mut self,
        state: State,
        builder: ExtensionRouteBuilder,
    ) -> ExtensionRouteBuilder {
        // The callback is unauthenticated, so it lives on the auth router; everything
        // else sits behind core's session middleware. Every path is namespaced by the
        // package name - an unnamespaced merge collides with core and panics axum at boot.
        builder
            .add_auth_api_router(|router| router.nest("/gdrive", routes::auth::router(&state)))
            .add_admin_api_router(|router| {
                router.nest(routes::admin::BASE, routes::admin::router(&state))
            })
            .add_client_api_router(|router| {
                router.nest(routes::client::BASE, routes::client::router(&state))
            })
    }

    async fn initialize_permissions(
        &mut self,
        _state: State,
        mut builder: ExtensionPermissionsBuilder,
    ) -> ExtensionPermissionsBuilder {
        {
            let group = builder.user_permissions.entry("gdrive").or_insert_with(|| {
                shared::permissions::PermissionGroup {
                    description: "Link a Google Drive account and view its backup status.",
                    permissions: Default::default(),
                }
            });

            group.add_permission("read", "View Google Drive connection and push status.");
            group.add_permission(
                "connect",
                "Link or unlink the Google Drive account for this user.",
            );
            // Separate from `read` because it destroys data: a session always holds every
            // user permission, but an API key can be scoped to `gdrive.read` alone, and
            // without this one that key would be able to delete archives.
            group.add_permission(
                "manage",
                "Delete the backup files this extension pushed to the linked Drive.",
            );
        }

        {
            let group = builder
                .server_permissions
                .entry("gdrive")
                .or_insert_with(|| shared::permissions::PermissionGroup {
                    description: "Push this server's backups to a linked Google Drive.",
                    permissions: Default::default(),
                });

            group.add_permission("push", "Upload this server's backups to Google Drive.");
            // Separate from `push` for the same reason `gdrive.manage` is separate from
            // `read`: it overwrites the server's files. An API key scoped to pushing
            // must not thereby be able to restore over a running server.
            group.add_permission(
                "restore",
                "Restore this server's files from a Google Drive backup.",
            );
        }

        builder
    }

    async fn initialize_background_tasks(
        &mut self,
        _state: State,
        builder: shared::extensions::background_tasks::BackgroundTaskBuilder,
    ) -> shared::extensions::background_tasks::BackgroundTaskBuilder {
        // Primary instance only, and it gets no shutdown hook - which is why the queue's
        // state lives in the database rather than in this task.
        builder.add_task("gdrive_push_worker", upload::run).await;

        builder
    }

    /// Check GitHub for a newer release of this extension. The Panel calls it on
    /// startup, every 12 hours, and per admin recheck, and lists the answer under
    /// Outdated Extensions on Admin -> Home -> Updates. The fetch, the hourly cache and
    /// the version folding all live in `updates.rs`.
    async fn check_for_updates(
        &self,
        state: State,
        current_version: &semver::Version,
    ) -> Result<Option<ExtensionUpdateInfo>, anyhow::Error> {
        updates::check(&state, current_version).await
    }

    async fn settings_deserializer(&self, _state: State) -> ExtensionSettingsDeserializer {
        std::sync::Arc::new(GDriveSettingsDeserializer)
    }
}
