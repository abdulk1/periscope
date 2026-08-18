//! The command handler: one session per connected cluster, one task per watch.
//!
//! Everything here runs on the tokio runtime owned by the bridge. A session
//! holds a `kube::Client`; each watched kind is its own task under it, so
//! switching kinds tears down exactly one stream and an unreachable cluster
//! cannot stall another.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::future::BoxFuture;
use kube::Client;
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ColumnSpec, CommandHandler, ConnectionState,
    EventSink, KindId, KindInfo, ResourceRow,
};
use tokio::task::JoinHandle;

use crate::errors::Failure;
use crate::mutate::WritePolicy;
use crate::watch::Filter;
use crate::{detail, discovery, exec, forward, kubeconfig, logs, mutate, watch};

/// Connects to clusters, discovers what they serve, and streams it.
#[derive(Debug, Default)]
pub struct KubeHandler {
    sessions: Arc<Sessions>,
    /// Where kubeconfig comes from; `None` is the standard search.
    source: kubeconfig::Source,
    /// Which clusters this process will change. Checked again here even though
    /// the store checks first: see `docs/DECISIONS.md` ADR-0028.
    policy: Arc<WritePolicy>,
    /// Where actions are recorded. `None` disables the log, which is only ever
    /// the case in tests.
    audit: Option<Arc<periscope_config::AuditLog>>,
}

/// One connected cluster.
#[derive(Clone)]
struct Session {
    client: Client,
    credential_plugin: Option<String>,
    /// What discovery said, so a watch knows whether its kind is namespaced.
    kinds: Arc<Vec<KindInfo>>,
}

/// The live sessions and watches.
///
/// Plain `std::sync::Mutex`es are right here: they are held only long enough to
/// insert or take a handle, never across an await.
#[derive(Default)]
struct Sessions {
    clusters: Mutex<HashMap<ClusterId, Session>>,
    tasks: Mutex<HashMap<ClusterId, JoinHandle<()>>>,
    watches: Mutex<HashMap<(ClusterId, KindId), JoinHandle<()>>>,
    /// One log session per cluster: one log view, one tail.
    logs: Mutex<HashMap<ClusterId, JoinHandle<()>>>,
    /// One command per cluster, for the same reason: there is one output pane.
    /// Starting another replaces it, which is also how it is cancelled.
    execs: Mutex<HashMap<ClusterId, JoinHandle<()>>>,
    /// Port forwards, which outlive the view that started them.
    forwards: Mutex<Forwards>,
}

/// The running forwards, by cluster and id.
///
/// The target is kept alongside the task so tearing one down can still say what
/// it was pointing at.
type Forwards = HashMap<
    (ClusterId, periscope_bridge::ForwardId),
    (JoinHandle<()>, Arc<periscope_bridge::ForwardTarget>),
>;

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("clusters", &guard(&self.clusters).len())
            .field("watches", &guard(&self.watches).len())
            .finish()
    }
}

/// Recovers a poisoned lock rather than taking the app down with it: these maps
/// hold task handles, and a handle map cannot be left half-updated.
fn guard<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        tracing::error!("a session lock was poisoned; recovering");
        poisoned.into_inner()
    })
}

impl Sessions {
    /// Registers a session task, aborting any it replaces.
    fn insert_task(&self, cluster: ClusterId, task: JoinHandle<()>) {
        if let Some(previous) = guard(&self.tasks).insert(cluster, task) {
            previous.abort();
        }
    }

    /// Records a connected client.
    fn connected(&self, cluster: ClusterId, session: Session) {
        guard(&self.clusters).insert(cluster, session);
    }

    /// The session for a cluster, if it is connected.
    fn session(&self, cluster: &ClusterId) -> Option<Session> {
        guard(&self.clusters).get(cluster).cloned()
    }

    /// Records what discovery found.
    fn set_kinds(&self, cluster: &ClusterId, kinds: Vec<KindInfo>) {
        if let Some(session) = guard(&self.clusters).get_mut(cluster) {
            session.kinds = Arc::new(kinds);
        }
    }

