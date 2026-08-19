//! Checking whether a newer Periscope exists.
//!
//! The only network call this application makes that is not to a cluster the
//! user configured, and it is **off unless asked for**. Nothing is downloaded
//! and nothing is installed: the answer is a sentence on screen with a link.
//!
//! Everything here is pure — a version comparison and a JSON parse — so the
//! decision "is this newer" is testable without a network, and the code that
//! does the request has nothing in it worth testing.

use serde::{Deserialize, Serialize};

/// GitHub's releases endpoint for the repository this is built from.
const DEFAULT_ENDPOINT: &str = "https://api.github.com/repos/abdulk1/periscope/releases/latest";

/// Whether to look for a newer version, and where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Updates {
    /// Off by default. `IMPLEMENTATION.md` §"Security posture" allows exactly
    /// one non-cluster network call and requires it to be opt-in; this is that
    /// opt-in, and nothing else in the app talks to anything but a cluster.
    pub check: bool,
    /// A GitHub releases API URL.
    pub endpoint: String,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            check: false,
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }
}

impl Updates {
    /// Whether a check should be made, and to where.
    ///
    /// `None` when it is switched off, which is the case a caller must handle
    /// first and the reason this returns an option rather than a bool.
    pub fn target(&self) -> Option<&str> {
        let endpoint = self.endpoint.trim();
        // An `https` scheme is required rather than assumed: an update check
        // that would follow a plain-http URL is a downgrade waiting to happen.
        (self.check && endpoint.starts_with("https://")).then_some(endpoint)
    }

    /// Why a check that is switched on will not happen, if it will not.
    pub fn problem(&self) -> Option<String> {
        if !self.check || self.target().is_some() {
            return None;
        }
        Some(format!(
            "`updates.endpoint` must be an https URL, not `{}`",
            self.endpoint.trim()
        ))
    }
}

/// A three-part version.
///
/// Deliberately not a full semver implementation: a pre-release suffix is
/// recorded but never makes one version newer than another, because "0.2.0-rc1
/// is available" is not something to tell somebody running 0.2.0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Breaking changes.
    pub major: u32,
    /// Features.
    pub minor: u32,
    /// Fixes.
    pub patch: u32,
}

impl Version {
    /// The version this binary was built as.
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).unwrap_or_default()
    }

    /// Parses `1.2.3`, `v1.2.3`, or either with a `-suffix` that is ignored.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches(['v', 'V']);
        let text = text.split(['-', '+']).next()?;

        let mut parts = text.split('.');
        let mut next = || parts.next()?.parse().ok();
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next().unwrap_or(0),
        };

        // A fourth part means this is not a version we understand.
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A published release, as the UI would mention it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Which version.
    pub version: Version,
    /// Where a person can read about it.
    pub url: String,
}

impl Release {
    /// The sentence shown when there is something newer.
    pub fn notice(&self, current: Version) -> String {
        format!(
            "Periscope {} is available — you are running {}. {}",
            self.version, current, self.url
        )
    }
}

/// What GitHub's releases API answers with. Only the fields that matter here.
#[derive(Deserialize)]
struct Payload {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The release worth telling the user about, if there is one.
///
/// Drafts and pre-releases are not: they are not for the person running a
/// stable build, and an update notice that points at one is noise.
pub fn newer_than(current: Version, payload: &str) -> Result<Option<Release>, String> {
    let release: Payload =
        serde_json::from_str(payload).map_err(|error| format!("unreadable answer: {error}"))?;

    if release.draft || release.prerelease {
        return Ok(None);
    }

    let version = Version::parse(&release.tag_name)
        .ok_or_else(|| format!("`{}` is not a version", release.tag_name))?;

    Ok((version > current).then_some(Release {
        version,
        url: release.html_url,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).expect("a version")
    }

    #[test]
    fn versions_parse_with_or_without_a_v() {
        assert_eq!(version("1.2.3"), version("v1.2.3"));
        assert_eq!(version("0.1.0").minor, 1);
        // A missing patch is zero, which is how people write major releases.
        assert_eq!(version("2.0"), version("2.0.0"));
    }

    #[test]
    fn nonsense_is_not_a_version() {
        for text in ["", "latest", "1", "1.2.3.4", "v.1.2", "one.two.three"] {
            assert!(Version::parse(text).is_none(), "{text} parsed");
        }
    }

    #[test]
    fn versions_order_by_number_not_by_string() {
        // The bug this prevents: "0.10.0" < "0.9.0" when compared as text.
        assert!(version("0.10.0") > version("0.9.0"));
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("1.2.3") == version("1.2.3"));
    }

    #[test]
    fn a_pre_release_suffix_is_dropped_rather_than_ranked() {
        assert_eq!(version("1.2.3-rc.1"), version("1.2.3"));
    }

    fn payload(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://example.invalid/{tag}",
               "draft":false,"prerelease":false,"name":"{tag}"}}"#
        )
    }

    #[test]
    fn a_newer_release_is_reported_with_its_link() {
        let found = newer_than(version("0.1.0"), &payload("v0.2.0"))
            .expect("parses")
            .expect("newer");

        assert_eq!(found.version, version("0.2.0"));
        assert_eq!(found.url, "https://example.invalid/v0.2.0");
        let notice = found.notice(version("0.1.0"));
        assert!(notice.contains("0.2.0"), "{notice}");
        assert!(notice.contains("0.1.0"), "{notice}");
    }

    #[test]
    fn the_same_or_an_older_release_is_not_news() {
        assert_eq!(newer_than(version("1.0.0"), &payload("v1.0.0")), Ok(None));
        assert_eq!(newer_than(version("1.0.0"), &payload("v0.9.0")), Ok(None));
    }

    #[test]
    fn drafts_and_pre_releases_are_never_offered() {
        // Somebody running a stable build is not the audience for either.
        let draft = r#"{"tag_name":"v9.0.0","html_url":"u","draft":true,"prerelease":false}"#;
        let pre = r#"{"tag_name":"v9.0.0","html_url":"u","draft":false,"prerelease":true}"#;

        assert_eq!(newer_than(version("1.0.0"), draft), Ok(None));
        assert_eq!(newer_than(version("1.0.0"), pre), Ok(None));
    }

    #[test]
    fn an_unreadable_answer_is_an_error_rather_than_a_silent_no() {
        assert!(newer_than(version("1.0.0"), "not json").is_err());
        assert!(newer_than(version("1.0.0"), &payload("latest")).is_err());
    }

    #[test]
    fn checking_is_off_until_it_is_asked_for() {
        let updates = Updates::default();

        assert!(!updates.check);
        assert_eq!(updates.target(), None);
        assert_eq!(updates.problem(), None);
    }

    #[test]
    fn a_check_that_is_on_names_an_https_endpoint() {
        let updates = Updates {
            check: true,
            ..Updates::default()
        };
        assert_eq!(updates.target(), Some(DEFAULT_ENDPOINT));
    }

    #[test]
    fn a_plain_http_endpoint_is_refused_and_says_so() {
        // Following one would turn an update check into a downgrade attack.
        let updates = Updates {
            check: true,
            endpoint: "http://example.invalid/latest".to_owned(),
        };

        assert_eq!(updates.target(), None);
        assert!(updates.problem().is_some_and(|why| why.contains("https")));
    }

    #[test]
    fn the_current_version_is_the_one_this_was_built_as() {
        assert_eq!(Version::current(), version(env!("CARGO_PKG_VERSION")));
    }
}
