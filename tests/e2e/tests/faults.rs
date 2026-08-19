//! Fault injection: the apiserver going away in the middle of a watch.
//!
//! `IMPLEMENTATION.md` §4 lists this as a test rather than an edge case, and
//! §"Error handling philosophy" says what the answer has to be: no empty tables
//! that silently mean something broke, and no spinner without a reason.
//!
//! The apiserver is not really killed — that would take the cluster down for
//! every other test in the run. A proxy is interposed and cut instead, which
//! breaks the socket the app is actually holding. See `periscope_e2e::proxy`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, ConnectionState, EventStream,
    RuntimeConfig,
};
use periscope_cluster::KubeHandler;
use periscope_e2e::exec::Scratch;
use periscope_e2e::proxy::Proxy;
use periscope_e2e::{describe, pods, wait_for};

/// Discovery through a proxy, on a cluster holding the load fixture.
const TIMEOUT: Duration = Duration::from_secs(60);

/// How long a broken watch has to report itself.
///
/// It should be immediate — the socket is reset, not idle — but a busy CI box
/// deserves room before this fails.
const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long recovery may take once the apiserver is back.
///
/// `kube`'s watcher backs off between attempts, so this is deliberately longer
/// than the failure budget.
const RECOVER_TIMEOUT: Duration = Duration::from_secs(90);

/// The context name in the kubeconfig these tests write.
const CONTEXT: &str = "through-proxy";

/// Writes a kubeconfig whose one context reaches the cluster through the proxy.
///
/// Everything but the server address is copied from the real context, so this
/// is the same cluster, the same CA and the same credentials — reached over a
/// socket the test can cut. Those credentials are `kind`'s admin certificate
/// and key, which is why this goes through `write_kubeconfig` rather than
/// `std::fs::write`.
fn kubeconfig_through(directory: &Path, port: u16) -> PathBuf {
    let mut cluster = periscope_e2e::exec::test_cluster();
    cluster["name"] = serde_json::json!(CONTEXT);
    cluster["cluster"]["server"] = serde_json::json!(format!("https://127.0.0.1:{port}"));

    let (certificate, key) = periscope_e2e::exec::test_client_certificate()
        .expect("the test context uses client certificates");

    let config = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "current-context": CONTEXT,
        "clusters": [cluster],
        "contexts": [{
            "name": CONTEXT,
            "context": { "cluster": CONTEXT, "user": "test-user" }
        }],
        "users": [{
            "name": "test-user",
            "user": {
                "client-certificate-data": certificate,
                "client-key-data": key
            }
        }]
    });

    periscope_e2e::exec::write_kubeconfig(directory, "proxied.kubeconfig", &config)
}

/// Connects through the proxy and watches pods until rows arrive.
fn watching_through(proxy: &Proxy, scratch: &Scratch) -> (ClusterRuntime, EventStream, ClusterId) {
    let kubeconfig = kubeconfig_through(scratch.path(), proxy.port());
    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig(&kubeconfig),
        RuntimeConfig::default(),
    )
    .expect("the runtime starts");
    let cluster = ClusterId::new(CONTEXT);

    runtime
        .send(ClusterCommand::Connect {
            cluster: cluster.clone(),
        })
        .expect("connect is queued");
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    // One namespace, so the watch is a small list and the test is about the
    // interruption rather than about ten thousand rows.
    runtime
        .send(ClusterCommand::Watch {
            cluster: cluster.clone(),
            kind: pods(),
            namespace: Some(Arc::from("default")),
            selector: None,
        })
        .expect("watch is queued");
    wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { rows, .. } if !rows.is_empty()),
    )
    .unwrap_or_else(|seen| panic!("no rows ever arrived; saw: {}", describe(&seen)));

    (runtime, stream, cluster)
}

