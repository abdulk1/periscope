//! The command handler: one session per connected cluster.
//!
//! Everything here runs on the tokio runtime owned by the bridge. A session is
//! a task holding a `kube::Client` and a pod watch; connecting starts one,
//! disconnecting aborts it. Nothing is shared between clusters, so one
//! unreachable cluster cannot stall another.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::future::BoxFuture;
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, CommandHandler, ConnectionState, EventSink,
};
use tokio::task::JoinHandle;

use crate::errors::Failure;
use crate::{kubeconfig, watch};

/// Connects to clusters and streams their pods.
#[derive(Debug, Default)]
pub struct KubeHandler {
    sessions: Arc<Sessions>,
    /// Where kubeconfig comes from; `None` is the standard search.
    source: kubeconfig::Source,
}

/// The live sessions, keyed by context name.
///
/// A plain `std::sync::Mutex` is right here: it is held only long enough to
/// insert or take a `JoinHandle`, never across an await.
#[derive(Debug, Default)]
struct Sessions {
    tasks: Mutex<HashMap<ClusterId, JoinHandle<()>>>,
}

impl Sessions {
    /// Registers a session, aborting any it replaces.
    fn insert(&self, cluster: ClusterId, task: JoinHandle<()>) {
        if let Some(previous) = self.lock().insert(cluster, task) {
            previous.abort();
        }
    }

    /// Removes a session and stops it.
    fn abort(&self, cluster: &ClusterId) -> bool {
        match self.lock().remove(cluster) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    /// Whether a session is running for this cluster.
    fn is_live(&self, cluster: &ClusterId) -> bool {
        self.lock()
            .get(cluster)
            .is_some_and(|task| !task.is_finished())
    }

    /// Stops every session.
    fn abort_all(&self) {
        for (_, task) in self.lock().drain() {
            task.abort();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ClusterId, JoinHandle<()>>> {
        // A poisoned lock here would mean a panic while holding a map of join
        // handles; the map itself cannot be left inconsistent, so recovering is
        // strictly better than taking the whole app down with it.
        self.tasks.lock().unwrap_or_else(|poisoned| {
            tracing::error!("session map lock was poisoned; recovering");
            poisoned.into_inner()
        })
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
            sessions: Arc::default(),
            source: Some(path.into()),
        }
    }

    /// How many sessions are currently running.
    pub fn live_sessions(&self) -> usize {
        self.sessions
            .lock()
            .values()
            .filter(|task| !task.is_finished())
            .count()
    }
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

/// Builds a client and watches pods until the session is stopped.
async fn session(cluster: ClusterId, source: kubeconfig::Source, events: EventSink) {
    let client = match kubeconfig::connect(&cluster, source).await {
        Ok(client) => client,
        Err(error) => {
            let failure = error.failure();
            tracing::warn!(%cluster, reason = failure.message(), "connection failed");
            let state = match failure {
                Failure::Auth(reason) => ConnectionState::AuthFailed { reason },
                Failure::Other(reason) => ConnectionState::Disconnected {
                    reason: Some(reason),
                },
            };
            events.send(ClusterEvent::Status { cluster, state });
            return;
        }
    };

    watch::run(cluster, client, events).await;
}

impl CommandHandler for KubeHandler {
    fn handle(&self, command: ClusterCommand, events: EventSink) -> BoxFuture<'static, ()> {
        let sessions = Arc::clone(&self.sessions);
        let source = self.source.clone();

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

                    let task = tokio::spawn(session(cluster.clone(), source, events));
                    sessions.insert(cluster, task);
                }

                ClusterCommand::Disconnect { cluster } => {
                    let was_live = sessions.abort(&cluster);
                    tracing::info!(%cluster, was_live, "disconnect requested");

                    // Report an empty table rather than leaving the last known
                    // rows on screen: after a disconnect we no longer know what
                    // is running, and stale rows that look live are worse than
                    // none.
                    events.send(ClusterEvent::PodsReset {
                        cluster: cluster.clone(),
                        pods: Arc::from([] as [periscope_bridge::PodSnapshot; 0]),
                    });
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
    use periscope_bridge::{ClusterRuntime, EventStream, RuntimeConfig};
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
    fn disconnecting_clears_the_table_and_reports_a_deliberate_stop() {
        let (runtime, stream) =
            ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default()).unwrap();

        runtime
            .send(ClusterCommand::Disconnect {
                cluster: "prod".into(),
            })
            .unwrap();

        match recv_timeout(&stream, Duration::from_secs(5)) {
            Some(ClusterEvent::PodsReset { pods, .. }) => assert!(pods.is_empty()),
            other => panic!("expected the table to be cleared, got {other:?}"),
        }
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
