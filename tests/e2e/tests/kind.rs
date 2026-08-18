//! End-to-end tests against a real cluster.
//!
//! Run with:
//!
//! ```text
//! kind create cluster --name periscope
//! cargo test -p periscope-e2e -- --ignored --test-threads 1
//! ```

use std::time::{Duration, Instant};

use periscope_bridge::{ClusterCommand, ClusterEvent, ConnectionState};
use periscope_e2e::{connected, describe, runtime, wait_for};
use periscope_store::AppState;

/// Generous: a cold connection may run an exec plugin and list every pod.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[test]
#[ignore = "needs a cluster"]
fn connecting_lists_the_pods_that_are_running() {
    let (_runtime, stream, cluster) = connected();

    let (event, seen) = wait_for(&stream, CONNECT_TIMEOUT, |event| {
        matches!(event, ClusterEvent::PodsReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing arrived; saw: {}", describe(&seen)));

    let ClusterEvent::PodsReset { pods, .. } = &event else {
        unreachable!()
    };
    let listed = pods.len();
    assert!(listed > 0, "a running cluster always has kube-system pods");
    assert!(
        pods.iter().any(|pod| &*pod.key.namespace == "kube-system"),
        "expected control-plane pods, got {:?}",
        pods.iter()
            .map(|pod| pod.key.to_string())
            .collect::<Vec<_>>()
    );

    // The connection state must have been reported before the data.
    assert!(
        seen.iter().any(|event| matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Connecting,
                ..
            }
        )),
        "expected a connecting status first, saw: {}",
        describe(&seen)
    );

    // And the store must turn that stream into rows.
    let mut state = AppState::new();
    state.select(cluster);
    state.apply_batch(std::slice::from_ref(&event), Instant::now());
    assert_eq!(state.rows().len(), listed);
    assert!(state.active_counts().1 > 0, "some pods should be ready");
}

/// The budget from `IMPLEMENTATION.md` §3 Phase 1: a change in the cluster must
/// reach the table within a second. Asserted at twice that, so a loaded CI box
/// does not fail the build, with the measured figure printed either way.
const CHANGE_BUDGET: Duration = Duration::from_secs(1);

#[test]
#[ignore = "needs a cluster"]
fn a_pod_created_and_deleted_in_the_cluster_reaches_the_stream() {
    let (_runtime, stream, _cluster) = connected();
    wait_for(&stream, CONNECT_TIMEOUT, |event| {
        matches!(event, ClusterEvent::PodsReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing arrived; saw: {}", describe(&seen)));

    let name = format!("periscope-probe-{}", std::process::id());

    // Timed from before the write, so this measures what a user sees: the gap
    // between something happening in the cluster and it appearing in the app.
    let created = Instant::now();
    periscope_e2e::create_probe_pod(&name).expect("probe pod is created");
    let (_, _) = wait_for(
        &stream,
        CHANGE_BUDGET * 2,
        |event| matches!(event, ClusterEvent::PodApplied { pod, .. } if *pod.key.name == name),
    )
    .unwrap_or_else(|seen| {
        let _ = periscope_e2e::delete_probe_pod(&name);
        panic!("the new pod never arrived; saw: {}", describe(&seen))
    });
    let create_latency = created.elapsed();

    let deleted = Instant::now();
    periscope_e2e::delete_probe_pod(&name).expect("probe pod is deleted");
    wait_for(
        &stream,
        CHANGE_BUDGET * 2,
        |event| matches!(event, ClusterEvent::PodDeleted { key, .. } if *key.name == name),
    )
    .unwrap_or_else(|seen| panic!("the deletion never arrived; saw: {}", describe(&seen)));
    let delete_latency = deleted.elapsed();

    println!(
        "create -> event {:?}, delete -> event {:?} (budget {CHANGE_BUDGET:?})",
        create_latency, delete_latency
    );
}

/// The Phase 1 budget: a 10,000-pod cluster lists in under three seconds.
const LIST_BUDGET: Duration = Duration::from_secs(3);

/// How many pods make this a load test rather than a smoke test.
const LOAD_SIZE: usize = 10_000;

#[test]
#[ignore = "needs a cluster seeded with `seed-pods`"]
fn a_ten_thousand_pod_cluster_lists_inside_the_budget() {
    let (_runtime, stream, cluster) = connected();

    let started = Instant::now();
    let (event, _) = wait_for(&stream, Duration::from_secs(60), |event| {
        matches!(event, ClusterEvent::PodsReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing arrived; saw: {}", describe(&seen)));
    let listed = started.elapsed();

    let ClusterEvent::PodsReset { pods, .. } = &event else {
        unreachable!()
    };
    assert!(
        pods.len() >= LOAD_SIZE,
        "expected at least {LOAD_SIZE} pods, found {}. Seed them with: \
         cargo run --release -p periscope-e2e --bin seed-pods -- --count {LOAD_SIZE}",
        pods.len()
    );

    // Everything between the wire and the rows the UI indexes into.
    let mut state = AppState::new();
    state.select(cluster);
    let applied = Instant::now();
    state.apply_batch(std::slice::from_ref(&event), Instant::now());
    let applied = applied.elapsed();

    assert_eq!(state.rows().len(), pods.len());
    println!(
        "{} pods: list+project {listed:?}, store+sort {applied:?} (budget {LIST_BUDGET:?})",
        pods.len()
    );
    assert!(
        listed + applied < LIST_BUDGET,
        "listing {} pods took {:?}, over the {LIST_BUDGET:?} budget",
        pods.len(),
        listed + applied
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_context_that_does_not_exist_fails_with_the_real_error() {
    let (runtime, stream) = runtime();
    runtime
        .send(ClusterCommand::Connect {
            cluster: "periscope-no-such-context".into(),
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
    let detail = state.detail().expect("a reason is always carried");
    assert!(
        detail.contains("periscope-no-such-context"),
        "the error should name the context: {detail}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn disconnecting_stops_the_stream() {
    let (runtime, stream, cluster) = connected();
    wait_for(&stream, CONNECT_TIMEOUT, |event| {
        matches!(event, ClusterEvent::PodsReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing arrived; saw: {}", describe(&seen)));

    runtime
        .send(ClusterCommand::Disconnect {
            cluster: cluster.clone(),
        })
        .expect("disconnect is queued");

    wait_for(&stream, Duration::from_secs(10), |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Disconnected { reason: None },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| panic!("no disconnect reported; saw: {}", describe(&seen)));

    // Nothing more should arrive for that cluster.
    std::thread::sleep(Duration::from_secs(2));
    while let Some(event) = stream.try_recv() {
        assert!(
            !matches!(
                event,
                ClusterEvent::PodApplied { .. } | ClusterEvent::PodDeleted { .. }
            ),
            "the watch kept running after disconnect: {event:?}"
        );
    }
}
