//! Several clusters at once.
//!
//! Five contexts are pointed at the same `kind` apiserver, plus one at a port
//! nothing is listening on. That is not five *clusters* — one apiserver is
//! answering all of them — but it is five independent clients, five sets of
//! watches and five sets of tables in this process, which is what the Phase 4
//! budgets are about. `docs/LIMITATIONS.md` says so plainly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, ConnectionState, EventStream, KindId,
    RuntimeConfig,
};
use periscope_cluster::KubeHandler;
use periscope_e2e::pods;
use periscope_store::AppState;

/// Five clients against one apiserver need room to all get through discovery.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The contexts the fixture kubeconfig defines.
const CLUSTERS: [&str; 5] = [
    "cluster-1",
    "cluster-2",
    "cluster-3",
    "cluster-4",
    "cluster-5",
];

/// Writes a kubeconfig with `CLUSTERS` all pointing at the test cluster, plus
/// one context pointing at a closed port.
fn multi_kubeconfig(directory: &std::path::Path) -> std::path::PathBuf {
    let cluster = periscope_e2e::exec::test_cluster();
    let context_name = periscope_e2e::context().to_string();

    let config = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "current-context": CLUSTERS[0],
        "clusters": CLUSTERS
            .iter()
            .map(|name| {
                let mut copy = cluster.clone();
                copy["name"] = serde_json::json!(name);
                copy
            })
            .chain(std::iter::once(serde_json::json!({
                "name": "unreachable",
                "cluster": {
                    "server": "https://127.0.0.1:1",
                    "insecure-skip-tls-verify": true
                }
            })))
            .collect::<Vec<_>>(),
        "contexts": CLUSTERS
            .iter()
            .map(|name| serde_json::json!({
                "name": name,
                "context": { "cluster": name, "user": "test-user" }
            }))
            .chain(std::iter::once(serde_json::json!({
                "name": "unreachable",
                "context": { "cluster": "unreachable", "user": "test-user" }
            })))
            .collect::<Vec<_>>(),
        "users": [{ "name": "test-user", "user": user_credentials(&context_name) }]
    });

    let path = directory.join("multi.kubeconfig");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap())
        .expect("the fixture kubeconfig is written");
    path
}

/// The credentials the real context authenticates with, copied verbatim.
fn user_credentials(_context: &str) -> serde_json::Value {
    let (certificate, key) = periscope_e2e::exec::test_client_certificate()
        .expect("the test context uses client certificates");
    serde_json::json!({
        "client-certificate-data": certificate,
        "client-key-data": key
    })
}

fn runtime_with(kubeconfig: &std::path::Path) -> (ClusterRuntime, EventStream) {
    ClusterRuntime::start(
        KubeHandler::with_kubeconfig(kubeconfig),
        RuntimeConfig::default(),
    )
    .expect("the cluster runtime starts")
}

/// Connects every cluster and watches pods on each.
fn connect_all(runtime: &ClusterRuntime, stream: &EventStream, kind: &KindId) {
    for name in CLUSTERS {
        runtime
            .send(ClusterCommand::Connect {
                cluster: ClusterId::new(name),
            })
            .expect("connect is queued");
    }

    let mut discovered = 0;
    let deadline = Instant::now() + TIMEOUT;
    while discovered < CLUSTERS.len() && Instant::now() < deadline {
        if let Some(ClusterEvent::Kinds { cluster, .. }) = stream.try_recv() {
            discovered += 1;
            runtime
                .send(ClusterCommand::Watch {
                    cluster,
                    kind: kind.clone(),
                    namespace: None,
                    selector: None,
                })
                .expect("watch is queued");
        }
    }
    assert_eq!(
        discovered,
        CLUSTERS.len(),
        "only {discovered} of {} clusters finished discovery",
        CLUSTERS.len()
    );
}

