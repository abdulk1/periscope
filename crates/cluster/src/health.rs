//! The Phase 0 command handler.
//!
//! It answers health probes and nothing else. Phase 1 replaces the body of
//! [`HealthHandler::handle`] with kube clients, watchers and reflectors; the
//! shape — an async fn per command, emitting events into a sink — stays.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::future::BoxFuture;
use periscope_bridge::{ClusterCommand, ClusterEvent, CommandHandler, ConnectionState, EventSink};

/// Answers [`ClusterCommand::Ping`] and tracks how many probes it has served.
#[derive(Debug, Default)]
pub struct HealthHandler {
    served: Arc<AtomicU64>,
}

impl HealthHandler {
    /// A fresh handler.
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared counter of probes served, for diagnostics.
    pub fn served(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.served)
    }
}

impl CommandHandler for HealthHandler {
    fn handle(&self, command: ClusterCommand, events: EventSink) -> BoxFuture<'static, ()> {
        let served = Arc::clone(&self.served);
        Box::pin(async move {
            match command {
                ClusterCommand::Ping { cluster, nonce } => {
                    let started = Instant::now();
                    served.fetch_add(1, Ordering::Relaxed);
                    tracing::trace!(%cluster, nonce, "health probe");

                    // Yield so the probe genuinely round-trips through the tokio
                    // scheduler rather than completing inline.
                    tokio::task::yield_now().await;

                    events.send(ClusterEvent::Pong {
                        cluster,
                        nonce,
                        elapsed: started.elapsed(),
                    });
                }
                ClusterCommand::Disconnect { cluster } => {
                    tracing::info!(%cluster, "disconnect requested");
                    events.send(ClusterEvent::Status {
                        cluster,
                        state: ConnectionState::Disconnected { reason: None },
                    });
                }
            }
        })
    }

    fn shutdown(&self, _events: EventSink) -> BoxFuture<'static, ()> {
        let served = self.served.load(Ordering::Relaxed);
        Box::pin(async move {
            tracing::info!(served, "cluster layer shutting down");
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

    #[test]
    fn a_ping_is_answered_with_a_matching_pong() {
        let handler = HealthHandler::new();
        let served = handler.served();
        let (runtime, stream) = ClusterRuntime::start(handler, RuntimeConfig::default()).unwrap();

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
        assert_eq!(served.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disconnect_reports_a_deliberate_shutdown_not_a_failure() {
        let (runtime, stream) =
            ClusterRuntime::start(HealthHandler::new(), RuntimeConfig::default()).unwrap();

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
}
