//! Port forwards.
//!
//! A forward is a local TCP listener that hands every connection to the
//! apiserver's port-forward endpoint. The interesting states are all failure
//! states, so they are modelled explicitly: a forward that has stopped working
//! must say so on screen rather than looking identical to one that is idle.

use std::sync::Arc;

/// Identifies one forward for as long as the app runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ForwardId(pub u64);

impl std::fmt::Display for ForwardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "forward-{}", self.0)
    }
}

/// What to forward to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTarget {
    /// Namespace the pod lives in.
    pub namespace: Arc<str>,
    /// Which pod.
    pub pod: Arc<str>,
    /// The port inside the pod.
    pub remote_port: u16,
    /// The port to listen on locally, or `None` to let the OS choose.
    ///
    /// Choosing is usually better: a fixed port fails when something else has
    /// it, and the failure ("address in use") is about the wrong thing.
    pub local_port: Option<u16>,
}

impl ForwardTarget {
    /// A forward to a pod's port, on an OS-chosen local port.
    pub fn new(namespace: impl AsRef<str>, pod: impl AsRef<str>, remote_port: u16) -> Self {
        Self {
            namespace: Arc::from(namespace.as_ref()),
            pod: Arc::from(pod.as_ref()),
            remote_port,
            local_port: None,
        }
    }

    /// Asks for a specific local port.
    pub fn on_local_port(mut self, port: u16) -> Self {
        self.local_port = Some(port);
        self
    }

    /// How the forward reads in a list.
    pub fn label(&self) -> String {
        format!("{}/{}:{}", self.namespace, self.pod, self.remote_port)
    }
}

/// Where a forward is in its life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardState {
    /// Binding the local port.
    Starting,
    /// Listening and ready to accept connections.
    Listening {
        /// The port that was actually bound.
        local_port: u16,
    },
    /// Listening, but the last connection failed. The listener is still up, so
    /// this recovers by itself if whatever broke comes back.
    Degraded {
        /// The port that is still bound.
        local_port: u16,
        /// Verbatim reason the last connection failed.
        reason: String,
    },
    /// Not forwarding: the listener could not be bound, or it stopped.
    Failed {
        /// Verbatim underlying reason.
        reason: String,
    },
    /// Torn down deliberately.
    Stopped,
}

impl ForwardState {
    /// Short label for the forwards list.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Listening { .. } => "listening",
            Self::Degraded { .. } => "degraded",
            Self::Failed { .. } => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// The port being listened on, when there is one.
    pub fn local_port(&self) -> Option<u16> {
        match self {
            Self::Listening { local_port } | Self::Degraded { local_port, .. } => Some(*local_port),
            _ => None,
        }
    }

    /// Why it is unhappy, if it is.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Degraded { reason, .. } | Self::Failed { reason } => Some(reason),
            _ => None,
        }
    }

    /// Whether this needs the user's attention.
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Degraded { .. } | Self::Failed { .. })
    }

    /// Whether the forward is finished for good.
    pub fn is_over(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Stopped)
    }
}

/// One forward, as the UI lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardInfo {
    /// Which forward.
    pub id: ForwardId,
    /// What it points at.
    pub target: Arc<ForwardTarget>,
    /// Where it is in its life.
    pub state: ForwardState,
    /// Connections accepted since it started.
    pub connections: u64,
}

impl ForwardInfo {
    /// A forward that has just been asked for.
    pub fn starting(id: ForwardId, target: Arc<ForwardTarget>) -> Self {
        Self {
            id,
            target,
            state: ForwardState::Starting,
            connections: 0,
        }
    }

    /// The address to paste into a browser, once there is one.
    pub fn address(&self) -> Option<String> {
        self.state
            .local_port()
            .map(|port| format!("127.0.0.1:{port}"))
    }

    /// One line describing the whole forward.
    pub fn summary(&self) -> String {
        match self.address() {
            Some(address) => format!("{address} → {}", self.target.label()),
            None => format!("{} ({})", self.target.label(), self.state.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Arc<ForwardTarget> {
        Arc::new(ForwardTarget::new("payments", "api-0", 8080))
    }

    #[test]
    fn a_target_reads_as_pod_and_port() {
        assert_eq!(target().label(), "payments/api-0:8080");
    }

    #[test]
    fn a_forward_without_a_port_yet_says_what_it_is_doing() {
        let forward = ForwardInfo::starting(ForwardId(1), target());

        assert_eq!(forward.address(), None);
        assert_eq!(forward.summary(), "payments/api-0:8080 (starting)");
    }

    #[test]
    fn a_listening_forward_shows_the_address_to_use() {
        let mut forward = ForwardInfo::starting(ForwardId(1), target());
        forward.state = ForwardState::Listening { local_port: 51_234 };

        assert_eq!(forward.address().as_deref(), Some("127.0.0.1:51234"));
        assert_eq!(forward.summary(), "127.0.0.1:51234 → payments/api-0:8080");
        assert!(!forward.state.is_problem());
    }

    #[test]
    fn a_degraded_forward_keeps_its_port_and_says_what_broke() {
        // The listener is still up, so the address stays usable; what failed
        // was one connection, and the reason has to survive.
        let state = ForwardState::Degraded {
            local_port: 51_234,
            reason: "pod api-0 does not have port 8080 open".to_owned(),
        };

        assert_eq!(state.local_port(), Some(51_234));
        assert!(state.is_problem());
        assert!(!state.is_over());
        assert!(state.detail().is_some_and(|reason| reason.contains("8080")));
    }

    #[test]
    fn a_failed_forward_is_over_and_has_no_address() {
        let state = ForwardState::Failed {
            reason: "address already in use".to_owned(),
        };

        assert_eq!(state.local_port(), None);
        assert!(state.is_problem());
        assert!(state.is_over());
    }

    #[test]
    fn stopping_is_not_a_problem() {
        assert!(!ForwardState::Stopped.is_problem());
        assert!(ForwardState::Stopped.is_over());
    }
}
