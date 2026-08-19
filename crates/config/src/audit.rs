//! The action audit log.
//!
//! Every mutation Periscope performs is written here, whether it succeeded,
//! failed, or was refused. It exists so that "what did I do to that cluster at
//! four o'clock" has an answer that does not depend on anyone's memory.
//!
//! Deliberate properties:
//!
//! * **Local only.** It is never uploaded anywhere, and it records cluster
//!   *context names* — which the user chose — not server URLs or credentials.
//! * **Append-only, one JSON object per line.** Greppable with the tools an SRE
//!   already has, and readable after a crash mid-write.
//! * **Refusals are recorded too.** A read-only cluster refusing a delete is
//!   exactly the kind of thing worth being able to prove afterwards.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What happened to an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The apiserver accepted it.
    Applied,
    /// A dry run: nothing was changed.
    DryRun,
    /// Periscope refused before sending anything.
    Refused,
    /// The apiserver rejected it, or the request failed.
    Failed,
}

impl Outcome {
    /// Whether anything actually changed in the cluster.
    pub fn changed_anything(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// One line of the audit log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// When it happened, RFC3339 in UTC.
    pub at: String,
    /// The kubeconfig context.
    pub cluster: String,
    /// Namespace, empty for cluster-scoped objects.
    pub namespace: String,
    /// Kind, as `kubectl` names it: `pods`, `deployments.apps`.
    pub kind: String,
    /// Object name.
    pub name: String,
    /// The operation: `delete`, `scale`, `restart`, `cordon`, `apply`.
    pub action: String,
    /// Anything worth recording about the operation itself, such as the replica
    /// count a scale asked for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// What happened.
    pub outcome: Outcome,
    /// Why it failed or was refused, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Entry {
    /// A new entry, stamped now.
    pub fn new(
        cluster: impl Into<String>,
        namespace: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            at: now(),
            cluster: cluster.into(),
            namespace: namespace.into(),
            kind: kind.into(),
            name: name.into(),
            action: action.into(),
            detail: String::new(),
            outcome: Outcome::Applied,
            reason: String::new(),
        }
    }

    /// Records what the operation asked for.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Records how it ended.
    pub fn outcome(mut self, outcome: Outcome, reason: impl Into<String>) -> Self {
        self.outcome = outcome;
        self.reason = reason.into();
        self
    }

    /// How the entry reads in the UI's activity list.
    pub fn summary(&self) -> String {
        let target = if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.namespace, self.name)
        };
        let detail = if self.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.detail)
        };

        format!(
            "{} {} {} on {}{detail}",
            match self.outcome {
                Outcome::Applied => "did",
                Outcome::DryRun => "dry-ran",
                Outcome::Refused => "refused",
                Outcome::Failed => "failed",
            },
            self.action,
            target,
            self.cluster
        )
    }
}

/// The current time, RFC3339 in UTC.
///
/// Public because a rollout restart stamps the same format onto a pod template,
/// and two clocks in one process would be one too many.
pub fn rfc3339_now() -> String {
    now()
}

/// The current time, RFC3339 in UTC.
fn now() -> String {
    // `jiff` comes in with k8s-openapi and is already the project's clock.
    k8s_time::now()
}

/// Wraps the timestamp source so the rest of the module does not care.
mod k8s_time {
    /// Now, as RFC3339.
    pub fn now() -> String {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(since) => format_epoch(since.as_secs()),
            // A clock before 1970 is not worth a panic in an audit writer.
            Err(_) => "1970-01-01T00:00:00Z".to_owned(),
        }
    }

    /// Formats seconds since the epoch as RFC3339 UTC, without a date library.
    pub fn format_epoch(seconds: u64) -> String {
        let (days, rest) = (seconds / 86_400, seconds % 86_400);
        let (hour, minute, second) = (rest / 3_600, rest % 3_600 / 60, rest % 60);
        let (year, month, day) = civil_from_days(days as i64);
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    }

    /// Howard Hinnant's `civil_from_days`, the standard way to turn a day count
    /// into a calendar date without a table.
    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
}

