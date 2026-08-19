//! User-facing application settings.
//!
//! Read from `settings.toml` in the platform's config directory. The file is
//! optional: a missing one means defaults, which is not an error. A *malformed*
//! one is an error, and it is surfaced rather than swallowed — silently running
//! with defaults when someone has written a read-only rule they are relying on
//! would be the worst possible failure of this module.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which appearance to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// Follow the operating system.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl ThemeChoice {
    /// The next choice in the cycle, for a toggle control.
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    /// Human-readable name for UI chrome.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// Which contexts may be changed, and which may only be read.
///
/// Two knobs rather than one because both directions are real: most people want
/// to name the two clusters that must never be touched, and some want the
/// opposite — everything read-only except a scratch cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Access {
    /// Contexts that refuse every mutation.
    pub read_only: BTreeSet<String>,
    /// When set, every context refuses mutations unless it is in `writable`.
    pub read_only_by_default: bool,
    /// Contexts that may be changed even when `read-only-by-default` is set.
    pub writable: BTreeSet<String>,
}

impl Access {
    /// Whether a context may be mutated.
    ///
    /// An explicit `read-only` entry always wins: if someone has written a
    /// cluster's name in the list of things not to touch, no other setting may
    /// override it.
    pub fn may_mutate(&self, context: &str) -> bool {
        if self.read_only.contains(context) {
            return false;
        }
        if self.read_only_by_default {
            return self.writable.contains(context);
        }
        true
    }

    /// Marks a context read-only, for tests and for a future UI toggle.
    pub fn deny(&mut self, context: impl Into<String>) {
        let context = context.into();
        self.writable.remove(&context);
        self.read_only.insert(context);
    }
}

/// A span of time, written the way a person would write it.
///
/// `"5m"`, `"30s"`, `"1h"` — or a bare number, which is seconds. A duration in
/// a config file is read far more often than it is written, and `300` is not a
/// thing anyone recognises as five minutes at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span(pub Duration);

impl Span {
    /// The duration itself.
    pub fn get(self) -> Duration {
        self.0
    }

    /// Parses `5m`, `30s`, `1h`, or a bare count of seconds.
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (value, multiplier) = match text.chars().last() {
            Some('s') => (&text[..text.len() - 1], 1),
            Some('m') => (&text[..text.len() - 1], 60),
            Some('h') => (&text[..text.len() - 1], 3_600),
            // No suffix means seconds, which is what every other tool does.
            Some(digit) if digit.is_ascii_digit() => (text, 1),
            _ => {
                return Err(format!(
                    "`{text}` is not a duration, e.g. `5m`, `30s`, `1h`"
                ));
            }
        };

        let seconds: u64 = value
            .trim()
            .parse()
            .map_err(|_| format!("`{text}` is not a duration, e.g. `5m`, `30s`, `1h`"))?;
        Ok(Self(Duration::from_secs(seconds * multiplier)))
    }

    /// How it is written back out.
    fn render(self) -> String {
        let seconds = self.0.as_secs();
        match seconds {
            0 => "0s".to_owned(),
            _ if seconds.is_multiple_of(3_600) => format!("{}h", seconds / 3_600),
            _ if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
            _ => format!("{seconds}s"),
        }
    }
}

impl Serialize for Span {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.render())
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Either form is accepted: `"5m"` or `300`.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Text(String),
            Seconds(u64),
        }

        match Written::deserialize(deserializer)? {
            Written::Text(text) => Self::parse(&text).map_err(serde::de::Error::custom),
            Written::Seconds(seconds) => Ok(Self(Duration::from_secs(seconds))),
        }
    }
}

/// How long a cluster nobody is looking at keeps streaming.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Rows one cluster may hold before its unviewed tables are released.
const DEFAULT_ROW_BUDGET: usize = 200_000;

/// Lines a log or command buffer keeps before dropping the oldest.
const DEFAULT_LOG_BUFFER: usize = 100_000;

/// The numbers that used to be constants in the code.
///
/// All three are about the same thing — how much of a cluster this process is
/// willing to hold on to — and all three are workload-dependent enough that
/// somebody running one enormous cluster and somebody running thirty small ones
/// want different answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Limits {
    /// How long a cluster stays warm after its last pane closes.
    pub idle_timeout: Span,
    /// Rows one cluster may hold before its unviewed tables are released.
    pub row_budget: usize,
    /// Lines a log or command buffer keeps before dropping the oldest.
    pub log_buffer: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle_timeout: Span(DEFAULT_IDLE_TIMEOUT),
            row_budget: DEFAULT_ROW_BUDGET,
            log_buffer: DEFAULT_LOG_BUFFER,
        }
    }
}

