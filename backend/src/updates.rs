//! Noticing a newer release of this extension.
//!
//! The Panel calls `check_for_updates` on startup, every 12 hours, and whenever an admin
//! presses *Recheck for Updates*, and lists whatever comes back under Outdated Extensions
//! on Admin -> Home -> Updates, next to panel-core and node updates. This extension is
//! distributed as a GitHub release zip, so the releases feed is the whole update channel:
//! fetch it, fold everything newer than the running version into a changelog, and stay
//! quiet when there is nothing newer.
//!
//! The feed is cached for an hour because GitHub allows only 60 unauthenticated requests
//! per hour per IP, while the Panel asks on every startup, every 12 hours, and per
//! recheck - a restart loop alone could otherwise spend the quota.

use serde::{Deserialize, Serialize};
use shared::{State, extensions::ExtensionUpdateInfo};

/// The release list rather than `releases/latest`: when a panel is several versions
/// behind, the changelog must cover every release in between, which only the list gives.
/// Drafts are never returned to unauthenticated callers, so an unfinished release cannot
/// be advertised here either.
const RELEASES_URL: &str =
    "https://api.github.com/repos/Caloptreyx/google-drive/releases?per_page=20";

const CACHE_KEY: &str = "dev.caloptreyx.gdrive::releases";
const CACHE_TTL_SECONDS: u64 = 60 * 60;

/// One entry of the GitHub releases feed. Any other field (`url`, `id`, `assets`, ...)
/// is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub body: Option<String>,
}

/// Ask GitHub (through the cache) whether something newer exists than the running one.
pub async fn check(
    state: &State,
    current_version: &semver::Version,
) -> Result<Option<ExtensionUpdateInfo>, anyhow::Error> {
    let releases = fetch_releases(state).await?;

    Ok(update_from_releases(&releases, current_version))
}

/// Download the releases feed, cached for an hour under this extension's own key.
async fn fetch_releases(state: &State) -> Result<Vec<Release>, anyhow::Error> {
    state
        .cache
        .cached(CACHE_KEY, CACHE_TTL_SECONDS, || async {
            let response = state.client.get(RELEASES_URL).send().await?;

            // A repository that does not exist (or is not public yet) answers 404. There
            // is nothing to offer and nothing an admin can fix from the panel, so cache
            // an empty feed instead of parking a failed check on the updates page every
            // cycle. Any other failure (rate limit, outage) still surfaces as an error.
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }

            response.error_for_status()?.json::<Vec<Release>>().await
        })
        .await
}

