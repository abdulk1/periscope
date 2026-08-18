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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_is_under_an_app_specific_directory() {
        let dir = log_dir().expect("home directory resolves in tests");
        assert!(dir.ends_with("logs"));
        assert!(dir.to_string_lossy().contains("Periscope"));
    }

    #[test]
    fn exports_land_in_the_apps_own_directory() {
        let dir = export_dir().expect("home directory resolves in tests");
        assert!(dir.ends_with("exports"));
        assert!(dir.to_string_lossy().contains("Periscope"));
    }

    #[test]
    fn the_audit_log_lives_with_the_apps_data() {
        let path = audit_file().expect("home directory resolves in tests");
        assert_eq!(path.file_name().unwrap(), "audit.log");
        assert!(path.to_string_lossy().contains("Periscope"));
    }

    #[test]
    fn settings_live_next_to_other_app_config() {
        let file = settings_file().expect("home directory resolves in tests");
        assert_eq!(file.file_name().unwrap(), "settings.toml");
    }
}