    /// Registers a watch, aborting any watch it replaces. Replacing is how a
    /// namespace or selector change is applied.
    fn insert_watch(&self, cluster: ClusterId, kind: KindId, task: JoinHandle<()>) {
        if let Some(previous) = guard(&self.watches).insert((cluster, kind), task) {
            previous.abort();
        }
    }

    /// Starts a log session, stopping the one it replaces.
    fn insert_logs(&self, cluster: ClusterId, task: JoinHandle<()>) {
        if let Some(previous) = guard(&self.logs).insert(cluster, task) {
            previous.abort();
        }
    }

    /// Stops a cluster's log session.
    fn stop_logs(&self, cluster: &ClusterId) -> bool {
        match guard(&self.logs).remove(cluster) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    /// Starts a command, cancelling the one it replaces.
    fn insert_exec(&self, cluster: ClusterId, task: JoinHandle<()>) {
        if let Some(previous) = guard(&self.execs).insert(cluster, task) {
            previous.abort();
        }
    }

    /// Cancels a cluster's running command.
    fn stop_exec(&self, cluster: &ClusterId) -> bool {
        match guard(&self.execs).remove(cluster) {
            Some(task) => {
                let running = !task.is_finished();
                task.abort();
                running
            }
            None => false,
        }
    }

    /// Registers a forward, replacing one with the same id.
    fn insert_forward(
        &self,
        cluster: ClusterId,
        id: periscope_bridge::ForwardId,
        task: JoinHandle<()>,
        target: Arc<periscope_bridge::ForwardTarget>,
    ) {
        if let Some((previous, _)) = guard(&self.forwards).insert((cluster, id), (task, target)) {
            previous.abort();
        }
    }

    /// Tears one forward down, returning what it was pointing at.
    fn stop_forward(
        &self,
        cluster: &ClusterId,
        id: periscope_bridge::ForwardId,
    ) -> Option<Arc<periscope_bridge::ForwardTarget>> {
        let (task, target) = guard(&self.forwards).remove(&(cluster.clone(), id))?;
        task.abort();
        Some(target)
    }

    /// Stops one watch.
    fn stop_watch(&self, cluster: &ClusterId, kind: &KindId) -> bool {
        match guard(&self.watches).remove(&(cluster.clone(), kind.clone())) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    /// Stops everything belonging to a cluster.
    fn disconnect(&self, cluster: &ClusterId) -> bool {
        guard(&self.clusters).remove(cluster);
        self.stop_logs(cluster);
        self.stop_exec(cluster);

        // A forward whose cluster has gone can only fail; taking it down is
        // more honest than leaving a listener that accepts and then errors.
        let mut forwards = guard(&self.forwards);
        let ids: Vec<_> = forwards
            .keys()
            .filter(|(forwarded, _)| forwarded == cluster)
            .cloned()
            .collect();
        for id in ids {
            if let Some((task, _)) = forwards.remove(&id) {
                task.abort();
            }
        }
        drop(forwards);

        let mut watches = guard(&self.watches);
        let keys: Vec<_> = watches
            .keys()
            .filter(|(watched, _)| watched == cluster)
            .cloned()
            .collect();
        for key in keys {
            if let Some(task) = watches.remove(&key) {
                task.abort();
            }
        }
        drop(watches);

        match guard(&self.tasks).remove(cluster) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    /// Whether a cluster is connected or connecting.
    fn is_live(&self, cluster: &ClusterId) -> bool {
        guard(&self.clusters).contains_key(cluster)
            || guard(&self.tasks)
                .get(cluster)
                .is_some_and(|task| !task.is_finished())
    }

    /// Stops every session, watch, log tail and forward.
    fn abort_all(&self) {
        for (_, (task, _)) in guard(&self.forwards).drain() {
            task.abort();
        }
        for (_, task) in guard(&self.execs).drain() {
            task.abort();
        }
        for (_, task) in guard(&self.logs).drain() {
            task.abort();
        }
        for (_, task) in guard(&self.watches).drain() {
            task.abort();
        }
        for (_, task) in guard(&self.tasks).drain() {
            task.abort();
        }
        guard(&self.clusters).clear();
    }
}

impl KubeHandler {
    /// A handler that reads kubeconfig from the standard locations.
    pub fn new() -> Self {
        Self::default()
    }

    /// A handler that reads one specific kubeconfig file.
    pub fn with_kubeconfig(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            source: Some(path.into()),
            ..Self::default()
        }
    }

    /// Sets which clusters may be changed.
    pub fn with_policy(mut self, policy: WritePolicy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Sets where actions are recorded.
    pub fn with_audit(mut self, audit: periscope_config::AuditLog) -> Self {
        self.audit = Some(Arc::new(audit));
        self
    }

    /// How many clusters are currently connected.
    pub fn live_sessions(&self) -> usize {
        guard(&self.sessions.clusters).len()
    }
}

/// The kind the app opens by default.
pub fn default_kind() -> KindId {
    KindId::new("", "v1", "Pod", "pods")
}

/// Reads kubeconfig off the runtime's worker threads and reports the result.
async fn list_contexts(source: kubeconfig::Source, events: EventSink) {
    let read = tokio::task::spawn_blocking(move || kubeconfig::read(&source)).await;

    let event = match read {
        Ok(Ok(contexts)) => ClusterEvent::Contexts {
            contexts: Arc::from(contexts.contexts),
            current: contexts.current,
        },
        Ok(Err(error)) => ClusterEvent::ConfigFailed {
            reason: crate::errors::describe(&error),
        },
        Err(join) => ClusterEvent::ConfigFailed {
            reason: format!("reading kubeconfig panicked: {join}"),
        },
    };

    events.send(event);
}

/// Reports a failure against a cluster, split by whether it is an auth problem.
fn report(cluster: ClusterId, failure: Failure, events: &EventSink) {
    let state = match failure {
        Failure::Auth(reason) => ConnectionState::AuthFailed { reason },
        Failure::Other(reason) => ConnectionState::Disconnected {
            reason: Some(reason),
        },
    };
    events.send(ClusterEvent::Status { cluster, state });
}

/// Builds a client and discovers what the cluster serves.
async fn connect(
    cluster: ClusterId,
    source: kubeconfig::Source,
    sessions: Arc<Sessions>,
    events: EventSink,
) {
    let connection = match kubeconfig::connect(&cluster, source).await {
        Ok(connection) => connection,
        Err(error) => {
            let failure = error.failure();
            tracing::warn!(%cluster, reason = failure.message(), "connection failed");
            report(cluster, failure, &events);
            return;
        }
    };

    sessions.connected(
        cluster.clone(),
        Session {
            client: connection.client.clone(),
            credential_plugin: connection.credential_plugin.clone(),
            kinds: Arc::default(),
        },
    );

    // Discovery is the first request the client makes, so it is also where a
    // credential that will not work shows up.
    match discovery::kinds(connection.client).await {
        Ok(kinds) => {
            tracing::info!(%cluster, kinds = kinds.len(), "discovered");
            sessions.set_kinds(&cluster, kinds.clone());
            events.send(ClusterEvent::Kinds {
                cluster: cluster.clone(),
                kinds: Arc::from(kinds),
            });
            events.send(ClusterEvent::Status {
                cluster,
                state: ConnectionState::Connected,
            });
        }
        Err(error) => {
            let failure = match crate::errors::classify(&error) {
                Failure::Auth(reason) => Failure::Auth(crate::errors::attribute_plugin(
                    reason,
                    connection.credential_plugin.as_deref(),
                )),
                other => other,
            };
            tracing::warn!(%cluster, reason = failure.message(), "discovery failed");
            sessions.disconnect(&cluster);
            report(cluster, failure, &events);
        }
    }
}

/// Whether a kind is namespaced, according to discovery.
///
/// An unknown kind is assumed namespaced: that is true of the overwhelming
/// majority, and the namespace filter is simply ignored if it turns out not to
/// be.
fn is_namespaced(session: &Session, kind: &KindId) -> bool {
    session
        .kinds
        .iter()
        .find(|known| &known.id == kind)
        .is_none_or(|info| info.namespaced)
}

impl CommandHandler for KubeHandler {
    fn handle(&self, command: ClusterCommand, events: EventSink) -> BoxFuture<'static, ()> {
        let sessions = Arc::clone(&self.sessions);
        let source = self.source.clone();
        let policy = Arc::clone(&self.policy);
        let audit = self.audit.clone();

        Box::pin(async move {
            match command {
                ClusterCommand::ListContexts => list_contexts(source, events).await,

                ClusterCommand::Connect { cluster } => {
                    if sessions.is_live(&cluster) {
                        tracing::debug!(%cluster, "already connected");
                        return;
                    }

                    events.send(ClusterEvent::Status {
                        cluster: cluster.clone(),
                        state: ConnectionState::Connecting,
                    });

                    let task = tokio::spawn(connect(
                        cluster.clone(),
                        source,
                        Arc::clone(&sessions),
                        events,
                    ));
                    sessions.insert_task(cluster, task);
                }

                ClusterCommand::Discover { cluster } => {
                    let Some(session) = sessions.session(&cluster) else {
                        tracing::debug!(%cluster, "discover requested before connecting");
                        return;
                    };

                    match discovery::kinds(session.client).await {
                        Ok(kinds) => {
                            sessions.set_kinds(&cluster, kinds.clone());
                            events.send(ClusterEvent::Kinds {
                                cluster,
                                kinds: Arc::from(kinds),
                            });
                        }
                        Err(error) => report(cluster, crate::errors::classify(&error), &events),
                    };
                }

                ClusterCommand::Watch {
                    cluster,
                    kind,
                    namespace,
                    selector,
                } => {
                    let Some(session) = sessions.session(&cluster) else {
                        tracing::debug!(%cluster, %kind, "watch requested before connecting");
                        return;
                    };

                    let namespaced = is_namespaced(&session, &kind);
                    let task = tokio::spawn(watch::run(
                        cluster.clone(),
                        kind.clone(),
                        session.client,
                        namespaced,
                        Filter {
                            namespace,
                            selector,
                        },
                        session.credential_plugin,
                        events,
                    ));
                    sessions.insert_watch(cluster, kind, task);
                }

                ClusterCommand::StopWatch { cluster, kind } => {
                    let stopped = sessions.stop_watch(&cluster, &kind);
                    tracing::debug!(%cluster, %kind, stopped, "watch stopped");

                    // Say the table is empty rather than leaving rows nothing is
                    // updating any more.
                    events.send(ClusterEvent::ResourceReset {
                        cluster,
                        kind,
                        columns: Arc::from([] as [ColumnSpec; 0]),
                        rows: Arc::from([] as [ResourceRow; 0]),
                    });
                }

                ClusterCommand::FetchObject {
                    cluster,
                    kind,
                    key,
                    reveal,
                } => {
                    let Some(session) = sessions.session(&cluster) else {
                        events.send(ClusterEvent::ObjectFailed {
                            cluster,
                            kind,
                            key,
                            reason: "not connected to this cluster".to_owned(),
                        });
                        return;
                    };

                    let namespaced = is_namespaced(&session, &kind);
                    match detail::fetch(session.client, &kind, namespaced, &key, reveal).await {
                        Ok(detail) => events.send(ClusterEvent::Object {
                            cluster,
                            kind,
                            detail: Arc::new(detail),
                        }),
                        Err(error) => events.send(ClusterEvent::ObjectFailed {
                            cluster,
                            kind,
                            key,
                            reason: crate::errors::describe(&error),
                        }),
                    };
                }

                ClusterCommand::StartLogs { cluster, target } => {
                    let Some(session) = sessions.session(&cluster) else {
                        events.send(ClusterEvent::LogsFailed {
                            cluster,
                            reason: "not connected to this cluster".to_owned(),
                        });
                        return;
                    };

                    tracing::info!(%cluster, target = %target.label(), "tailing logs");
                    let task =
                        tokio::spawn(logs::run(cluster.clone(), session.client, target, events));
                    sessions.insert_logs(cluster, task);
                }

                ClusterCommand::StopLogs { cluster } => {
                    let stopped = sessions.stop_logs(&cluster);
                    tracing::debug!(%cluster, stopped, "log session stopped");
                }

                ClusterCommand::Exec { cluster, target } => {
                    let Some(session) = sessions.session(&cluster) else {
                        events.send(ClusterEvent::ExecFinished {
                            cluster,
                            status: periscope_bridge::ExecStatus::Failed {
                                reason: "not connected to this cluster".to_owned(),
                            },
                        });
                        return;
                    };

                    tracing::info!(%cluster, target = %target.label(), "running a command");
                    let running = cluster.clone();
                    let task = tokio::spawn(async move {
                        exec::run(
                            running,
                            session.client,
                            target,
                            &policy,
                            audit.as_deref(),
                            events,
                        )
                        .await;
                    });
                    sessions.insert_exec(cluster, task);
                }

                ClusterCommand::CancelExec { cluster } => {
                    let running = sessions.stop_exec(&cluster);
                    tracing::info!(%cluster, running, "command cancelled");

                    // The task is gone, so it cannot report its own ending.
                    if running {
                        events.send(ClusterEvent::ExecFinished {
                            cluster,
                            status: periscope_bridge::ExecStatus::Cancelled,
                        });
                    }
                }

                ClusterCommand::StartForward {
                    cluster,
                    id,
                    target,
                } => {
                    let Some(session) = sessions.session(&cluster) else {
                        events.send(ClusterEvent::ForwardChanged {
                            cluster,
                            forward: Arc::new(periscope_bridge::ForwardInfo {
                                state: periscope_bridge::ForwardState::Failed {
                                    reason: "not connected to this cluster".to_owned(),
                                },
                                ..periscope_bridge::ForwardInfo::starting(id, target)
                            }),
                        });
                        return;
                    };

                    let task = tokio::spawn(forward::run(
                        cluster.clone(),
                        id,
                        session.client,
                        Arc::clone(&target),
                        events,
                    ));
                    sessions.insert_forward(cluster, id, task, target);
                }

                ClusterCommand::StopForward { cluster, id } => {
                    let target = sessions.stop_forward(&cluster, id);
                    tracing::info!(%cluster, %id, stopped = target.is_some(), "forward stopped");

                    // The task is gone, so it cannot report its own ending.
                    if let Some(target) = target {
                        events.send(ClusterEvent::ForwardChanged {
                            cluster,
                            forward: Arc::new(periscope_bridge::ForwardInfo {
                                state: periscope_bridge::ForwardState::Stopped,
                                ..periscope_bridge::ForwardInfo::starting(id, target)
                            }),
                        });
                    }
                }

                ClusterCommand::Mutate { cluster, mutation } => {
                    let Some(session) = sessions.session(&cluster) else {
                        events.send(ClusterEvent::MutationDone {
                            cluster,
                            mutation,
                            outcome: periscope_bridge::MutationOutcome::Refused {
                                reason: "not connected to this cluster".to_owned(),
                            },
                        });
                        return;
                    };

                    let namespaced = is_namespaced(&session, &mutation.kind());
                    let outcome = mutate::run(
                        cluster.clone(),
                        session.client,
                        Arc::clone(&mutation),
                        namespaced,
                        &policy,
                        audit.as_deref(),
                    )
                    .await;

                    tracing::info!(
                        %cluster,
                        verb = mutation.verb(),
                        object = %mutation.key(),
                        outcome = outcome.message(),
                        "mutation"
                    );
                    events.send(ClusterEvent::MutationDone {
                        cluster,
                        mutation,
                        outcome,
                    });
                }

                ClusterCommand::Disconnect { cluster } => {
                    let was_live = sessions.disconnect(&cluster);
                    tracing::info!(%cluster, was_live, "disconnect requested");

                    events.send(ClusterEvent::Status {
                        cluster,
                        state: ConnectionState::Disconnected { reason: None },
                    });
                }

                ClusterCommand::Ping { cluster, nonce } => {
                    let started = Instant::now();
                    // A bridge liveness probe, deliberately not an API call: it
                    // answers "is the runtime scheduling work", which is what
                    // --perf wants to know.
                    tokio::task::yield_now().await;
                    events.send(ClusterEvent::Pong {
                        cluster,
                        nonce,
                        elapsed: started.elapsed(),
                    });
                }
            }
        })
    }

    fn shutdown(&self, _events: EventSink) -> BoxFuture<'static, ()> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            sessions.abort_all();
            tracing::info!("cluster layer shutting down");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::{ClusterRuntime, EventStream, ResourceKey, RuntimeConfig};
    use std::time::Duration;

    fn recv_timeout(stream: &EventStream, timeout: Duration) -> Option<ClusterEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(event) = stream.try_recv() {
                return Some(event);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }

    /// Collects events until `wanted` matches one, or the deadline passes.
    fn wait_for(
        stream: &EventStream,
        timeout: Duration,
        mut wanted: impl FnMut(&ClusterEvent) -> bool,
    ) -> Option<ClusterEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(event) = recv_timeout(stream, Duration::from_millis(50))
                && wanted(&event)
            {
                return Some(event);
            }
        }
        None
    }

