//! Where Periscope keeps its files.
//!
//! Deliberately short: logs and settings only. Credentials are never written to
//! disk, so there is no cache or keychain directory here and there must not be
//! one added.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "periscope";
const APPLICATION: &str = "Periscope";

/// Resolves the platform's project directories.
fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .context("could not determine a home directory for this user")
}

/// Directory for rotating log files.
pub fn log_dir() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    // `data_local_dir` on macOS is inside ~/Library, which is where a log
    // directory belongs; on Linux it lands under ~/.local/share.
    Ok(dirs.data_local_dir().join("logs"))
}

/// Directory exported log buffers are written to.
///
/// Under the app's own data directory rather than anywhere shared: an export is
/// a debugging artefact, and cluster logs are not something to scatter into a
/// user's Documents folder by default.
pub fn export_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_local_dir().join("exports"))
}

/// Path to the action audit log.
///
/// Beside the logs rather than in the config directory: it is a record of what
/// happened, not something the user edits.
pub fn audit_file() -> Result<PathBuf> {
    Ok(project_dirs()?.data_local_dir().join("audit.log"))
}

/// Path to the user's settings file.
pub fn settings_file() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("settings.toml"))
}

/// Path to the remembered UI state.
///
/// Beside the audit log for the same reason it is: the app writes this one, the
/// user writes `settings.toml`, and the two must not share a file that either
/// of them may overwrite. See [`crate::state`].
pub fn state_file() -> Result<PathBuf> {
    Ok(project_dirs()?.data_local_dir().join("state.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a path is inside a directory belonging to this application.
    ///
    /// Case-insensitively, because the platforms disagree and both are right:
    /// macOS uses `~/Library/Application Support/dev.periscope.Periscope`, and
    /// XDG on Linux uses `~/.local/share/periscope`. Asserting the capitalised
    /// form passed on the machine these were written on and failed the first
    /// time CI ran them on Linux.
    fn is_ours(path: &std::path::Path) -> bool {
        path.to_string_lossy().to_lowercase().contains("periscope")
    }

    #[test]
    fn log_dir_is_under_an_app_specific_directory() {
        let dir = log_dir().expect("home directory resolves in tests");
        assert!(dir.ends_with("logs"));
        assert!(is_ours(&dir), "{}", dir.display());
    }

    #[test]
    fn exports_land_in_the_apps_own_directory() {
        let dir = export_dir().expect("home directory resolves in tests");
        assert!(dir.ends_with("exports"));
        assert!(is_ours(&dir), "{}", dir.display());
    }

    #[test]
    fn the_audit_log_lives_with_the_apps_data() {
        let path = audit_file().expect("home directory resolves in tests");
        assert_eq!(path.file_name().unwrap(), "audit.log");
        assert!(is_ours(&path), "{}", path.display());
    }

    #[test]
    fn settings_live_next_to_other_app_config() {
        let file = settings_file().expect("home directory resolves in tests");
        assert_eq!(file.file_name().unwrap(), "settings.toml");
    }

    #[test]
    fn remembered_state_is_a_file_of_its_own_beside_the_audit_log() {
        // The app writes this one and the user writes settings.toml; a session
        // that rewrote a hand-edited config file would lose its comments. On
        // macOS the two directories are the same path, which is exactly why the
        // separation has to be in the file name.
        let state = state_file().expect("home directory resolves in tests");
        let settings = settings_file().expect("home directory resolves in tests");
        let audit = audit_file().expect("home directory resolves in tests");

        assert_eq!(state.file_name().unwrap(), "state.toml");
        assert_ne!(state, settings);
        assert_eq!(state.parent(), audit.parent());
    }
}
