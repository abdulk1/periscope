//! User-facing application settings.
//!
//! Read from `settings.toml` in the platform's config directory. The file is
//! optional: a missing one means defaults, which is not an error. A *malformed*
//! one is an error, and it is surfaced rather than swallowed — silently running
//! with defaults when someone has written a read-only rule they are relying on
//! would be the worst possible failure of this module.

use std::collections::BTreeSet;
use std::path::Path;

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

/// Everything the user can configure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// Appearance.
    pub theme: ThemeChoice,
    /// Which clusters may be changed.
    pub access: Access,
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

        toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.display().to_string(),
            source,
        })
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
