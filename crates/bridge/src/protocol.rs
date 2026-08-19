//! The message vocabulary that crosses the tokio <-> GPUI boundary.
//!
//! Commands travel UI -> cluster layer. Events travel cluster layer -> UI.
//! Nothing in this module touches GPUI or kube; it is deliberately a plain
//! data module so both sides can depend on it without pulling either runtime.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::coalesce::CoalesceKey;
use crate::exec::{ExecStatus, ExecTarget};
use crate::forward::{ForwardId, ForwardInfo, ForwardTarget};
use crate::logs::{LogLine, LogSource, LogSourceState, LogTarget};
use crate::mutation::{Mutation, MutationOutcome};
use crate::resource::{
    ColumnSpec, ContextInfo, KindId, KindInfo, ObjectDetail, ResourceKey, ResourceRow,
};

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
    /// Re-read kubeconfig and report the contexts it defines. Answered with
    /// [`ClusterEvent::Contexts`] or [`ClusterEvent::ConfigFailed`].
    ListContexts,
    /// Connect to a context and start watching it. Idempotent: connecting to an
    /// already-connected cluster is a no-op rather than a second set of watches.
    Connect {
        /// Which kubeconfig context to connect to.
        cluster: ClusterId,
    },
    /// Ask the apiserver what kinds it serves, including CRDs. Answered with
    /// [`ClusterEvent::Kinds`].
    Discover {
        /// Which cluster to interrogate.
        cluster: ClusterId,
    },
    /// Start streaming a kind. Replaces any watch already running for that
    /// kind on that cluster, which is how the namespace and selector filters
    /// are applied.
    Watch {
        /// Which cluster.
        cluster: ClusterId,
        /// Which kind.
        kind: KindId,
        /// Restrict to one namespace, or `None` for every namespace.
        namespace: Option<Arc<str>>,
        /// A label selector, e.g. `app=web,tier!=cache`.
        selector: Option<Arc<str>>,
    },
    /// Stop streaming a kind and forget its rows.
    StopWatch {
        /// Which cluster.
        cluster: ClusterId,
        /// Which kind.
        kind: KindId,
    },
    /// Fetch one object in full, for the detail view. Answered with
    /// [`ClusterEvent::Object`] or [`ClusterEvent::ObjectFailed`].
    FetchObject {
        /// Which cluster.
        cluster: ClusterId,
        /// Which kind.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
        /// Show secret values. Off unless the user explicitly asked: a Secret's
        /// data is masked by default, in the YAML as well as in the table.
        reveal: bool,
    },
    /// Start tailing logs. Replaces whatever session was running for this
    /// cluster: one log view, one session.
    StartLogs {
        /// Which cluster.
        cluster: ClusterId,
        /// Which pods, which container, and how much history.
        target: Arc<LogTarget>,
    },
    /// Stop tailing.
    StopLogs {
        /// Which cluster.
        cluster: ClusterId,
    },
    /// Run a command in a container and stream its output.
    Exec {
        /// Which cluster.
        cluster: ClusterId,
        /// What to run, and where.
        target: Arc<ExecTarget>,
    },
    /// Stop a running command.
    CancelExec {
        /// Which cluster.
        cluster: ClusterId,
    },
    /// Open a local port that forwards to a pod's port.
    StartForward {
        /// Which cluster.
        cluster: ClusterId,
        /// Which forward this is, chosen by the caller so it can be matched up
        /// with the events that follow.
        id: ForwardId,
        /// What to forward to.
        target: Arc<ForwardTarget>,
    },
    /// Tear a forward down.
    StopForward {
        /// Which cluster.
        cluster: ClusterId,
        /// Which forward.
        id: ForwardId,
    },
    /// Change something in a cluster.
    ///
    /// The UI sends this only after the store has authorised it and the user
    /// has confirmed a sentence naming the cluster and the object. The cluster
    /// layer checks the read-only policy again before touching the apiserver:
    /// two independent gates, because one bug in the UI must not be enough to
    /// mutate a cluster somebody marked read-only.
    Mutate {
        /// Which cluster.
        cluster: ClusterId,
        /// What to do.
        mutation: Arc<Mutation>,
    },
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
    /// The cluster this command addresses, for commands that address one.
    pub fn cluster(&self) -> Option<&ClusterId> {
        match self {
            Self::Ping { cluster, .. }
            | Self::Disconnect { cluster }
            | Self::Connect { cluster }
            | Self::Discover { cluster }
            | Self::Watch { cluster, .. }
            | Self::StopWatch { cluster, .. }
            | Self::FetchObject { cluster, .. }
            | Self::StartLogs { cluster, .. }
            | Self::StopLogs { cluster }
            | Self::Mutate { cluster, .. }
            | Self::StartForward { cluster, .. }
            | Self::StopForward { cluster, .. }
            | Self::Exec { cluster, .. }
            | Self::CancelExec { cluster } => Some(cluster),
            Self::ListContexts => None,
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
    /// The contexts kubeconfig defines.
    Contexts {
        /// Every context, in kubeconfig order.
        contexts: Arc<[ContextInfo]>,
        /// The `current-context`, when kubeconfig names one.
        current: Option<ClusterId>,
    },
    /// Kubeconfig could not be read or parsed. The UI shows this instead of an
    /// empty context list, which would look like "you have no clusters".
    ConfigFailed {
        /// Verbatim underlying reason.
        reason: String,
    },
    /// The kinds a cluster serves, from the discovery endpoint.
    Kinds {
        /// Cluster the kinds belong to.
        cluster: ClusterId,
        /// Every kind, including custom resources.
        kinds: Arc<[KindInfo]>,
    },
    /// A complete listing for one kind, replacing whatever the store held.
    ///
    /// Emitted when a watch starts and whenever it has to restart, which is the
    /// only way to learn that objects disappeared while the watch was down.
    ResourceReset {
        /// Cluster the listing belongs to.
        cluster: ClusterId,
        /// Kind that was listed.
        kind: KindId,
        /// The columns these rows carry cells for.
        columns: Arc<[ColumnSpec]>,
        /// Every object visible to this client, in no particular order.
        rows: Arc<[ResourceRow]>,
    },
    /// An object was added or changed.
    ResourceApplied {
        /// Cluster the object belongs to.
        cluster: ClusterId,
        /// Kind of the object.
        kind: KindId,
        /// The object's current row.
        row: Arc<ResourceRow>,
    },
    /// An object was deleted.
    ResourceDeleted {
        /// Cluster the object belonged to.
        cluster: ClusterId,
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
    },
    /// A full object, for the detail view.
    Object {
        /// Cluster the object belongs to.
        cluster: ClusterId,
        /// Kind of the object.
        kind: KindId,
        /// The object, its events and its owners.
        detail: Arc<ObjectDetail>,
    },
    /// Log lines, merged across every pod in the session and batched.
    ///
    /// Never coalesced: a batch that is dropped is text nobody can get back.
    /// The cluster layer keeps the rate manageable instead, by batching.
    LogBatch {
        /// Cluster the lines came from.
        cluster: ClusterId,
        /// The lines, in the order they were read.
        lines: Arc<[LogLine]>,
    },
    /// A log source attached, ended, or failed.
    LogSourceChanged {
        /// Cluster the source belongs to.
        cluster: ClusterId,
        /// Which pod and container.
        source: LogSource,
        /// What it is doing now.
        state: LogSourceState,
    },
    /// Output from a running command.
    ExecOutput {
        /// Cluster it is running on.
        cluster: ClusterId,
        /// The lines, in the order they were read.
        lines: Arc<[LogLine]>,
    },
    /// A command ended.
    ExecFinished {
        /// Cluster it ran on.
        cluster: ClusterId,
        /// How it ended.
        status: ExecStatus,
    },
    /// A forward's state changed.
    ForwardChanged {
        /// Cluster it belongs to.
        cluster: ClusterId,
        /// The forward, as it now stands.
        forward: Arc<ForwardInfo>,
    },
    /// A mutation finished, one way or another.
    MutationDone {
        /// Cluster it was aimed at.
        cluster: ClusterId,
        /// What was asked for.
        mutation: Arc<Mutation>,
        /// What happened.
        outcome: MutationOutcome,
    },
    /// The log session itself could not start or could not continue.
    LogsFailed {
        /// Cluster the session belonged to.
        cluster: ClusterId,
        /// Verbatim underlying reason.
        reason: String,
    },
    /// An object could not be fetched. Shown in place of the detail view, never
    /// as an empty pane.
    ObjectFailed {
        /// Cluster the object belongs to.
        cluster: ClusterId,
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
        /// Verbatim underlying reason.
        reason: String,
    },
    /// One kind cannot be watched, but the cluster is fine.
    ///
    /// Almost always RBAC: a role that grants `pods` and not `secrets` answers
    /// 403 for the one and streams the other. That is a fact about a kind, not
    /// about the credential, so it must not be reported as the connection
    /// failing — and it must not leave an empty table, which would read as "no
    /// secrets exist here".
    KindFailed {
        /// Cluster the kind belongs to.
        cluster: ClusterId,
        /// The kind that cannot be listed.
        kind: KindId,
        /// Verbatim underlying reason.
        reason: String,
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
    /// Only the newest reading of kubeconfig matters.
    Contexts,
    /// Only the newest discovery result for a cluster matters.
    Kinds(ClusterId),
    /// Latest state of one object wins; a delete supersedes an earlier update.
    Resource(ClusterId, KindId, ResourceKey),
    /// A full resync supersedes every pending event for that cluster and kind.
    ResourceReset(ClusterId, KindId),
    /// Only the newest detail fetch for an object matters.
    Object(ClusterId, KindId, ResourceKey),
    /// Latest state of one log source wins.
    LogSource(ClusterId, LogSource),
    /// Only the newest log failure for a cluster matters.
    LogsFailed(ClusterId),
    /// Latest state of one forward wins.
    Forward(ClusterId, ForwardId),
}

impl CoalesceKey for EventKey {
    fn is_barrier(&self) -> bool {
        matches!(self, Self::ResourceReset(..))
    }

    fn supersedes(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ResourceReset(cluster, kind),
                Self::Resource(other_cluster, other_kind, _)
                | Self::ResourceReset(other_cluster, other_kind),
            ) => cluster == other_cluster && kind == other_kind,
            _ => false,
        }
    }
}

