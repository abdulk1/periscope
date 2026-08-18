//! The log-streaming vocabulary.
//!
//! Logs are the one thing in Periscope that arrives faster than a person can
//! read, so the types here are shaped around throughput rather than around one
//! line at a time:
//!
//! * Lines cross the bridge in **batches**. A single busy pod can emit
//!   thousands of lines a second; one event per line would flood a bounded
//!   channel that is sized for watch events.
//! * A batch carries lines from **every pod in the session**, already merged.
//!   Merging on the cluster side means the UI never sorts, and the interleaving
//!   is the same one `kubectl logs -f` would produce.
//! * Every line names its source, because a merged stream is unreadable
//!   without saying which pod said what.

use std::sync::Arc;
use std::time::SystemTime;

/// Which pods a log session follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogSelector {
    /// One pod by name.
    Pod(Arc<str>),
    /// Every pod matching a label selector, including pods that appear later.
    Labels(Arc<str>),
}

impl std::fmt::Display for LogSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pod(name) => f.write_str(name),
            Self::Labels(selector) => write!(f, "{selector}"),
        }
    }
}

/// Everything a log session needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTarget {
    /// Namespace the pods live in.
    pub namespace: Arc<str>,
    /// Which pods.
    pub selector: LogSelector,
    /// Which container, or `None` for the pod's only (or default) container.
    pub container: Option<Arc<str>>,
    /// Read the previous container's logs instead of the running one — what
    /// `kubectl logs --previous` does, and the only way to see why a pod
    /// crash-looped.
    pub previous: bool,
    /// How many lines of history to fetch before following.
    pub tail_lines: Option<i64>,
}

impl LogTarget {
    /// A session following one pod.
    pub fn pod(namespace: impl AsRef<str>, name: impl AsRef<str>) -> Self {
        Self {
            namespace: Arc::from(namespace.as_ref()),
            selector: LogSelector::Pod(Arc::from(name.as_ref())),
            container: None,
            previous: false,
            tail_lines: Some(DEFAULT_TAIL_LINES),
        }
    }

    /// A session following every pod matching a label selector.
    pub fn labels(namespace: impl AsRef<str>, selector: impl AsRef<str>) -> Self {
        Self {
            namespace: Arc::from(namespace.as_ref()),
            selector: LogSelector::Labels(Arc::from(selector.as_ref())),
            container: None,
            previous: false,
            tail_lines: Some(DEFAULT_TAIL_LINES),
        }
    }

    /// Reads a specific container.
    pub fn container(mut self, container: Option<Arc<str>>) -> Self {
        self.container = container;
        self
    }

    /// Reads the previous container's logs.
    pub fn previous(mut self, previous: bool) -> Self {
        self.previous = previous;
        self
    }

    /// A short description for the log view's header.
    pub fn label(&self) -> String {
        let container = self
            .container
            .as_deref()
            .map(|container| format!(" [{container}]"))
            .unwrap_or_default();
        let previous = if self.previous { " (previous)" } else { "" };
        format!("{}/{}{container}{previous}", self.namespace, self.selector)
    }
}

/// How much history a session asks for before following.
///
/// `kubectl logs -f` starts from the beginning of the container's logs, which
/// on a long-running pod means megabytes before the first live line. This is
/// the same default `kubectl logs --tail` uses interactively.
pub const DEFAULT_TAIL_LINES: i64 = 1_000;

/// One pod-and-container a session is reading from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogSource {
    /// Pod name.
    pub pod: Arc<str>,
    /// Container name, once known.
    pub container: Arc<str>,
}

impl LogSource {
    /// Builds a source identity.
    pub fn new(pod: impl AsRef<str>, container: impl AsRef<str>) -> Self {
        Self {
            pod: Arc::from(pod.as_ref()),
            container: Arc::from(container.as_ref()),
        }
    }

    /// How the source column reads.
    pub fn label(&self) -> String {
        if self.container.is_empty() {
            self.pod.to_string()
        } else {
            format!("{}/{}", self.pod, self.container)
        }
    }
}

/// What one source is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogSourceState {
    /// Attached and streaming.
    Streaming,
    /// The stream ended cleanly — the container exited, or the pod went away.
    Ended,
    /// The stream failed. Carries the API's own words.
    Failed {
        /// Verbatim underlying reason.
        reason: String,
    },
}

/// One log line.
///
/// The text has already had its trailing newline and, when timestamps were
/// requested, its RFC3339 prefix removed: the prefix is parsed into `timestamp`
/// so the view can render and sort by it without re-parsing every frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// Which pod and container emitted it.
    pub source: LogSource,
    /// When the container emitted it, if the apiserver said.
    pub timestamp: Option<SystemTime>,
    /// The line itself, without its trailing newline.
    pub text: Arc<str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_describes_itself_for_the_header() {
        let target = LogTarget::pod("payments", "api-0");
        assert_eq!(target.label(), "payments/api-0");

        let with_container = LogTarget::pod("payments", "api-0")
            .container(Some(Arc::from("sidecar")))
            .previous(true);
        assert_eq!(
            with_container.label(),
            "payments/api-0 [sidecar] (previous)"
        );

        let selector = LogTarget::labels("payments", "app=api");
        assert_eq!(selector.label(), "payments/app=api");
    }

    #[test]
    fn a_source_reads_as_pod_slash_container() {
        assert_eq!(LogSource::new("api-0", "api").label(), "api-0/api");
        // Before the container is known, the pod alone is better than a stray
        // slash.
        assert_eq!(LogSource::new("api-0", "").label(), "api-0");
    }

    #[test]
    fn targets_default_to_a_bounded_amount_of_history() {
        // Following from the beginning of a month-old container's logs is not
        // what anyone means by "show me the logs".
        assert_eq!(
            LogTarget::pod("default", "api-0").tail_lines,
            Some(DEFAULT_TAIL_LINES)
        );
    }
}