impl Limits {
    /// Rejects values that would leave the app unable to do its job.
    ///
    /// Zero is the only thing refused: a zero idle timeout releases a cluster
    /// the instant it leaves the screen, a zero row budget holds nothing, and a
    /// zero log buffer drops every line as it arrives. None of those are
    /// configurations, they are ways to make the app look broken.
    fn validate(&self) -> Result<(), String> {
        if self.idle_timeout.get().is_zero() {
            return Err("`limits.idle-timeout` must be more than zero".to_owned());
        }
        if self.row_budget == 0 {
            return Err("`limits.row-budget` must be more than zero".to_owned());
        }
        if self.log_buffer == 0 {
            return Err("`limits.log-buffer` must be more than zero".to_owned());
        }
        Ok(())
    }
}

/// Something a key can be bound to.
///
/// Deliberately a closed set rather than free-form strings: a keymap that
/// silently ignores a line it does not understand is the worst kind of config,
/// because the only symptom is a key that does nothing. A misspelled command
/// name is refused, and the error names what was expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Command {
    /// Open or close the jump palette.
    Palette,
    /// Close whatever is on top.
    Dismiss,
    /// Tail logs for the selection.
    Logs,
    /// Stick the log view to the newest line, or stop.
    Follow,
    /// Show two clusters side by side, or go back to one.
    Split,
    /// Move down the palette's results.
    Next,
    /// Move up the palette's results.
    Previous,
    /// Take the highlighted palette result.
    Confirm,
    /// Move down the table.
    RowDown,
    /// Move up the table.
    RowUp,
    /// Jump to the first row.
    RowTop,
    /// Jump to the last row.
    RowBottom,
    /// Move half a screen down the table.
    HalfPageDown,
    /// Move half a screen up the table.
    HalfPageUp,
    /// Open the row the keyboard is on.
    OpenRow,
}

impl Command {
    /// Every command, so the UI can bind them all.
    pub const ALL: [Self; 15] = [
        Self::Palette,
        Self::Dismiss,
        Self::Logs,
        Self::Follow,
        Self::Split,
        Self::Next,
        Self::Previous,
        Self::Confirm,
        Self::RowDown,
        Self::RowUp,
        Self::RowTop,
        Self::RowBottom,
        Self::HalfPageDown,
        Self::HalfPageUp,
        Self::OpenRow,
    ];

    /// How it is written in `settings.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Palette => "palette",
            Self::Dismiss => "dismiss",
            Self::Logs => "logs",
            Self::Follow => "follow",
            Self::Split => "split",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Confirm => "confirm",
            Self::RowDown => "row-down",
            Self::RowUp => "row-up",
            Self::RowTop => "row-top",
            Self::RowBottom => "row-bottom",
            Self::HalfPageDown => "half-page-down",
            Self::HalfPageUp => "half-page-up",
            Self::OpenRow => "open-row",
        }
    }

    /// The keys this command answers to unless the user says otherwise.
    ///
    /// The single-letter ones are k9s's: `:` opens the jump prompt, `l` tails
    /// logs, `q` goes back. They are bound only where no text field has focus,
    /// so typing `l` into a namespace filter types an `l`.
    pub fn default_keys(self) -> &'static [&'static str] {
        match self {
            Self::Palette => &["cmd-k", "ctrl-k", ":"],
            Self::Dismiss => &["escape", "q"],
            Self::Logs => &["cmd-l", "ctrl-l", "l"],
            Self::Follow => &["cmd-shift-f"],
            Self::Split => &["cmd-\\", "ctrl-\\"],
            // Inside the palette a text field always has focus, so these are
            // the arrows and the readline pair — never `j` and `k`, which are
            // letters somebody is trying to type.
            Self::Next => &["down", "ctrl-n"],
            Self::Previous => &["up", "ctrl-p"],
            Self::Confirm => &["enter"],
            // The table's own navigation: vim's letters and the arrows, which
            // is what both halves of the audience reach for. `enter` opens,
            // as it does everywhere else.
            Self::RowDown => &["j", "down"],
            Self::RowUp => &["k", "up"],
            Self::RowTop => &["g", "home"],
            Self::RowBottom => &["shift-g", "end"],
            // vim's, and k9s's, half-screen pair. They are also text-editing
            // keys — `ctrl-u` clears a line in every readline there has ever
            // been — so they are bound outside text fields even though they
            // carry a modifier; see `context_of`.
            Self::HalfPageDown => &["ctrl-d"],
            Self::HalfPageUp => &["ctrl-u"],
            Self::OpenRow => &["enter"],
        }
    }
}