impl ClusterEvent {
    /// The coalescing identity, or `None` for events that must all be delivered.
    pub fn coalesce_key(&self) -> Option<EventKey> {
        match self {
            // Every pong answers a distinct ping; collapsing them would lose replies.
            Self::Pong { .. } => None,
            // A config failure explains itself; the next successful read
            // replaces it via the Contexts key, so it must share that key.
            Self::Contexts { .. } | Self::ConfigFailed { .. } => Some(EventKey::Contexts),
            Self::Status { cluster, .. } => Some(EventKey::Status(cluster.clone())),
            Self::Stale { cluster, .. } => Some(EventKey::Stale(cluster.clone())),
            Self::Kinds { cluster, .. } => Some(EventKey::Kinds(cluster.clone())),
            Self::ResourceReset { cluster, kind, .. } => {
                Some(EventKey::ResourceReset(cluster.clone(), kind.clone()))
            }
            Self::ResourceApplied { cluster, kind, row } => Some(EventKey::Resource(
                cluster.clone(),
                kind.clone(),
                row.key.clone(),
            )),
            Self::ResourceDeleted { cluster, kind, key } => Some(EventKey::Resource(
                cluster.clone(),
                kind.clone(),
                key.clone(),
            )),
            // A failure and a success answer the same request, so the newer one
            // must replace the older rather than both landing.
            Self::Object {
                cluster,
                kind,
                detail,
            } => Some(EventKey::Object(
                cluster.clone(),
                kind.clone(),
                detail.key.clone(),
            )),
            Self::ObjectFailed {
                cluster, kind, key, ..
            } => Some(EventKey::Object(cluster.clone(), kind.clone(), key.clone())),
            // A kind's availability and its listing answer the same question,
            // so a fresh listing supersedes the refusal and vice versa.
            Self::KindFailed { cluster, kind, .. } => {
                Some(EventKey::ResourceReset(cluster.clone(), kind.clone()))
            }
            // Dropping a batch would lose lines outright, so batches are never
            // collapsed into one another.
            Self::LogBatch { .. } => None,
            Self::LogSourceChanged {
                cluster, source, ..
            } => Some(EventKey::LogSource(cluster.clone(), source.clone())),
            Self::LogsFailed { cluster, .. } => Some(EventKey::LogsFailed(cluster.clone())),
            // Every outcome is reported: two deletes are two things that
            // happened, and collapsing them would hide one.
            Self::MutationDone { .. } => None,
            Self::ForwardChanged { cluster, forward } => {
                Some(EventKey::Forward(cluster.clone(), forward.id))
            }
            // Output is text: collapsing two chunks would lose one of them.
            Self::ExecOutput { .. } | Self::ExecFinished { .. } => None,
        }
    }