#[test]
#[ignore = "needs a cluster"]
fn an_apiserver_that_goes_away_mid_watch_is_reported_rather_than_frozen() {
    let scratch = Scratch::new("fault-degraded");
    let proxy = Proxy::to_test_cluster().expect("the proxy starts");
    let (_runtime, stream, _cluster) = watching_through(&proxy, &scratch);

    proxy.interrupt();

    let (event, seen) = wait_for(&stream, REPORT_TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Degraded { .. }
                    | ConnectionState::Disconnected { reason: Some(_) }
                    | ConnectionState::AuthFailed { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| {
        panic!(
            "a watch whose apiserver vanished must say so; saw: {}",
            describe(&seen)
        )
    });

    let ClusterEvent::Status { state, .. } = &event else {
        unreachable!()
    };

    // The whole point: a reason, not a silent stall.
    let reason = state.detail().expect("the state carries a reason");
    assert!(!reason.is_empty(), "{state:?}");
    // And it says which kind stopped, so it does not read as the whole cluster
    // having failed.
    assert!(reason.contains("pods"), "{reason}");

    // The rows already on screen are not thrown away. An empty table would be
    // indistinguishable from a namespace with nothing in it, which is exactly
    // the confusion the error philosophy forbids.
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ClusterEvent::ResourceReset { rows, .. } if rows.is_empty()
        )),
        "the table was cleared instead of the failure being reported"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_watch_recovers_by_itself_when_the_apiserver_comes_back() {
    let scratch = Scratch::new("fault-recover");
    let proxy = Proxy::to_test_cluster().expect("the proxy starts");
    let (_runtime, stream, _cluster) = watching_through(&proxy, &scratch);

    proxy.interrupt();
    wait_for(&stream, REPORT_TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Degraded { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| panic!("the break was never reported; saw: {}", describe(&seen)));

    proxy.restore();

    // Nobody clicked anything: the watcher's own backoff brings it back, and
    // the cluster must say so on its own. Recovery is *not* asserted through a
    // resync — `kube` resumes an interrupted watch from the resource version it
    // last saw, so a brief outage produces no fresh list at all, which is
    // exactly how this went unnoticed until the fault was injected.
    wait_for(&stream, RECOVER_TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Connected,
                ..
            }
        )
    })
    .unwrap_or_else(|seen| {
        panic!(
            "the watch must report itself healthy again, not stay degraded; saw: {}",
            describe(&seen)
        )
    });

    // And it is really streaming, not merely claiming to be: a pod created
    // afterwards has to arrive.
    let probe = "periscope-fault-probe";
    let _ = periscope_e2e::delete_probe_pod(probe);
    periscope_e2e::create_probe_pod(probe).expect("the probe pod is created");

    let arrived = wait_for(
        &stream,
        RECOVER_TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceApplied { row, .. } if &*row.key.name == probe),
    );
    let _ = periscope_e2e::delete_probe_pod(probe);
    arrived.unwrap_or_else(|seen| {
        panic!(
            "the recovered watch is not actually streaming; saw: {}",
            describe(&seen)
        )
    });
}

#[test]
#[ignore = "needs a cluster"]
fn an_apiserver_that_never_answers_fails_the_connection_with_a_reason() {
    // The other half of the fault: not a watch breaking, but the apiserver
    // being gone before anything was established.
    let scratch = Scratch::new("fault-cold");
    let proxy = Proxy::to_test_cluster().expect("the proxy starts");
    proxy.interrupt();

    let kubeconfig = kubeconfig_through(scratch.path(), proxy.port());
    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig(&kubeconfig),
        RuntimeConfig::default(),
    )
    .expect("the runtime starts");

    runtime
        .send(ClusterCommand::Connect {
            cluster: ClusterId::new(CONTEXT),
        })
        .expect("connect is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Disconnected { reason: Some(_) }
                    | ConnectionState::AuthFailed { .. }
                    | ConnectionState::Degraded { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| {
        panic!(
            "connecting to a dead apiserver must fail, not hang; saw: {}",
            describe(&seen)
        )
    });

    let ClusterEvent::Status { state, .. } = &event else {
        unreachable!()
    };
    assert!(
        state.detail().is_some_and(|reason| !reason.is_empty()),
        "{state:?}"
    );
}