/// Fold the feed into an update notice, or `None` when `current_version` is already the
/// newest thing out there.
///
/// Deliberately tolerant: pre-releases and tags that are not semver are skipped rather
/// than failing the check, because one careless tag would otherwise park an error on the
/// admin updates page until someone notices. A release with an empty body still counts -
/// the Panel then shows the new version with no changelog list, which its docs call out
/// as perfectly fine.
pub fn update_from_releases(
    releases: &[Release],
    current_version: &semver::Version,
) -> Option<ExtensionUpdateInfo> {
    let mut newer: Vec<(semver::Version, &Release)> = releases
        .iter()
        .filter(|release| !release.prerelease)
        .filter_map(|release| {
            let version = semver::Version::parse(release.tag_name.trim_start_matches('v')).ok()?;

            // A semver pre-release (v2.0.0-rc.1) is not something to offer even when the
            // GitHub pre-release flag was left off.
            if version.pre.is_empty() {
                Some((version, release))
            } else {
                None
            }
        })
        .filter(|(version, _)| version > current_version)
        .collect();

    if newer.is_empty() {
        return None;
    }

    // Newest first regardless of feed order: GitHub sorts the feed by creation date,
    // which does not have to match version order when a release is published late.
    newer.sort_by(|a, b| b.0.cmp(&a.0));

    let version = newer[0].0.clone();
    let changes = newer
        .iter()
        .flat_map(|(release_version, release)| {
            release
                .body
                .as_deref()
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(move |line| compact_str::format_compact!("{release_version}: {line}"))
        })
        .collect();

    Some(ExtensionUpdateInfo { version, changes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(raw: &str) -> semver::Version {
        semver::Version::parse(raw).expect("test version parses")
    }

    fn release(tag: &str, prerelease: bool, body: Option<&str>) -> Release {
        Release {
            tag_name: tag.to_string(),
            prerelease,
            body: body.map(str::to_string),
        }
    }

    fn changelog(info: &ExtensionUpdateInfo) -> Vec<String> {
        info.changes.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn nothing_is_offered_when_the_installed_version_is_newest_or_ahead() {
        let releases = [release("v1.0.0", false, Some("old news"))];

        assert!(update_from_releases(&releases, &version("1.0.0")).is_none());
        assert!(update_from_releases(&releases, &version("1.1.0")).is_none());
        assert!(update_from_releases(&[], &version("0.1.0")).is_none());
    }

    #[test]
    fn every_release_newer_than_installed_is_folded_into_the_changelog() {
        let releases = [
            release("v1.3.0", false, Some("third\nand a second line")),
            release("v1.2.0", false, Some("second")),
            release("v1.0.0", false, Some("already installed")),
        ];

        let update = update_from_releases(&releases, &version("1.0.0")).expect("an update");

        assert_eq!(update.version, version("1.3.0"));
        assert_eq!(
            changelog(&update),
            vec!["1.3.0: third", "1.3.0: and a second line", "1.2.0: second",]
        );
    }

    #[test]
    fn changelog_order_follows_version_not_feed_order() {
        // The feed is ordered by creation date; a back-dated release must not invert it.
        let releases = [
            release("v1.1.0", false, Some("first")),
            release("v1.2.0", false, Some("second")),
        ];

        let update = update_from_releases(&releases, &version("1.0.0")).expect("an update");

        assert_eq!(update.version, version("1.2.0"));
        assert_eq!(changelog(&update), vec!["1.2.0: second", "1.1.0: first"]);
    }

    #[test]
    fn pre_releases_are_never_offered() {
        let flagged = [
            release("v2.0.0", true, Some("beta notes")),
            release("v1.1.0", false, Some("stable")),
        ];
        assert_eq!(
            update_from_releases(&flagged, &version("1.0.0"))
                .expect("the stable update")
                .version,
            version("1.1.0")
        );

        // A semver pre-release with the GitHub flag left off is still a pre-release.
        let unflagged = [
            release("v2.0.0-rc.1", false, Some("rc notes")),
            release("v1.1.0", false, Some("stable")),
        ];
        assert_eq!(
            update_from_releases(&unflagged, &version("1.0.0"))
                .expect("the stable update")
                .version,
            version("1.1.0")
        );
    }

    #[test]
    fn tags_that_are_not_semver_are_skipped_instead_of_failing_the_check() {
        let releases = [
            release("latest", false, Some("junk")),
            release("1.1.0", false, Some("no v prefix, still fine")),
        ];

        let update = update_from_releases(&releases, &version("1.0.0")).expect("an update");

        assert_eq!(update.version, version("1.1.0"));
        assert_eq!(changelog(&update), vec!["1.1.0: no v prefix, still fine"]);
    }

    #[test]
    fn a_release_without_a_body_still_shows_the_new_version() {
        let releases = [release("v1.1.0", false, None)];

        let update = update_from_releases(&releases, &version("1.0.0")).expect("an update");

        assert_eq!(update.version, version("1.1.0"));
        assert!(update.changes.is_empty());
    }

    #[test]
    fn a_real_github_payload_parses() {
        let json = r#"[{
            "url": "https://api.github.com/repos/Caloptreyx/google-drive/releases/42",
            "tag_name": "v1.1.0",
            "prerelease": false,
            "draft": false,
            "body": "Fixed it.\n\n- one\n- two",
            "assets": []
        }]"#;

        let releases: Vec<Release> = serde_json::from_str(json).expect("payload parses");

        let update = update_from_releases(&releases, &version("1.0.0")).expect("an update");

        assert_eq!(update.version, version("1.1.0"));
        assert_eq!(
            changelog(&update),
            vec!["1.1.0: Fixed it.", "1.1.0: - one", "1.1.0: - two"]
        );
    }
}
