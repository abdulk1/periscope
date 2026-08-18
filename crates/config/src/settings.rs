//! User-facing application settings.
//!
//! Phase 0 only needs the theme. This type is UI-framework agnostic on purpose:
//! `config` must not depend on GPUI, so the mapping to a concrete theme lives in
//! the `ui` crate.

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

/// Everything the user can configure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// Appearance.
    pub theme: ThemeChoice,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn settings_default_when_fields_are_missing() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, Settings::default());
    }
}
