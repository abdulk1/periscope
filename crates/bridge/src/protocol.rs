//! The message vocabulary that crosses the tokio <-> GPUI boundary.
//!
//! Commands travel UI -> cluster layer. Events travel cluster layer -> UI.
//! Nothing in this module touches GPUI or kube; it is deliberately a plain
//! data module so both sides can depend on it without pulling either runtime.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Identifies a connected cluster. This is the kubeconfig context name, which
/// is what the user sees and what every error message must be able to name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClusterId(Arc<str>);

impl ClusterId {
    /// Wraps a context name.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(Arc::from(name.as_ref()))
    }

    /// The context name as it appears in kubeconfig.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ClusterId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ClusterId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// A request from the UI to the cluster layer.
///
/// Deliberately *not* `#[non_exhaustive]`: every consumer is a crate in this
/// workspace, so exhaustive matching is the point. When Phase 1 adds commands,
/// the compiler should name every place that has to handle them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterCommand {
    /// Round-trip liveness probe. The cluster layer answers with
    /// [`ClusterEvent::Pong`] carrying the same nonce.
    Ping {
        /// Which cluster to probe.
        cluster: ClusterId,
        /// Caller-chosen value echoed back, so replies can be correlated.
        nonce: u64,
    },
    /// Tear down all work for a cluster.
    Disconnect {
        /// Which cluster to stop.
        cluster: ClusterId,
    },
}

impl ClusterCommand {
    /// The cluster this command addresses.
    pub fn cluster(&self) -> &ClusterId {
        match self {
            Self::Ping { cluster, .. } | Self::Disconnect { cluster } => cluster,
        }
    }
}

/// Where a cluster connection currently stands.
///
/// Every variant that can fail carries the underlying error text. This audience
/// wants the real API error, not "something went wrong".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection attempted yet.
    Idle,
    /// Handshake in flight.
    Connecting,
    /// Watches established and healthy.
    Connected,
    /// Connected but some watches are failing or lagging.
    Degraded {
        /// Verbatim underlying reason.
        reason: String,
    },
    /// Credentials were rejected or expired. Never render this as an empty table.
    AuthFailed {
        /// Verbatim underlying reason.
        reason: String,
    },
    /// Connection closed, either deliberately or by failure.
    Disconnected {
        /// Verbatim underlying reason, if the disconnect was not deliberate.
        reason: Option<String>,
    },
}

impl ConnectionState {
    /// Short label for status chrome.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Degraded { .. } => "degraded",
            Self::AuthFailed { .. } => "auth failed",
            Self::Disconnected { .. } => "disconnected",
        }
    }

    /// The underlying error text, when there is one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Degraded { reason } | Self::AuthFailed { reason } => Some(reason),
            Self::Disconnected { reason } => reason.as_deref(),
            _ => None,
        }
    }

    /// Whether this state needs the user's attention.
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::Degraded { .. }
                | Self::AuthFailed { .. }
                | Self::Disconnected { reason: Some(_) }
        )
    }
}

/// A message from the cluster layer to the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ClusterEvent {
    /// Reply to [`ClusterCommand::Ping`].
    Pong {
        /// Cluster that answered.
        cluster: ClusterId,
        /// Nonce copied from the ping.
        nonce: u64,
        /// Time the cluster layer spent handling the ping.
        elapsed: Duration,
    },
    /// The connection state machine moved.
    Status {
        /// Cluster whose state changed.
        cluster: ClusterId,
        /// The new state.
        state: ConnectionState,
    },
    /// The event channel overflowed and messages were discarded. The UI must
    /// treat affected data as stale and resync rather than silently drift.
    Stale {
        /// Cluster the drops belong to, if it could be attributed.
        cluster: Option<ClusterId>,
        /// How many events were dropped since the last `Stale`.
        dropped: usize,
    },
}

/// Identity used to collapse superseded events during coalescing.
///
/// Two events with the same key are redundant: only the newer one matters. A
/// resync storm of 10k object updates collapses to one update per object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EventKey {
    /// Latest connection state for a cluster wins.
    Status(ClusterId),
    /// Drop notices accumulate per cluster; the newest count supersedes.
    Stale(Option<ClusterId>),
}

impl ClusterEvent {
    /// The coalescing identity, or `None` for events that must all be delivered.
    pub fn coalesce_key(&self) -> Option<EventKey> {
        match self {
            // Every pong answers a distinct ping; collapsing them would lose replies.
            Self::Pong { .. } => None,
            Self::Status { cluster, .. } => Some(EventKey::Status(cluster.clone())),
            Self::Stale { cluster, .. } => Some(EventKey::Stale(cluster.clone())),
        }
    }

    /// The cluster this event concerns, when it concerns one.
    pub fn cluster(&self) -> Option<&ClusterId> {
        match self {
            Self::Pong { cluster, .. } | Self::Status { cluster, .. } => Some(cluster),
            Self::Stale { cluster, .. } => cluster.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_id_round_trips() {
        let id = ClusterId::from("prod-eks");
        assert_eq!(id.as_str(), "prod-eks");
        assert_eq!(id.to_string(), "prod-eks");
        assert_eq!(id, ClusterId::new(String::from("prod-eks")));
    }

    #[test]
    fn status_events_for_same_cluster_share_a_key() {
        let a = ClusterEvent::Status {
            cluster: "prod".into(),
            state: ConnectionState::Connecting,
        };
        let b = ClusterEvent::Status {
            cluster: "prod".into(),
            state: ConnectionState::Connected,
        };
        let other = ClusterEvent::Status {
            cluster: "staging".into(),
            state: ConnectionState::Connected,
        };

        assert_eq!(a.coalesce_key(), b.coalesce_key());
        assert_ne!(a.coalesce_key(), other.coalesce_key());
    }

    #[test]
    fn pongs_are_never_coalesced() {
        let pong = ClusterEvent::Pong {
            cluster: "prod".into(),
            nonce: 1,
            elapsed: Duration::ZERO,
        };
        assert!(pong.coalesce_key().is_none());
    }

    #[test]
    fn failure_states_keep_their_error_text() {
        let state = ConnectionState::AuthFailed {
            reason: "exec plugin `aws` exited 255: expired token".into(),
        };
        assert!(state.is_problem());
        assert_eq!(state.label(), "auth failed");
        assert!(state.detail().unwrap().contains("expired token"));
    }

    #[test]
    fn deliberate_disconnect_is_not_a_problem() {
        assert!(!ConnectionState::Disconnected { reason: None }.is_problem());
        assert!(
            ConnectionState::Disconnected {
                reason: Some("connection reset by peer".into())
            }
            .is_problem()
        );
    }
}
