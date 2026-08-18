//! Port forwarding.
//!
//! One local TCP listener per forward. Each accepted connection opens its own
//! port-forward stream to the apiserver and copies bytes in both directions
//! until either end hangs up.
//!
//! Per-connection streams are what makes a forward survive a brief
//! interruption: a stream that breaks takes its own connection down, the
//! listener stays bound, and the next connection gets a fresh stream. When the
//! failure is not brief — the pod is gone, the port is not open — every
//! connection fails the same way, and the forward says so in the UI rather than
//! accepting connections that quietly go nowhere.

use std::sync::Arc;

use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};
use periscope_bridge::{
    ClusterEvent, ClusterId, EventSink, ForwardId, ForwardInfo, ForwardState, ForwardTarget,
};
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

/// Forwards are bound to loopback only.
///
/// Binding `0.0.0.0` would expose a cluster-internal service to the network the
/// laptop is on, which is not something a debugging tool should do without
/// being asked very explicitly.
const BIND_ADDRESS: &str = "127.0.0.1";

/// Runs a forward until the task is cancelled.
pub async fn run(
    cluster: ClusterId,
    id: ForwardId,
    client: Client,
    target: Arc<ForwardTarget>,
    events: EventSink,
) {
    let mut info = ForwardInfo::starting(id, Arc::clone(&target));
    report(&cluster, &info, &events);

    let listener = match TcpListener::bind((BIND_ADDRESS, target.local_port.unwrap_or(0))).await {
        Ok(listener) => listener,
        Err(error) => {
            info.state = ForwardState::Failed {
                reason: format!("could not listen on {BIND_ADDRESS}: {error}"),
            };
            report(&cluster, &info, &events);
            return;
        }
    };

    let local_port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            info.state = ForwardState::Failed {
                reason: format!("could not read the local port: {error}"),
            };
            report(&cluster, &info, &events);
            return;
        }
    };

    info.state = ForwardState::Listening { local_port };
    report(&cluster, &info, &events);
    tracing::info!(
        %cluster,
        %id,
        target = %target.label(),
        local_port,
        "forwarding"
    );

    let api: Api<Pod> = Api::namespaced(client, &target.namespace);

    loop {
        let (socket, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // The listener itself is gone; nothing will arrive again.
                info.state = ForwardState::Failed {
                    reason: format!("stopped accepting connections: {error}"),
                };
                report(&cluster, &info, &events);
                return;
            }
        };

        info.connections += 1;
        match forward_one(&api, &target, socket).await {
            Ok(()) => {
                // A connection that worked clears a previous complaint.
                if info.state.is_problem() {
                    info.state = ForwardState::Listening { local_port };
                }
                report(&cluster, &info, &events);
            }
            Err(reason) => {
                // Name the target: the apiserver's own message for a missing
                // pod is "404 Not Found", which says nothing about which pod.
                let reason = format!("{}: {reason}", target.label());
                tracing::warn!(%cluster, %id, %reason, "forwarded connection failed");
                info.state = ForwardState::Degraded { local_port, reason };
                report(&cluster, &info, &events);
            }
        }

        if events
            .send(ClusterEvent::ForwardChanged {
                cluster: cluster.clone(),
                forward: Arc::new(info.clone()),
            })
            .is_closed()
        {
            return;
        }
    }
}

/// Opens one port-forward stream and pumps a connection through it.
async fn forward_one(
    api: &Api<Pod>,
    target: &ForwardTarget,
    mut socket: TcpStream,
) -> Result<(), String> {
    let mut forwarder = api
        .portforward(&target.pod, &[target.remote_port])
        .await
        .map_err(|error| crate::errors::describe(&error))?;

    // The apiserver reports per-port problems — "port not open", "pod not
    // running" — on this channel rather than by failing the request.
    let error = forwarder.take_error(target.remote_port);

    let Some(mut upstream) = forwarder.take_stream(target.remote_port) else {
        return Err(format!(
            "the apiserver did not open port {} on {}",
            target.remote_port, target.pod
        ));
    };

    let copied = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
    let _ = socket.shutdown().await;
    drop(upstream);

    // A failure message from the apiserver is the truth about what happened,
    // and it explains the copy error rather than the other way round.
    if let Some(error) = error
        && let Some(message) = error.await
    {
        return Err(message);
    }

    copied.map(|_| ()).map_err(|error| error.to_string())
}

fn report(cluster: &ClusterId, info: &ForwardInfo, events: &EventSink) {
    events.send(ClusterEvent::ForwardChanged {
        cluster: cluster.clone(),
        forward: Arc::new(info.clone()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_bind_loopback_only() {
        // Binding anything else would put a cluster-internal service on the
        // network the laptop happens to be attached to.
        assert_eq!(BIND_ADDRESS, "127.0.0.1");
    }

    #[tokio::test]
    async fn a_port_that_is_already_taken_fails_loudly() {
        let (sink, stream) = periscope_bridge::event_channel(16);
        let occupied = TcpListener::bind((BIND_ADDRESS, 0)).await.expect("binds");
        let port = occupied.local_addr().expect("has an address").port();

        // No client is needed: this fails before it would be used.
        let target = Arc::new(ForwardTarget::new("default", "api-0", 8080).on_local_port(port));
        let info = ForwardInfo::starting(ForwardId(1), Arc::clone(&target));
        assert_eq!(info.state, ForwardState::Starting);

        let bind = TcpListener::bind((BIND_ADDRESS, port)).await;
        assert!(bind.is_err(), "the port should already be taken");

        // The shape the caller sees: a Failed state carrying the OS's words.
        let failed = ForwardState::Failed {
            reason: format!("could not listen on {BIND_ADDRESS}: {}", bind.unwrap_err()),
        };
        assert!(failed.is_over());
        assert!(
            failed
                .detail()
                .is_some_and(|reason| reason.contains("127.0.0.1"))
        );

        sink.send(ClusterEvent::ForwardChanged {
            cluster: "prod".into(),
            forward: Arc::new(ForwardInfo {
                state: failed,
                ..info
            }),
        });
        assert!(stream.try_recv().is_some());
    }
}