    /// The cluster this event concerns, when it concerns one.
    pub fn cluster(&self) -> Option<&ClusterId> {
        match self {
            Self::Pong { cluster, .. }
            | Self::Status { cluster, .. }
            | Self::Kinds { cluster, .. }
            | Self::ResourceReset { cluster, .. }
            | Self::ResourceApplied { cluster, .. }
            | Self::ResourceDeleted { cluster, .. }
            | Self::Object { cluster, .. }
            | Self::ObjectFailed { cluster, .. }
            | Self::KindFailed { cluster, .. }
            | Self::LogBatch { cluster, .. }
            | Self::LogSourceChanged { cluster, .. }
            | Self::LogsFailed { cluster, .. }
            | Self::MutationDone { cluster, .. }
            | Self::ForwardChanged { cluster, .. }
            | Self::ExecOutput { cluster, .. }
            | Self::ExecFinished { cluster, .. } => Some(cluster),
            Self::Stale { cluster, .. } => cluster.as_ref(),
            Self::Contexts { .. } | Self::ConfigFailed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceRow;

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
    fn a_resync_supersedes_pending_events_for_its_own_cluster_and_kind_only() {
        let pods = KindId::new("", "v1", "Pod", "pods");
        let deployments = KindId::new("apps", "v1", "Deployment", "deployments");
        let reset = EventKey::ResourceReset("prod".into(), pods.clone());
        assert!(reset.is_barrier());

        assert!(reset.supersedes(&EventKey::Resource(
            "prod".into(),
            pods.clone(),
            ResourceKey::new("default", "api-0")
        )));
        assert!(reset.supersedes(&EventKey::ResourceReset("prod".into(), pods.clone())));

        // Another cluster, another kind, and this cluster's connection state
        // all survive: a pod resync says nothing about deployments.
        assert!(!reset.supersedes(&EventKey::Resource(
            "staging".into(),
            pods.clone(),
            ResourceKey::new("default", "api-0")
        )));
        assert!(!reset.supersedes(&EventKey::Resource(
            "prod".into(),
            deployments,
            ResourceKey::new("default", "api")
        )));
        assert!(!reset.supersedes(&EventKey::Status("prod".into())));
    }

    #[test]
    fn object_events_are_keyed_by_object_so_a_delete_supersedes_an_update() {
        let kind = KindId::new("", "v1", "Pod", "pods");
        let row = ResourceRow {
            key: ResourceKey::new("default", "api-0"),
            uid: None,
            cells: Arc::from([] as [Arc<str>; 0]),
            state: crate::resource::RowState::Healthy,
            created: None,
        };
        let applied = ClusterEvent::ResourceApplied {
            cluster: "prod".into(),
            kind: kind.clone(),
            row: Arc::new(row.clone()),
        };
        let deleted = ClusterEvent::ResourceDeleted {
            cluster: "prod".into(),
            kind,
            key: row.key.clone(),
        };

        assert_eq!(applied.coalesce_key(), deleted.coalesce_key());
        assert!(!applied.coalesce_key().unwrap().is_barrier());
    }

    #[test]
    fn a_detail_fetch_and_its_failure_share_a_key() {
        let kind = KindId::new("", "v1", "Pod", "pods");
        let key = ResourceKey::new("default", "api-0");
        let ok = ClusterEvent::Object {
            cluster: "prod".into(),
            kind: kind.clone(),
            detail: Arc::new(crate::resource::ObjectDetail {
                key: key.clone(),
                yaml: Arc::from("kind: Pod"),
                maskable: false,
                revealed: true,
                events: Arc::from([] as [crate::resource::EventLine; 0]),
                owners: Arc::from([] as [crate::resource::OwnerRef; 0]),
            }),
        };
        let failed = ClusterEvent::ObjectFailed {
            cluster: "prod".into(),
            kind,
            key,
            reason: "not found".into(),
        };

        // Both answer the same request; the newer one must win rather than the
        // detail pane showing an error next to the object it just loaded.
        assert_eq!(ok.coalesce_key(), failed.coalesce_key());
    }

    #[test]
    fn a_successful_kubeconfig_read_replaces_a_previous_failure() {
        let failed = ClusterEvent::ConfigFailed {
            reason: "no such file".into(),
        };
        let read = ClusterEvent::Contexts {
            contexts: Arc::from([] as [ContextInfo; 0]),
            current: None,
        };
        assert_eq!(failed.coalesce_key(), read.coalesce_key());
    }

    #[test]
    fn log_batches_are_never_coalesced() {
        let batch = ClusterEvent::LogBatch {
            cluster: "prod".into(),
            lines: Arc::from([] as [crate::logs::LogLine; 0]),
        };
        // Collapsing two batches would silently discard log lines, which is the
        // one thing a log view may never do.
        assert_eq!(batch.coalesce_key(), None);
    }

    #[test]
    fn log_source_states_collapse_per_source() {
        let attached = ClusterEvent::LogSourceChanged {
            cluster: "prod".into(),
            source: LogSource::new("api-0", "api"),
            state: LogSourceState::Streaming,
        };
        let ended = ClusterEvent::LogSourceChanged {
            cluster: "prod".into(),
            source: LogSource::new("api-0", "api"),
            state: LogSourceState::Ended,
        };
        let other = ClusterEvent::LogSourceChanged {
            cluster: "prod".into(),
            source: LogSource::new("api-1", "api"),
            state: LogSourceState::Streaming,
        };

        assert_eq!(attached.coalesce_key(), ended.coalesce_key());
        assert_ne!(attached.coalesce_key(), other.coalesce_key());
    }

    #[test]
    fn a_resync_does_not_supersede_log_events() {
        // Log lines have nothing to do with a resource listing; a resync that
        // swallowed them would lose text.
        let reset = EventKey::ResourceReset("prod".into(), KindId::new("", "v1", "Pod", "pods"));
        assert!(!reset.supersedes(&EventKey::LogSource(
            "prod".into(),
            LogSource::new("api-0", "api")
        )));
        assert!(!reset.supersedes(&EventKey::LogsFailed("prod".into())));
    }

    #[test]
    fn mutation_outcomes_are_never_collapsed() {
        // Two deletes are two things that happened. Coalescing them would mean
        // the audit trail on screen disagreed with the one on disk.
        let done = ClusterEvent::MutationDone {
            cluster: "prod".into(),
            mutation: Arc::new(crate::mutation::Mutation::Delete {
                kind: KindId::new("", "v1", "Pod", "pods"),
                key: ResourceKey::new("default", "api-0"),
                grace_period: None,
            }),
            outcome: crate::mutation::MutationOutcome::Applied {
                detail: "deleted".into(),
            },
        };
        assert_eq!(done.coalesce_key(), None);
    }

    #[test]
    fn commands_that_address_no_cluster_say_so() {
        assert_eq!(ClusterCommand::ListContexts.cluster(), None);
        assert_eq!(
            ClusterCommand::Connect {
                cluster: "prod".into()
            }
            .cluster()
            .map(ClusterId::as_str),
            Some("prod")
        );
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
