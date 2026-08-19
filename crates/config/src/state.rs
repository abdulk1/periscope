//! What the sidebar looked like when the window last closed.
//!
//! This is **not** settings. `settings.toml` is written by a person, read once
//! at startup, and full of rules — which clusters refuse changes, which keys do
//! what — that the app must never quietly rewrite: a program that serialises a
//! config file back over itself destroys the comments and the ordering its
//! author put there, and races with the editor they still have it open in.
//! Which sections are expanded and which kinds were looked at last are not
//! decisions anybody wrote down; they are a record of a session, like
//! `audit.log`, so they live beside it in the data directory under a name that
//! says who writes it.
//!
//! Nothing here is a safety rule, which is what makes the failure handling the
//! opposite of settings': an unreadable or malformed state file starts a fresh
//! session rather than refusing to start, because the worst consequence is a
//! section that begins closed.
//!
//! What is recorded is deliberately narrow: kubeconfig **context names**, which
//! the audit log already records and the user chose, and kind names, which are
//! types rather than objects. No server URLs, no namespaces, no object names,
//! and never anything from a credential.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many visits are kept across every context.
///
/// One list rather than a map keyed by context, so recency is a property of the
/// list itself and there is a single number bounding the file. Thirty-two is
/// several days of switching around for anybody with a handful of clusters.
const RECENT_MEMORY: usize = 32;

/// One kind, looked at on one cluster.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visit {
    /// The kubeconfig context name.
    pub context: String,
    /// The kind, as `kubectl` names it: `pods`, `deployments.apps`.
    pub kind: String,
}

/// Remembered UI state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct UiState {
    /// Sidebar sections the user opened or closed, keyed by heading, holding
    /// only the ones they actually clicked — everything else follows the
    /// category's own default.
    pub sections: BTreeMap<String, bool>,
    /// Kinds visited, most recent first.
    pub recent: Vec<Visit>,
}

impl UiState {
    /// Whether a section was left open, if the user ever said.
    pub fn section(&self, heading: &str) -> Option<bool> {
        self.sections.get(heading).copied()
    }

    /// Records that a section is open or closed.
    pub fn set_section(&mut self, heading: impl Into<String>, open: bool) {
        self.sections.insert(heading.into(), open);
    }

    /// Records a kind having been opened on a cluster.
    ///
    /// Moves an existing entry to the front rather than adding a second one, so
    /// the list is a set ordered by recency and switching back and forth
    /// between two kinds cannot fill it.
    pub fn visited(&mut self, context: impl Into<String>, kind: impl Into<String>) {
        let visit = Visit {
            context: context.into(),
            kind: kind.into(),
        };

        self.recent.retain(|held| held != &visit);
        self.recent.insert(0, visit);
        self.recent.truncate(RECENT_MEMORY);
    }

    /// The kinds looked at on one cluster, most recent first.
    pub fn recent_kinds<'a>(&'a self, context: &str) -> impl Iterator<Item = &'a str> {
        // Owned so the iterator outlives the caller's name rather than
        // borrowing it: the sidebar builds this from a cluster id it is holding
        // for one frame.
        let context = context.to_owned();
        self.recent
            .iter()
            .filter(move |visit| visit.context == context)
            .map(|visit| visit.kind.as_str())
    }

    /// The last kind looked at on one cluster.
    pub fn last_kind(&self, context: &str) -> Option<&str> {
        self.recent_kinds(context).next()
    }
}

/// The file remembered state is kept in.
///
/// Holding the path rather than reading it from the environment on every write
/// is what lets a test point one at a temporary directory, and what stops the
/// app writing anything at all when it was never given one.
#[derive(Clone, Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// A state file at a specific path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The state file in the platform's data directory.
    pub fn open() -> Result<Self, std::io::Error> {
        let path =
            crate::paths::state_file().map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(Self::at(path))
    }

    /// Where state is written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads what was remembered, or nothing at all.
    ///
    /// A missing, unreadable or malformed file all mean the same thing here: a
    /// session that starts on the defaults. The reason is returned alongside so
    /// the caller can log it — silently starting fresh is fine, silently
    /// starting fresh *and* overwriting the file that would have explained why
    /// is not.
    pub fn read(&self) -> (UiState, Option<String>) {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (UiState::default(), None);
            }
            Err(error) => {
                return (
                    UiState::default(),
                    Some(format!("could not read {}: {error}", self.path.display())),
                );
            }
        };

        match toml::from_str(&text) {
            Ok(state) => (state, None),
            Err(error) => (
                UiState::default(),
                Some(format!("{} is not readable: {error}", self.path.display())),
            ),
        }
    }

    /// Writes the state, creating the directory if it is not there yet.
    ///
    /// Written whole on every change and not atomically: the file is a few
    /// hundred bytes, and a crash mid-write costs a session's worth of
    /// remembered sections, which [`StateFile::read`] already treats as "start
    /// fresh". Paying for a temporary file and a rename on every click of a
    /// heading would buy nothing worth having.
    pub fn write(&self, state: &UiState) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let body = toml::to_string(state).map_err(std::io::Error::other)?;
        std::fs::write(&self.path, format!("{HEADER}{body}"))?;
        crate::paths::restrict(&self.path)
    }
}