#[test]
#[ignore = "needs a cluster"]
fn five_clusters_stream_at_once_and_stay_inside_the_row_budget() {
    let scratch = periscope_e2e::exec::Scratch::new("multicluster");
    let kubeconfig = multi_kubeconfig(scratch.path());
    let (runtime, stream) = runtime_with(&kubeconfig);

    connect_all(&runtime, &stream, &pods());

    // Fold everything into a store, exactly as the UI does.
    let mut state = AppState::new();
    let mut listed: std::collections::BTreeSet<ClusterId> = Default::default();
    let deadline = Instant::now() + TIMEOUT;

    while listed.len() < CLUSTERS.len() && Instant::now() < deadline {
        match stream.try_recv() {
            Some(event) => {
                if let ClusterEvent::ResourceReset { cluster, rows, .. } = &event
                    && !rows.is_empty()
                {
                    listed.insert(cluster.clone());
                }
                state.apply_batch(std::slice::from_ref(&event), Instant::now());
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    assert_eq!(
        listed.len(),
        CLUSTERS.len(),
        "only {listed:?} produced a listing"
    );

    for name in CLUSTERS {
        let rows = state.cluster_rows(&ClusterId::new(name));
        assert!(rows > 0, "{name} holds no rows");
        assert!(
            rows <= state.budget(),
            "{name} holds {rows} rows, over the {} budget",
            state.budget()
        );
    }

    println!("five clusters: {} rows held in total", state.total_rows());

    // `PERISCOPE_E2E_SOAK=60` keeps the five sessions streaming for a minute so
    // resident memory can be sampled from outside: a budget about memory needs
    // a window in which to measure it.
    if let Ok(seconds) = std::env::var("PERISCOPE_E2E_SOAK")
        && let Ok(seconds) = seconds.parse::<u64>()
    {
        let until = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < until {
            match stream.try_recv() {
                Some(event) => {
                    state.apply_batch(std::slice::from_ref(&event), Instant::now());
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        println!(
            "after {seconds}s: {} rows held across {} clusters",
            state.total_rows(),
            CLUSTERS.len()
        );
    }
}

#[test]
#[ignore = "needs a cluster"]
fn one_unreachable_cluster_does_not_disturb_the_others() {
    let scratch = periscope_e2e::exec::Scratch::new("multicluster-dead");
    let kubeconfig = multi_kubeconfig(scratch.path());
    let (runtime, stream) = runtime_with(&kubeconfig);

    // The dead one first, so it is failing while the others connect.
    runtime
        .send(ClusterCommand::Connect {
            cluster: ClusterId::new("unreachable"),
        })
        .expect("connect is queued");
    for name in [CLUSTERS[0], CLUSTERS[1]] {
        runtime
            .send(ClusterCommand::Connect {
                cluster: ClusterId::new(name),
            })
            .expect("connect is queued");
    }

    let mut state = AppState::new();
    let deadline = Instant::now() + TIMEOUT;
    let mut healthy = 0;
    let mut failed = false;

    while (healthy < 2 || !failed) && Instant::now() < deadline {
        match stream.try_recv() {
            Some(event) => {
                if let ClusterEvent::Status { cluster, state } = &event {
                    match state {
                        ConnectionState::Connected if cluster.as_str() != "unreachable" => {
                            healthy += 1
                        }
                        ConnectionState::Disconnected { reason: Some(_) }
                        | ConnectionState::AuthFailed { .. }
                            if cluster.as_str() == "unreachable" =>
                        {
                            failed = true
                        }
                        _ => {}
                    }
                }
                state.apply_batch(std::slice::from_ref(&event), Instant::now());
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    assert!(failed, "the unreachable cluster never reported a failure");
    assert_eq!(healthy, 2, "the reachable clusters did not both connect");

    // And the failure is attributed to the cluster that had it, with the real
    // reason, while the others are untouched.
    let dead = state
        .connection(&ClusterId::new("unreachable"))
        .expect("tracked");
    assert!(dead.state.is_problem());
    assert!(dead.state.detail().is_some_and(|reason| !reason.is_empty()));

    for name in [CLUSTERS[0], CLUSTERS[1]] {
        let connection = state.connection(&ClusterId::new(name)).expect("tracked");
        assert!(
            !connection.state.is_problem(),
            "{name} was disturbed: {:?}",
            connection.state
        );
    }
}

#[test]
#[ignore = "needs a cluster"]
fn switching_between_warm_clusters_needs_no_refetch() {
    let scratch = periscope_e2e::exec::Scratch::new("multicluster-warm");
    let kubeconfig = multi_kubeconfig(scratch.path());
    let (runtime, stream) = runtime_with(&kubeconfig);

    connect_all(&runtime, &stream, &pods());

    let mut state = AppState::new();
    state.apply_batch(
        &[ClusterEvent::Contexts {
            contexts: CLUSTERS
                .iter()
                .map(|name| periscope_bridge::ContextInfo {
                    name: Arc::from(*name),
                    cluster: Arc::from(*name),
                    user: None,
                    namespace: None,
                })
                .collect(),
            current: Some(ClusterId::new(CLUSTERS[0])),
        }],
        Instant::now(),
    );
    state.select_kind(pods());

    let deadline = Instant::now() + TIMEOUT;
    let mut listed = 0;
    while listed < CLUSTERS.len() && Instant::now() < deadline {
        if let Some(event) = stream.try_recv() {
            if matches!(&event, ClusterEvent::ResourceReset { rows, .. } if !rows.is_empty()) {
                listed += 1;
            }
            state.apply_batch(std::slice::from_ref(&event), Instant::now());
        }
    }

    // Every cluster is warm; switching is a selection, not a fetch.
    for name in CLUSTERS {
        let switched = Instant::now();
        state.select_cluster(ClusterId::new(name));
        let elapsed = switched.elapsed();

        assert!(!state.rows().is_empty(), "{name} showed nothing");
        assert!(
            elapsed < Duration::from_millis(50),
            "switching to {name} took {elapsed:?}"
        );
    }
}