/// Which keys do what.
///
/// A command named in the file **replaces** its defaults rather than adding to
/// them, and an empty list unbinds it. A command that is not named keeps its
/// defaults, so remapping one key does not silently drop the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keys(std::collections::BTreeMap<Command, Vec<String>>);

impl Keys {
    /// The keys bound to a command: what the user wrote, or the defaults.
    pub fn keys(&self, command: Command) -> Vec<String> {
        match self.0.get(&command) {
            Some(keys) => keys.clone(),
            None => command
                .default_keys()
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
        }
    }

    /// Every command and the keys it answers to.
    pub fn bindings(&self) -> impl Iterator<Item = (Command, Vec<String>)> + '_ {
        Command::ALL
            .into_iter()
            .map(move |command| (command, self.keys(command)))
    }

    /// Binds a command, for tests.
    pub fn bind(&mut self, command: Command, keys: impl IntoIterator<Item: Into<String>>) {
        self.0
            .insert(command, keys.into_iter().map(Into::into).collect());
    }
}

/// Which columns a kind shows, by kind.
///
/// Keyed the way `kubectl` names a kind — `pods`, `deployments.apps` — and
/// valued with column headings as the table prints them, matched without regard
/// to case. A kind that is not mentioned shows everything it always did.
///
/// `NAMESPACE`, `NAME` and `AGE` are structural and always shown; this chooses
/// among the columns that are specific to the kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Columns(std::collections::BTreeMap<String, Vec<String>>);

impl Columns {
    /// The columns wanted for a kind, if the user chose any.
    pub fn wanted(&self, kind: &str) -> Option<&[String]> {
        self.0.get(kind).map(Vec::as_slice)
    }

    /// Chooses columns for a kind, for tests.
    pub fn choose(
        &mut self,
        kind: impl Into<String>,
        columns: impl IntoIterator<Item: Into<String>>,
    ) {
        self.0
            .insert(kind.into(), columns.into_iter().map(Into::into).collect());
    }
}

/// Everything the user can configure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// Appearance.
    pub theme: ThemeChoice,
    /// Which clusters may be changed.
    pub access: Access,
    /// How much this process holds on to.
    pub limits: Limits,
    /// Which keys do what.
    pub keys: Keys,
    /// Which columns each kind shows.
    pub columns: Columns,
    /// Whether to look for a newer Periscope.
    pub updates: crate::updates::Updates,
}

/// Why settings could not be read.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file exists but could not be read.
    #[error("could not read {path}: {source}")]
    Read {
        /// Which file.
        path: String,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The file exists but is not valid TOML, or does not match the schema.
    #[error("{path} is not valid settings: {source}")]
    Parse {
        /// Which file.
        path: String,
        /// What went wrong.
        #[source]
        source: toml::de::Error,
    },
    /// The file parsed, but a value in it cannot be honoured.
    #[error("{path}: {reason}")]
    Invalid {
        /// Which file.
        path: String,
        /// What is wrong with it, and what would be acceptable.
        reason: String,
    },
}

