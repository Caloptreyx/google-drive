use serde::{Deserialize, Serialize};
use shared::extensions::settings::{
    ExtensionSettings, SettingsDeserializeExt, SettingsDeserializer, SettingsSerializeExt,
    SettingsSerializer,
};
use utoipa::ToSchema;

/// The operator-facing configuration for this extension.
///
/// These are the values the admin enters once on the extension's Configure page.
/// The `client_secret` is written through `write_raw_encrypted_setting` so it is
/// encrypted with the Panel's application key before it ever reaches the database.
#[derive(ToSchema, Serialize, Deserialize, Clone)]
pub struct GDriveSettingsData {
    /// The Google OAuth client ID from the Google Cloud console.
    pub client_id: compact_str::CompactString,
    /// The Google OAuth client secret. Encrypted at rest.
    pub client_secret: compact_str::CompactString,
    /// Human readable name of the Drive folder backups are collected in.
    pub folder_name: compact_str::CompactString,
    /// Queue a finished server backup for upload as soon as it completes, instead of
    /// waiting for someone to press Push.
    ///
    /// Defaults to on: this is a backup extension, and an operator who has filled in
    /// OAuth credentials and a user who has linked an account have both opted into
    /// archives leaving the panel. The switch is on the Configure page for the cases
    /// where they haven't.
    pub auto_push: bool,
    /// Trash the copy in Drive when the backup it came from is deleted.
    ///
    /// Also on by default, for the mirror-image reason: if pushes are automatic then
    /// retention pruning would otherwise strand a permanent Drive copy of every backup
    /// the panel threw away. Drive's delete is a *trash*, so a mistake is recoverable
    /// for thirty days.
    pub delete_copy: bool,
    /// Keep at most this many copies of each server's backups in Drive; 0 keeps them
    /// all. Enforced after every successful push against the `appProperties` tag
    /// stamped on each upload, so it works across flat and per-server folder layouts.
    pub keep_copies: i64,
    /// Glob patterns deciding which servers auto-push, e.g. `prod-*, staging/*`.
    ///
    /// Case-insensitive against the server name, `*` and `?` wildcards, one pattern per
    /// line or comma. Empty = every server. The manual Push button never consults
    /// these - an explicit click is an explicit decision.
    pub push_include: compact_str::CompactString,
    /// Patterns opting a server back *out* of auto-push, checked after the include list.
    pub push_exclude: compact_str::CompactString,
    /// Queue database-instance backups automatically and list them as pushable too.
    ///
    /// On by default alongside `auto_push`: an off-site copy of the database dumps is
    /// the same insurance as one of the server files, and `download_url` serves both
    /// kinds identically.
    pub push_database_backups: bool,
    /// Give every server its own subfolder under the backup folder instead of pushing
    /// everything flat into it.
    ///
    /// On by default: it keeps servers separated even when names are close enough to
    /// collide on a filename prefix, and it gives retention an exact per-server scope.
    /// Turning it off returns to the original flat layout; files already uploaded stay
    /// where they are either way.
    pub folder_per_server: bool,
    /// ID of a folder inside a shared drive to nest the backup folder under; empty
    /// means the user's own My Drive.
    ///
    /// Applies at (re)connect time: the root folder is created while linking, so an
    /// existing connection keeps pointing wherever it already points until the user
    /// unlinks and connects again. Enumerating shared drives needs a broader OAuth
    /// scope than this extension asks for, which is why the ID is pasted by hand.
    pub shared_folder_id: compact_str::CompactString,
}

impl Default for GDriveSettingsData {
    fn default() -> Self {
        Self {
            client_id: compact_str::CompactString::default(),
            client_secret: compact_str::CompactString::default(),
            folder_name: "Calagopus Backups".into(),
            auto_push: true,
            delete_copy: true,
            keep_copies: 0,
            push_include: compact_str::CompactString::default(),
            push_exclude: compact_str::CompactString::default(),
            push_database_backups: true,
            folder_per_server: true,
            shared_folder_id: compact_str::CompactString::default(),
        }
    }
}

impl GDriveSettingsData {
    /// True once the operator has actually filled in a client id and secret.
    #[inline]
    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
}

#[async_trait::async_trait]
impl SettingsSerializeExt for GDriveSettingsData {
    async fn serialize(
        &self,
        serializer: SettingsSerializer,
    ) -> Result<SettingsSerializer, anyhow::Error> {
        // Every field the deserializer reads has to be written here too, or it survives
        // only until the next save: `save()` persists exactly this list, and a key left
        // out reads back as its "never written" default forever. That is how the first
        // version shipped a `delete_copy` switch that could not be turned off.
        let serializer = serializer
            .write_raw_setting("client_id", self.client_id.clone())
            .write_raw_setting("folder_name", self.folder_name.clone())
            .write_raw_setting("auto_push", if self.auto_push { "true" } else { "false" })
            .write_raw_setting(
                "delete_copy",
                if self.delete_copy { "true" } else { "false" },
            )
            .write_raw_setting("keep_copies", self.keep_copies.to_string())
            .write_raw_setting("push_include", self.push_include.clone())
            .write_raw_setting("push_exclude", self.push_exclude.clone())
            .write_raw_setting(
                "push_database_backups",
                if self.push_database_backups {
                    "true"
                } else {
                    "false"
                },
            )
            .write_raw_setting(
                "folder_per_server",
                if self.folder_per_server {
                    "true"
                } else {
                    "false"
                },
            )
            .write_raw_setting("shared_folder_id", self.shared_folder_id.clone());

        // an empty secret means "not configured yet", so don't bother encrypting a blank
        if self.client_secret.is_empty() {
            Ok(serializer.write_raw_setting("client_secret", ""))
        } else {
            Ok(serializer
                .write_raw_encrypted_setting("client_secret", self.client_secret.clone())
                .await?)
        }
    }
}

