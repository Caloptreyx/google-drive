//! Which servers push automatically - the include/exclude globs on the Configure page.
//!
//! Automatic means *automatic*: these rules gate the completion listener and the
//! reconcile sweep only. The Push button on the account page never consults them,
//! because an explicit click is an explicit decision and talking someone out of their
//! own intent is how "push this" ends up not pushing.
//!
//! Patterns are split on newlines, commas or semicolons; each pattern is a
//! case-insensitive glob where `*` matches any run of characters (including none) and
//! `?` matches exactly one. An empty include list means "every server"; exclude wins
//! over include when both match.

use crate::settings::GDriveSettingsData;

/// Parsed once per decision point from the two raw setting strings.
#[derive(Debug, Default, Clone)]
pub struct PushRules {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl PushRules {
    pub fn from_settings(settings: &GDriveSettingsData) -> Self {
        Self {
            include: split_patterns(&settings.push_include),
            exclude: split_patterns(&settings.push_exclude),
        }
    }

    /// Whether this server's backups should be queued when they finish.
    pub fn allows(&self, server_name: &str) -> bool {
        let included = self.include.is_empty()
            || self
                .include
                .iter()
                .any(|pattern| glob_match(pattern, server_name));

        included
            && !self
                .exclude
                .iter()
                .any(|pattern| glob_match(pattern, server_name))
    }
}

fn split_patterns(raw: &str) -> Vec<String> {
    raw.split([',', '\n', ';'])
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Case-insensitive glob match supporting `*` and `?`.
///
/// Iterative with a single backtrack point, the classic algorithm: remember where a
/// `*` last matched and retry from there when the tail fails. Linear in practice, and
/// immune to the exponential blowup a naive recursive matcher hits on `a*a*a*b`.
fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let value: Vec<char> = value.to_lowercase().chars().collect();

    let (mut p, mut v) = (0usize, 0usize);
    let (mut star, mut backtrack) = (None, 0usize);

    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            backtrack = v;
            p += 1;
        } else if let Some(star_at) = star {
            // The tail failed after a `*`: stretch that star by one character and
            // resume from where the tail last was.
            p = star_at + 1;
            backtrack += 1;
            v = backtrack;
        } else {
            return false;
        }
    }

    // Trailing `*`(s) match the empty remainder.
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }

    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(include: &str, exclude: &str) -> PushRules {
        PushRules {
            include: split_patterns(include),
            exclude: split_patterns(exclude),
        }
    }

    #[test]
    fn no_patterns_at_all_allows_everything() {
        let rules = rules("", "  \n , ");
        assert!(rules.allows("Goon"));
        assert!(rules.allows(""));
    }

    #[test]
    fn include_is_a_whitelist_when_present() {
        let rules = rules("prod-*, staging", "");
        assert!(rules.allows("prod-eu"));
        assert!(rules.allows("staging"));
        assert!(!rules.allows("dev"));
        // A prefix that merely *starts* like a pattern still needs the `*`.
        assert!(!rules.allows("prod"));
    }

    #[test]
    fn exclude_wins_over_a_matching_include() {
        let rules = rules("*", "staging-?");
        assert!(rules.allows("production"));
        assert!(!rules.allows("staging-1"));
        // `?` is exactly one character, not "one or more".
        assert!(rules.allows("staging-12"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let whitelist = rules("PROD-*", "");
        assert!(whitelist.allows("prod-eu"));
        assert!(whitelist.allows("Prod-EU"));

        let blacklist = rules("", "GoOn");
        assert!(!blacklist.allows("Goon"));
    }

    #[test]
    fn patterns_split_on_newlines_commas_and_semicolons() {
        let rules = rules("alpha\nbeta, gamma; delta", "");
        assert!(rules.allows("alpha"));
        assert!(rules.allows("beta"));
        assert!(rules.allows("gamma"));
        assert!(rules.allows("delta"));
        assert!(!rules.allows("epsilon"));
    }

    #[test]
    fn a_star_matches_the_empty_tail() {
        let rules = rules("goon*", "");
        assert!(rules.allows("goon"));
        assert!(rules.allows("goonlord"));
    }

    /// The pathological shape a naive recursive matcher turns exponential on.
    #[test]
    fn stars_do_not_blow_up_on_repeated_wildcards() {
        let rules = rules("a*a*a*a*a*b", "");
        assert!(!rules.allows(&"a".repeat(64)));
        assert!(rules.allows("aaaaaaaaab"));
    }
}