impl Settings {
    /// Reads settings from a path, or returns defaults when there is no file.
    pub fn read_from(path: &Path) -> Result<Self, SettingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // No file is the normal case, not a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(SettingsError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        let settings: Self = toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.display().to_string(),
            source,
        })?;

        settings
            .limits
            .validate()
            .map_err(|reason| SettingsError::Invalid {
                path: path.display().to_string(),
                reason,
            })?;

        Ok(settings)
    }

    /// Reads settings from the platform's config directory.
    pub fn read() -> Result<Self, SettingsError> {
        match crate::paths::settings_file() {
            Ok(path) => Self::read_from(&path),
            // No home directory: defaults are the only sensible answer, and the
            // app has bigger problems to report than this one.
            Err(_) => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(text: &str) -> (tempdir::TempDir, std::path::PathBuf) {
        let dir = tempdir::TempDir::new();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, text).expect("fixture written");
        (dir, path)
    }

    /// A directory that removes itself.
    mod tempdir {
        pub struct TempDir(std::path::PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "periscope-settings-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }

            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn theme_cycles_back_to_system() {
        let mut theme = ThemeChoice::default();
        assert_eq!(theme, ThemeChoice::System);
        theme = theme.next();
        assert_eq!(theme, ThemeChoice::Light);
        theme = theme.next();
        assert_eq!(theme, ThemeChoice::Dark);
        theme = theme.next();
        assert_eq!(theme, ThemeChoice::System);
    }

    #[test]
    fn everything_is_writable_by_default() {
        // Periscope is read-only until Phase 5 regardless; this is about what
        // the *settings* say when nobody has said anything.
        assert!(Access::default().may_mutate("prod"));
    }

    #[test]
    fn a_context_named_read_only_refuses_mutations() {
        let access = Access {
            read_only: ["prod".to_owned()].into_iter().collect(),
            ..Access::default()
        };

        assert!(!access.may_mutate("prod"));
        assert!(access.may_mutate("staging"));
    }

    #[test]
    fn read_only_by_default_inverts_the_rule() {
        let access = Access {
            read_only_by_default: true,
            writable: ["scratch".to_owned()].into_iter().collect(),
            ..Access::default()
        };

        assert!(access.may_mutate("scratch"));
        assert!(!access.may_mutate("prod"));
        assert!(!access.may_mutate("anything-else"));
    }

    #[test]
    fn an_explicit_read_only_entry_beats_writable() {
        // Otherwise a stale `writable` entry could quietly re-arm a cluster
        // somebody deliberately locked.
        let access = Access {
            read_only: ["prod".to_owned()].into_iter().collect(),
            read_only_by_default: true,
            writable: ["prod".to_owned()].into_iter().collect(),
        };

        assert!(!access.may_mutate("prod"));
    }

    #[test]
    fn settings_are_read_from_toml() {
        let (_dir, path) = write(
            r#"
theme = "dark"

[access]
read-only = ["prod", "prod-eu"]
read-only-by-default = false
writable = []
"#,
        );

        let settings = Settings::read_from(&path).expect("parses");
        assert_eq!(settings.theme, ThemeChoice::Dark);
        assert!(!settings.access.may_mutate("prod"));
        assert!(settings.access.may_mutate("kind-local"));
    }

    #[test]
    fn a_missing_file_means_defaults_rather_than_an_error() {
        let settings = Settings::read_from(std::path::Path::new("/nonexistent/settings.toml"))
            .expect("a missing file is not a failure");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn a_malformed_file_is_an_error_rather_than_silent_defaults() {
        // Falling back to defaults here would mean ignoring a read-only rule
        // somebody is relying on.
        let (_dir, path) = write("theme = \"dark\"\n[access\nread-only = [");
        let error = Settings::read_from(&path).expect_err("malformed settings are an error");
        assert!(error.to_string().contains("settings.toml"), "{error}");
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_fatal() {
        // A settings file written by a newer version must not stop this one
        // from starting.
        let (_dir, path) = write("theme = \"light\"\nfuture-option = 42\n");
        let settings = Settings::read_from(&path).expect("parses");
        assert_eq!(settings.theme, ThemeChoice::Light);
    }

    #[test]
    fn durations_are_written_the_way_people_write_them() {
        assert_eq!(Span::parse("5m").expect("parses").get().as_secs(), 300);
        assert_eq!(Span::parse("30s").expect("parses").get().as_secs(), 30);
        assert_eq!(Span::parse("1h").expect("parses").get().as_secs(), 3_600);
        // A bare number is seconds, as it is everywhere else.
        assert_eq!(Span::parse("90").expect("parses").get().as_secs(), 90);
    }

    #[test]
    fn a_duration_that_is_not_one_says_what_a_duration_looks_like() {
        for text in ["soon", "5 minutes", "m", "-1", "5d"] {
            let error = Span::parse(text).expect_err("{text} is not a duration");
            assert!(error.contains("5m"), "{text}: {error}");
        }
    }

    #[test]
    fn durations_survive_a_round_trip() {
        for text in ["30s", "5m", "2h"] {
            let span = Span::parse(text).expect("parses");
            assert_eq!(span.render(), text);
        }
        // Anything that is not a whole number of minutes stays in seconds.
        assert_eq!(Span(Duration::from_secs(90)).render(), "90s");
    }

    #[test]
    fn limits_are_read_from_toml() {
        let (_dir, path) = write(
            r#"
[limits]
idle-timeout = "90s"
row-budget = 50000
log-buffer = 5000
"#,
        );

        let limits = Settings::read_from(&path).expect("parses").limits;
        assert_eq!(limits.idle_timeout.get().as_secs(), 90);
        assert_eq!(limits.row_budget, 50_000);
        assert_eq!(limits.log_buffer, 5_000);
    }

    #[test]
    fn a_partial_limits_table_keeps_the_defaults_for_the_rest() {
        let (_dir, path) = write("[limits]\nrow-budget = 1000\n");

        let limits = Settings::read_from(&path).expect("parses").limits;
        assert_eq!(limits.row_budget, 1_000);
        assert_eq!(limits.idle_timeout, Limits::default().idle_timeout);
        assert_eq!(limits.log_buffer, Limits::default().log_buffer);
    }

    #[test]
    fn a_zero_limit_is_refused_rather_than_making_the_app_look_broken() {
        for (key, value) in [
            ("idle-timeout", "\"0s\""),
            ("row-budget", "0"),
            ("log-buffer", "0"),
        ] {
            let (_dir, path) = write(&format!("[limits]\n{key} = {value}\n"));
            let error = Settings::read_from(&path).expect_err("{key} = 0 is refused");

            assert!(error.to_string().contains(key), "{key}: {error}");
            assert!(
                error.to_string().contains("more than zero"),
                "{key}: {error}"
            );
        }
    }

    #[test]
    fn keys_default_to_the_built_in_ones() {
        let keys = Keys::default();

        assert_eq!(keys.keys(Command::Palette), ["cmd-k", "ctrl-k", ":"]);
        assert_eq!(keys.bindings().count(), Command::ALL.len());
    }

    #[test]
    fn a_remapped_command_replaces_its_defaults_and_leaves_the_rest_alone() {
        let (_dir, path) = write("[keys]\npalette = [\"cmd-p\"]\n");
        let keys = Settings::read_from(&path).expect("parses").keys;

        // Replaced, not added to: somebody who rebinds a key means it.
        assert_eq!(keys.keys(Command::Palette), ["cmd-p"]);
        // And everything they did not mention still works.
        assert_eq!(keys.keys(Command::Logs), Command::Logs.default_keys());
    }

    #[test]
    fn a_command_bound_to_nothing_is_unbound() {
        let (_dir, path) = write("[keys]\ndismiss = []\n");
        let keys = Settings::read_from(&path).expect("parses").keys;

        assert!(keys.keys(Command::Dismiss).is_empty());
    }

    #[test]
    fn a_misspelled_command_is_refused_rather_than_doing_nothing() {
        // The alternative is a key that silently never fires, with no way to
        // tell that from a bug in the app.
        let (_dir, path) = write("[keys]\npallette = [\"cmd-p\"]\n");
        let error = Settings::read_from(&path).expect_err("a typo is an error");

        let message = error.to_string();
        assert!(message.contains("pallette"), "{message}");
        assert!(message.contains("palette"), "{message}");
    }

    #[test]
    fn the_palette_is_never_bound_to_a_letter() {
        // Inside the palette a text field has focus, so a letter bound there
        // would be swallowed instead of typed — or worse, both.
        for command in [Command::Next, Command::Previous, Command::Confirm] {
            for key in command.default_keys() {
                assert!(
                    key.len() > 1,
                    "{} is bound to the single key `{key}`",
                    command.name()
                );
            }
        }
    }

    #[test]
    fn denying_a_context_removes_it_from_writable() {
        let mut access = Access {
            read_only_by_default: true,
            writable: ["prod".to_owned()].into_iter().collect(),
            ..Access::default()
        };
        access.deny("prod");

        assert!(!access.may_mutate("prod"));
        assert!(!access.writable.contains("prod"));
    }
}