pub struct GDriveSettingsDeserializer;

#[async_trait::async_trait]
impl SettingsDeserializeExt for GDriveSettingsDeserializer {
    async fn deserialize_boxed(
        &self,
        mut deserializer: SettingsDeserializer<'_>,
    ) -> Result<ExtensionSettings, anyhow::Error> {
        // every field falls back to a default: missing keys are normal on first
        // startup, and any time a new field is added without a migration.
        let client_secret = match deserializer.take_raw_setting("client_secret") {
            Some(raw) if raw.is_empty() => compact_str::CompactString::default(),
            Some(raw) => deserializer.database.decrypt_base64(&raw).await?,
            None => compact_str::CompactString::default(),
        };

        Ok(Box::new(GDriveSettingsData {
            client_id: deserializer
                .take_raw_setting("client_id")
                .unwrap_or_default(),
            client_secret,
            folder_name: deserializer
                .take_raw_setting("folder_name")
                .unwrap_or_else(|| "Calagopus Backups".into()),
            // Absent means "never written", which covers both a panel that predates the
            // switch and one restoring from backup - defaulting to the documented value
            // rather than to a silent opt-out.
            auto_push: deserializer
                .take_raw_setting("auto_push")
                .is_none_or(|raw| raw == "true"),
            delete_copy: deserializer
                .take_raw_setting("delete_copy")
                .is_none_or(|raw| raw == "true"),
            // Numbers parse leniently: an unparseable value (or a key restored from an
            // older backup) falls back to "keep everything" rather than to a prune.
            keep_copies: deserializer
                .take_raw_setting("keep_copies")
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(0),
            push_include: deserializer
                .take_raw_setting("push_include")
                .unwrap_or_default(),
            push_exclude: deserializer
                .take_raw_setting("push_exclude")
                .unwrap_or_default(),
            push_database_backups: deserializer
                .take_raw_setting("push_database_backups")
                .is_none_or(|raw| raw == "true"),
            folder_per_server: deserializer
                .take_raw_setting("folder_per_server")
                .is_none_or(|raw| raw == "true"),
            shared_folder_id: deserializer
                .take_raw_setting("shared_folder_id")
                .unwrap_or_default(),
        }))
    }
}

/// The extension's settings as they stand, or the documented defaults when nothing has
/// been written yet.
///
/// One read path for every caller that only needs a flag out of it - the two event
/// listeners - so "what does an unwritten setting mean" is answered in one place.
pub async fn current(state: &shared::State) -> Result<GDriveSettingsData, anyhow::Error> {
    let settings = state.settings.get().await?;

    // `get_extension_settings` hands back a borrow of the guard's contents, so the value
    // is cloned out before the guard drops - callers only ever want the flags.
    Ok(settings
        .get_extension_settings::<GDriveSettingsData>(crate::IDENTIFIER)
        .ok()
        .cloned()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an operator gets when nothing has ever been written for this extension -
    /// first startup, a fresh install, or a key added by a newer version.
    ///
    /// Both switches default on deliberately (see the field docs): an admin who entered
    /// OAuth credentials and a user who linked an account have already opted into
    /// archives leaving the panel, and a Drive delete is a recoverable trash.
    #[test]
    fn unwritten_settings_read_both_switches_as_on() {
        let defaults = GDriveSettingsData::default();

        assert!(defaults.auto_push, "auto push should default to on");
        assert!(defaults.delete_copy, "copy cleanup should default to on");
        assert_eq!(defaults.folder_name, "Calagopus Backups");

        // The feature switches added later ride the same "absent = documented default"
        // rule as the first two: database pushes and per-server folders default on, and
        // retention defaults to pruning nothing (`keep_copies = 0`).
        assert!(defaults.push_database_backups);
        assert!(defaults.folder_per_server);
        assert_eq!(defaults.keep_copies, 0);
        assert!(defaults.push_include.is_empty());
        assert!(defaults.push_exclude.is_empty());
        assert!(defaults.shared_folder_id.is_empty());
    }

    #[test]
    fn not_configured_until_both_oauth_values_are_present() {
        assert!(!GDriveSettingsData::default().is_configured());

        let with_id = GDriveSettingsData {
            client_id: "client-id".into(),
            ..Default::default()
        };
        assert!(
            !with_id.is_configured(),
            "a client id without a secret is not usable"
        );

        let fully_configured = GDriveSettingsData {
            client_secret: "client-secret".into(),
            ..with_id
        };
        assert!(fully_configured.is_configured());
    }

    /// The literal forms the serializer writes and the deserializer accepts. A flag saved
    /// as anything else reads back as its default, so the two sides have to agree.
    #[test]
    fn boolean_flags_round_trip_as_the_words_true_and_false() {
        let read = |raw: &str| Some(raw.to_string()).is_some_and(|raw| raw == "true");

        assert!(read("true"));
        assert!(!read("false"));
        // The absent-key case is the deserializer's `is_none_or(...)`; it has to land on
        // the documented default rather than on a silent opt-out.
        assert!(Option::<String>::None.is_none_or(|raw| raw == "true"));
    }
}
