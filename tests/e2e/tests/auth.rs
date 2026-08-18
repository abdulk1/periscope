//! Authentication behaviour against a real apiserver.
//!
//! The Phase 1 rule is that a credential the cluster rejects produces a stated
//! auth-failed state — never an empty table and never a hang. That cannot be
//! proven with a mock, so this test points a deliberately bad credential at the
//! real `kind` apiserver and watches what comes back.

use std::time::Duration;

use kube::config::Kubeconfig;
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, ConnectionState, RuntimeConfig,
};
use periscope_cluster::KubeHandler;
use periscope_e2e::{context, describe, wait_for};

/// A rejected credential must be reported, not retried into a hang.
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);

/// Writes a kubeconfig that points at the test cluster with a junk bearer token.
///
/// The cluster stanza — server URL and CA — is copied from the real config, so
/// TLS succeeds and the request gets far enough to be rejected on its
/// credentials rather than failing to connect at all.
fn kubeconfig_with_a_bad_token(directory: &std::path::Path) -> std::path::PathBuf {
    let real = Kubeconfig::read().expect("the machine has a kubeconfig");
    let context_name = context().to_string();

    let context = real
        .contexts
        .iter()
        .find(|named| named.name == context_name)
        .and_then(|named| named.context.clone())
        .unwrap_or_else(|| panic!("context `{context_name}` is not in kubeconfig"));
    let cluster = real
        .clusters
        .iter()
        .find(|named| named.name == context.cluster)
        .expect("the context names a cluster that exists");

    let doctored = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "current-context": context_name,
        "clusters": [cluster],
        "contexts": [{
            "name": context_name,
            "context": { "cluster": context.cluster, "user": "rejected" }
        }],
        "users": [{
            "name": "rejected",
            "user": { "token": "periscope-e2e-not-a-real-token" }
        }]
    });

    let path = directory.join("kubeconfig-bad-token.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&doctored).unwrap())
        .expect("the fixture kubeconfig is written");
    path
}

#[test]
#[ignore = "needs a cluster"]
fn a_credential_the_cluster_rejects_produces_an_auth_failed_state() {
    let directory = std::env::temp_dir().join(format!("periscope-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temp directory");
    let path = kubeconfig_with_a_bad_token(&directory);

    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig(&path),
        RuntimeConfig::default(),
    )
    .expect("the cluster runtime starts");

    runtime
        .send(ClusterCommand::Connect { cluster: context() })
        .expect("connect is queued");

    let (event, seen) = wait_for(&stream, AUTH_TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| {
        panic!(
            "a rejected credential must surface as auth-failed; saw: {}",
            describe(&seen)
        )
    });

    let ClusterEvent::Status { state, .. } = event else {
        unreachable!()
    };
    let reason = state.detail().expect("the reason is carried");
    assert!(
        reason.to_lowercase().contains("unauthorized") || reason.contains("401"),
        "the apiserver's own words should reach the user: {reason}"
    );

    // And nothing pretended the cluster was fine in the meantime.
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Connected,
                ..
            }
        )),
        "reported connected despite a rejected credential: {}",
        describe(&seen)
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
#[ignore = "needs a cluster"]
fn a_missing_kubeconfig_is_reported_rather_than_silently_empty() {
    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig("/nonexistent/periscope/kubeconfig"),
        RuntimeConfig::default(),
    )
    .expect("the cluster runtime starts");

    runtime
        .send(ClusterCommand::ListContexts)
        .expect("the command is queued");

    let (event, _) = wait_for(&stream, Duration::from_secs(10), |event| {
        matches!(event, ClusterEvent::ConfigFailed { .. })
    })
    .unwrap_or_else(|seen| panic!("no config failure reported; saw: {}", describe(&seen)));

    let ClusterEvent::ConfigFailed { reason } = event else {
        unreachable!()
    };
    assert!(
        reason.contains("/nonexistent/periscope/kubeconfig"),
        "the path that failed should be named: {reason}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn connecting_to_a_context_missing_from_a_kubeconfig_names_the_context() {
    let directory =
        std::env::temp_dir().join(format!("periscope-e2e-empty-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temp directory");
    let path = directory.join("empty.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({ "apiVersion": "v1", "kind": "Config" })).unwrap(),
    )
    .expect("the fixture kubeconfig is written");

    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig(&path),
        RuntimeConfig::default(),
    )
    .expect("the cluster runtime starts");

    runtime
        .send(ClusterCommand::Connect {
            cluster: ClusterId::new("ghost"),
        })
        .expect("connect is queued");

    let (event, _) = wait_for(&stream, Duration::from_secs(10), |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Disconnected { reason: Some(_) }
                    | ConnectionState::AuthFailed { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| panic!("no failure reported; saw: {}", describe(&seen)));

    let ClusterEvent::Status { state, .. } = event else {
        unreachable!()
    };
    assert!(
        state
            .detail()
            .is_some_and(|reason| reason.contains("ghost")),
        "the missing context should be named: {state:?}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}