/// Says who writes the file, in the file, for whoever finds it.
const HEADER: &str = "# Written by Periscope. Settings live in settings.toml; this is\n\
                      # remembered session state and is safe to delete.\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that removes itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "periscope-state-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn file(&self) -> StateFile {
            StateFile::at(self.0.join("state.toml"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_section_the_user_never_touched_has_no_remembered_state() {
        let state = UiState::default();
        assert_eq!(state.section("STORAGE"), None);
    }

    #[test]
    fn the_most_recent_visit_comes_first() {
        let mut state = UiState::default();
        state.visited("prod", "pods");
        state.visited("prod", "deployments.apps");

        assert_eq!(state.last_kind("prod"), Some("deployments.apps"));
        assert_eq!(
            state.recent_kinds("prod").collect::<Vec<_>>(),
            ["deployments.apps", "pods"]
        );
    }

    #[test]
    fn visiting_a_kind_again_moves_it_rather_than_repeating_it() {
        let mut state = UiState::default();
        state.visited("prod", "pods");
        state.visited("prod", "secrets");
        state.visited("prod", "pods");

        assert_eq!(
            state.recent_kinds("prod").collect::<Vec<_>>(),
            ["pods", "secrets"]
        );
    }

    #[test]
    fn one_clusters_recents_are_not_anothers() {
        // Half the kinds on one cluster do not exist on the next, and a list of
        // things to jump to that jumps nowhere is worse than no list.
        let mut state = UiState::default();
        state.visited("prod", "certificates.cert-manager.io");
        state.visited("staging", "pods");

        assert_eq!(
            state.recent_kinds("prod").collect::<Vec<_>>(),
            ["certificates.cert-manager.io"]
        );
        assert_eq!(state.last_kind("staging"), Some("pods"));
        assert_eq!(state.last_kind("never-opened"), None);
    }

    #[test]
    fn the_recent_list_stops_growing() {
        let mut state = UiState::default();
        for index in 0..RECENT_MEMORY * 2 {
            state.visited("prod", format!("kind-{index}"));
        }

        assert_eq!(state.recent.len(), RECENT_MEMORY);
        assert_eq!(state.last_kind("prod"), Some("kind-63"));
    }

    #[test]
    fn state_survives_a_round_trip_through_the_file() {
        let dir = TempDir::new("round-trip");
        let file = dir.file();

        let mut state = UiState::default();
        state.set_section("STORAGE", true);
        state.set_section("WORKLOADS", false);
        state.visited("kind-periscope", "deployments.apps");
        file.write(&state).expect("written");

        let (read, problem) = file.read();
        assert_eq!(read, state);
        assert!(problem.is_none(), "{problem:?}");
    }

    #[test]
    fn a_missing_file_is_a_fresh_session_rather_than_a_problem() {
        let file = StateFile::at("/nonexistent/periscope/state.toml");
        let (state, problem) = file.read();

        assert_eq!(state, UiState::default());
        assert!(problem.is_none());
    }

    #[test]
    fn a_malformed_file_starts_fresh_and_says_why() {
        // The opposite of settings.toml, which refuses to start: nothing here
        // is a rule anybody is relying on, so losing it is not worth a failure.
        let dir = TempDir::new("malformed");
        let file = dir.file();
        std::fs::write(file.path(), "[sections\nWORKLOADS = ").expect("fixture written");

        let (state, problem) = file.read();
        assert_eq!(state, UiState::default());
        assert!(
            problem.is_some_and(|problem| problem.contains("state.toml")),
            "a malformed file must say which one it was"
        );
    }

    #[test]
    fn the_file_says_who_writes_it() {
        let dir = TempDir::new("header");
        let file = dir.file();
        file.write(&UiState::default()).expect("written");

        let text = std::fs::read_to_string(file.path()).expect("readable");
        assert!(text.contains("Written by Periscope"), "{text}");
        assert!(text.contains("settings.toml"), "{text}");
    }

    #[test]
    fn writing_creates_the_directory() {
        let dir = TempDir::new("nested");
        let file = StateFile::at(dir.0.join("deeper").join("state.toml"));

        file.write(&UiState::default()).expect("written");
        assert!(file.path().exists());
    }
}