/// Appends entries to a file.
#[derive(Clone, Debug)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// A log writing to a specific file.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The log in the platform's data directory.
    pub fn open() -> Result<Self, std::io::Error> {
        let path =
            crate::paths::audit_file().map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(Self::at(path))
    }

    /// Where entries are written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry.
    ///
    /// Failing to write the audit log must not stop the app, but it must not be
    /// silent either: the caller logs what it could not record.
    pub fn append(&self, entry: &Entry) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut line = serde_json::to_string(entry)?;
        line.push('\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // Every append, not only the create: a log that was already there with
        // the wrong mode is the case that matters.
        crate::paths::restrict(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Reads the whole log back, oldest first.
    ///
    /// Lines that cannot be parsed are skipped rather than failing the read: a
    /// truncated last line after a crash must not hide everything before it.
    pub fn read(&self) -> Result<Vec<Entry>, std::io::Error> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };

        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "periscope-audit-{}-{:?}-{name}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn entry() -> Entry {
        Entry::new("prod", "payments", "pods", "api-0", "delete")
    }

    #[test]
    fn an_entry_round_trips_through_the_file() {
        let file = TempFile::new("roundtrip");
        let log = AuditLog::at(&file.0);

        log.append(&entry()).expect("appends");
        log.append(
            &entry()
                .detail("replicas=3")
                .outcome(Outcome::Failed, "not found"),
        )
        .expect("appends");

        let read = log.read().expect("reads");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].cluster, "prod");
        assert_eq!(read[1].detail, "replicas=3");
        assert_eq!(read[1].outcome, Outcome::Failed);
        assert_eq!(read[1].reason, "not found");
    }

    #[test]
    fn reading_a_log_that_does_not_exist_yet_is_empty_not_an_error() {
        let file = TempFile::new("missing");
        assert!(AuditLog::at(&file.0).read().expect("reads").is_empty());
    }

    #[test]
    fn a_truncated_last_line_does_not_hide_the_rest() {
        let file = TempFile::new("truncated");
        let log = AuditLog::at(&file.0);
        log.append(&entry()).expect("appends");
        std::fs::write(
            &file.0,
            format!(
                "{}\n{{\"at\":\"2026-08-18T10:00:00Z\",\"clus",
                serde_json::to_string(&entry()).unwrap()
            ),
        )
        .expect("writes");

        // A crash mid-write must not make the whole history unreadable.
        assert_eq!(log.read().expect("reads").len(), 1);
    }

    #[test]
    fn refusals_are_recorded_as_such() {
        let refused = entry().outcome(Outcome::Refused, "prod is read-only");

        assert!(!refused.outcome.changed_anything());
        assert_eq!(refused.summary(), "refused delete payments/api-0 on prod");
    }

    #[test]
    fn a_dry_run_says_it_changed_nothing() {
        let dry = Entry::new("prod", "payments", "deployments.apps", "api", "apply")
            .outcome(Outcome::DryRun, "");
        assert!(!dry.outcome.changed_anything());
        assert_eq!(dry.summary(), "dry-ran apply payments/api on prod");
    }

    #[test]
    fn cluster_scoped_objects_read_without_a_stray_slash() {
        let entry = Entry::new("prod", "", "nodes", "worker-0", "cordon");
        assert_eq!(entry.summary(), "did cordon worker-0 on prod");
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(
            k8s_time::format_epoch(1_786_960_800),
            "2026-08-17T10:00:00Z"
        );
        assert_eq!(
            k8s_time::format_epoch(1_787_047_200),
            "2026-08-18T10:00:00Z"
        );
        assert_eq!(k8s_time::format_epoch(0), "1970-01-01T00:00:00Z");
        // A leap day, which is where a hand-rolled calendar usually breaks.
        assert_eq!(
            k8s_time::format_epoch(1_709_164_800),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_log_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        // It records what was done to which cluster and why it failed. Other
        // local accounts have no business in it, and the platform default is
        // world-readable.
        let file = TempFile::new("modes");
        let log = AuditLog::at(file.0.clone());
        log.append(&Entry::new("prod", "default", "pods", "api-0", "delete"))
            .expect("appended");

        let mode = std::fs::metadata(log.path())
            .expect("the log exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode {:o}", mode & 0o777);
    }

    #[test]
    fn the_log_never_records_a_server_url_or_a_token() {
        // The audit log is the one file that could leak what the rest of the
        // app is careful never to write down.
        let line = serde_json::to_string(&entry()).unwrap();
        assert!(!line.contains("https://"), "{line}");
        assert!(!line.to_lowercase().contains("token"), "{line}");
    }
}
