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
use periscope_config::{AuditEntry, AuditLog, AuditOutcome};

use crate::mutate::WritePolicy;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

/// Forwards are bound to loopback only.
///
/// Binding `0.0.0.0` would expose a cluster-internal service to the network the
/// laptop is on, which is not something a debugging tool should do without
/// being asked very explicitly.
const BIND_ADDRESS: &str = "127.0.0.1";

/// Runs a forward until the task is cancelled.
///
/// A forward changes nothing in the cluster, which is why it needs no
/// confirmation — but `pods/portforward` is the same `create` verb `pods/exec`
/// is, and a tunnel to a production database is not something a cluster marked
/// read-only should hand out. So it passes the same gate exec does and is
/// recorded the same way. See `docs/DECISIONS.md` ADR-0040.
pub async fn run(
    cluster: ClusterId,
    id: ForwardId,
    client: Client,
    target: Arc<ForwardTarget>,
    policy: &WritePolicy,
    audit: Option<&AuditLog>,
    events: EventSink,
) {
    let mut info = ForwardInfo::starting(id, Arc::clone(&target));

    if !policy.may_mutate(&cluster) {
        let reason = format!("{cluster} is read-only; no port is forwarded from it");
        tracing::warn!(%cluster, %id, target = %target.label(), "refused: cluster is read-only");
        record(&cluster, &target, AuditOutcome::Refused, &reason, audit);

        info.state = ForwardState::Failed { reason };
        report(&cluster, &info, &events);
        return;
    }

    // Recorded when it opens rather than when it closes: a tunnel that is still
    // up is the one worth being able to prove afterwards.
    record(&cluster, &target, AuditOutcome::Applied, "", audit);
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

/// Writes one line of the audit log for a forward.
fn record(
    cluster: &ClusterId,
    target: &ForwardTarget,
    outcome: AuditOutcome,
    reason: &str,
    audit: Option<&AuditLog>,
) {
    let Some(audit) = audit else {
        return;
    };

    let entry = AuditEntry::new(
        cluster.as_str(),
        &*target.namespace,
        "pods",
        &*target.pod,
        "port-forward",
    )
    .detail(format!("port {}", target.remote_port))
    .outcome(outcome, crate::redact::text(reason));

    if let Err(error) = audit.append(&entry) {
        tracing::error!(%cluster, %error, "could not write the audit log");
    }
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
    use crate::forward;

    #[test]
    fn forwards_bind_loopback_only() {
        // Binding anything else would put a cluster-internal service on the
        // network the laptop happens to be attached to.
        assert_eq!(BIND_ADDRESS, "127.0.0.1");
    }

    #[tokio::test]
    async fn a_read_only_cluster_forwards_nothing_and_records_the_refusal() {
        // `pods/portforward` is the same `create` verb `pods/exec` is, and a
        // tunnel to a production database is not something a cluster somebody
        // marked read-only should hand out. This runs with a client that would
        // fail if it were ever used, so a gate that stopped refusing fails here
        // rather than reaching the network.
        let (sink, stream) = periscope_bridge::event_channel(16);
        let scratch = std::env::temp_dir().join(format!(
            "periscope-forward-audit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        let audit = periscope_config::AuditLog::at(scratch.join("audit.log"));

        let mut policy = WritePolicy::permissive();
        policy.deny(ClusterId::new("prod"));

        forward::run(
            "prod".into(),
            ForwardId(1),
            Client::try_default()
                .await
                .unwrap_or_else(|_| unreachable!("a client is only built, never used")),
            Arc::new(ForwardTarget::new("payments", "db-0", 5432)),
            &policy,
            Some(&audit),
            sink,
        )
        .await;

        let reported = std::iter::from_fn(|| stream.try_recv())
            .filter_map(|event| match event {
                ClusterEvent::ForwardChanged { forward, .. } => Some(forward),
                _ => None,
            })
            .last()
            .expect("the refusal is reported");
        assert!(
            matches!(&reported.state, ForwardState::Failed { reason } if reason.contains("read-only")),
            "{:?}",
            reported.state
        );

        let entries = audit.read().expect("the audit log is readable");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].action, "port-forward");
        assert_eq!(entries[0].outcome, periscope_config::AuditOutcome::Refused);
        assert_eq!(entries[0].name, "db-0");
        assert!(entries[0].detail.contains("5432"), "{}", entries[0].detail);

        let _ = std::fs::remove_dir_all(&scratch);
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