    #[test]
    fn a_ping_is_answered_with_a_matching_pong() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::Ping {
                cluster: "kind-periscope".into(),
                nonce: 99,
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::Pong { cluster, nonce, .. }) => {
                assert_eq!(cluster.as_str(), "kind-periscope");
                assert_eq!(nonce, 99);
            }
            other => panic!("expected a pong, got {other:?}"),
        }
    }

    #[test]
    fn connecting_to_a_context_that_does_not_exist_fails_with_a_reason() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::Connect {
                cluster: "no-such-context-c0ffee".into(),
            })
            .unwrap();

        let event = wait_for(&stream, Duration::from_secs(10), |event| {
            matches!(
                event,
                ClusterEvent::Status {
                    state: ConnectionState::Disconnected { reason: Some(_) }
                        | ConnectionState::AuthFailed { .. },
                    ..
                }
            )
        })
        .expect("a failed connection is reported");

        // Never an empty table with no explanation.
        let ClusterEvent::Status { state, .. } = &event else {
            unreachable!()
        };
        assert!(
            state.detail().is_some_and(|text| !text.is_empty()),
            "{state:?}"
        );
    }

    #[test]
    fn watching_before_connecting_does_nothing_rather_than_panicking() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::Watch {
                cluster: "not-connected".into(),
                kind: default_kind(),
                namespace: None,
                selector: None,
            })
            .unwrap();

        assert!(recv_timeout(&stream, Duration::from_millis(500)).is_none());
    }

    #[test]
    fn fetching_an_object_without_a_connection_says_so() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::FetchObject {
                cluster: "not-connected".into(),
                kind: default_kind(),
                key: ResourceKey::new("default", "api-0"),
                reveal: false,
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::ObjectFailed { reason, .. }) => {
                assert!(reason.contains("not connected"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn tailing_logs_without_a_connection_says_so() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::StartLogs {
                cluster: "not-connected".into(),
                target: Arc::new(periscope_bridge::LogTarget::pod("default", "api-0")),
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::LogsFailed { reason, .. }) => {
                assert!(reason.contains("not connected"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn stopping_a_log_session_that_is_not_running_is_harmless() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::StopLogs {
                cluster: "prod".into(),
            })
            .unwrap();

        assert!(recv_timeout(&stream, Duration::from_millis(300)).is_none());
    }

    #[test]
    fn stopping_a_watch_clears_its_table() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::StopWatch {
                cluster: "prod".into(),
                kind: default_kind(),
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::ResourceReset { rows, .. }) => assert!(rows.is_empty()),
            other => panic!("expected the table to be cleared, got {other:?}"),
        }
    }

    #[test]
    fn disconnecting_reports_a_deliberate_stop() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::Disconnect {
                cluster: "prod".into(),
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::Status { state, .. }) => {
                assert_eq!(state, ConnectionState::Disconnected { reason: None });
                assert!(!state.is_problem());
            }
            other => panic!("expected a status event, got {other:?}"),
        }
    }

    #[test]
    fn listing_contexts_answers_even_when_kubeconfig_is_missing() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime.send(ClusterCommand::ListContexts).unwrap();

        // Either outcome is fine — this machine may or may not have a
        // kubeconfig — but silence is not.
        match recv_timeout(&stream, Duration::from_secs(10)) {
            Some(ClusterEvent::Contexts { .. } | ClusterEvent::ConfigFailed { .. }) => {}
            other => panic!("expected contexts or a config failure, got {other:?}"),
        }
    }
}
