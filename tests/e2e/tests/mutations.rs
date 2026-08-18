//! Mutations against a real cluster.
//!
//! Everything here changes the test cluster, so every test creates what it
//! destroys. Nothing touches a workload it did not make.

use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, EventStream, KindId, Mutation,
    MutationOutcome, ResourceKey, RuntimeConfig,
};
use periscope_cluster::{KubeHandler, WritePolicy};
use periscope_config::{AuditLog, AuditOutcome};
use periscope_e2e::{context, describe, wait_for};

const TIMEOUT: Duration = Duration::from_secs(30);

fn deployments() -> KindId {
    KindId::new("apps", "v1", "Deployment", "deployments")
}

/// A connected runtime, with a policy and an audit log of the test's choosing.
fn connected_with(
    policy: WritePolicy,
    audit: Option<AuditLog>,
) -> (ClusterRuntime, EventStream, ClusterId) {
    let mut handler = KubeHandler::new().with_policy(policy);
    if let Some(audit) = audit {
        handler = handler.with_audit(audit);
    }

    let (runtime, stream) =
        ClusterRuntime::start(handler, RuntimeConfig::default()).expect("the runtime starts");
    let cluster = context();

    runtime
        .send(ClusterCommand::Connect {
            cluster: cluster.clone(),
        })
        .expect("connect is queued");
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    (runtime, stream, cluster)
}

/// Sends a mutation and waits for its outcome.
fn mutate(
    runtime: &ClusterRuntime,
    stream: &EventStream,
    cluster: &ClusterId,
    mutation: Mutation,
) -> MutationOutcome {
    runtime
        .send(ClusterCommand::Mutate {
            cluster: cluster.clone(),
            mutation: Arc::new(mutation),
        })
        .expect("the command is queued");

    let (event, seen) = wait_for(stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::MutationDone { .. })
    })
    .unwrap_or_else(|seen| panic!("no outcome came back; saw: {}", describe(&seen)));
    let _ = seen;

    let ClusterEvent::MutationDone { outcome, .. } = event else {
        unreachable!()
    };
    outcome
}

/// A scratch deployment that deletes itself.
struct Scratch {
    name: String,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let name = format!("periscope-{label}-{}", std::process::id());
        periscope_e2e::apply_yaml(&format!(
            "apiVersion: apps/v1\n\
             kind: Deployment\n\
             metadata:\n  \
               name: {name}\n  \
               namespace: default\n\
             spec:\n  \
               replicas: 1\n  \
               selector:\n    \
                 matchLabels:\n      \
                   app: {name}\n  \
               template:\n    \
                 metadata:\n      \
                   labels:\n        \
                     app: {name}\n    \
                 spec:\n      \
                   containers:\n      \
                   - name: pause\n        \
                     image: registry.k8s.io/pause:3.10\n"
        ))
        .expect("the fixture deployment is created");
        Self { name }
    }

    fn key(&self) -> ResourceKey {
        ResourceKey::new("default", &self.name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = periscope_e2e::delete_deployment("default", &self.name);
    }
}

#[test]
#[ignore = "needs a cluster"]
fn a_read_only_cluster_refuses_and_changes_nothing() {
    let scratch = Scratch::new("readonly");
    let audit = periscope_e2e::exec::Scratch::new("audit-readonly");
    let log = AuditLog::at(audit.path().join("audit.log"));

    let mut policy = WritePolicy::permissive();
    policy.deny(context());
    let (runtime, stream, cluster) = connected_with(policy, Some(log.clone()));

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Delete {
            kind: deployments(),
            key: scratch.key(),
            grace_period: None,
        },
    );

    match &outcome {
        MutationOutcome::Refused { reason } => assert!(reason.contains("read-only"), "{reason}"),
        other => panic!("a read-only cluster must refuse, got {other:?}"),
    }

    // And the object is still there: the refusal happened before the request.
    assert!(
        periscope_e2e::deployment_exists("default", &scratch.name),
        "the deployment was deleted despite the refusal"
    );

    // The refusal is in the audit log, which is the point of recording them.
    let entries = log.read().expect("the audit log is readable");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, AuditOutcome::Refused);
    assert_eq!(entries[0].action, "delete");
    assert_eq!(entries[0].name, scratch.name);
    assert!(!entries[0].outcome.changed_anything());
}

#[test]
#[ignore = "needs a cluster"]
fn scaling_a_deployment_changes_its_replicas_and_is_audited() {
    let scratch = Scratch::new("scale");
    let audit = periscope_e2e::exec::Scratch::new("audit-scale");
    let log = AuditLog::at(audit.path().join("audit.log"));
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), Some(log.clone()));

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Scale {
            kind: deployments(),
            key: scratch.key(),
            replicas: 2,
            current: Some(1),
        },
    );

    match &outcome {
        MutationOutcome::Applied { detail } => assert!(detail.contains("scaled to 2"), "{detail}"),
        other => panic!("expected the scale to apply, got {other:?}"),
    }

    let deadline = Instant::now() + TIMEOUT;
    let mut replicas = 0;
    while Instant::now() < deadline && replicas != 2 {
        replicas = periscope_e2e::deployment_replicas("default", &scratch.name).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(replicas, 2, "the cluster did not reach two replicas");

    let entries = log.read().expect("readable");
    assert_eq!(entries[0].outcome, AuditOutcome::Applied);
    assert_eq!(entries[0].action, "scale");
    assert_eq!(entries[0].detail, "replicas=2");
}

#[test]
#[ignore = "needs a cluster"]
fn a_dry_run_apply_changes_nothing_and_shows_what_would_happen() {
    let scratch = Scratch::new("dryrun");
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);

    let yaml = format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n  \
           name: {name}\n  \
           namespace: default\n  \
           labels:\n    \
             touched-by: periscope\n\
         spec:\n  \
           replicas: 3\n  \
           selector:\n    \
             matchLabels:\n      \
               app: {name}\n  \
           template:\n    \
             metadata:\n      \
               labels:\n        \
                 app: {name}\n    \
             spec:\n      \
               containers:\n      \
               - name: pause\n        \
                 image: registry.k8s.io/pause:3.10\n",
        name = scratch.name
    );

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Apply {
            kind: deployments(),
            key: scratch.key(),
            yaml: Arc::from(yaml.as_str()),
            dry_run: true,
        },
    );

    match &outcome {
        MutationOutcome::DryRun { preview } => {
            // The preview is the object as it *would* be.
            assert!(preview.contains("replicas: 3"), "{preview}");
            assert!(preview.contains("touched-by: periscope"), "{preview}");
        }
        other => panic!("expected a dry run, got {other:?}"),
    }

    // Nothing changed in the cluster.
    assert_eq!(
        periscope_e2e::deployment_replicas("default", &scratch.name),
        Some(1),
        "a dry run must not change the replica count"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn applying_an_edit_changes_the_object() {
    let scratch = Scratch::new("apply");
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);

    let yaml = format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n  \
           name: {name}\n  \
           namespace: default\n\
         spec:\n  \
           replicas: 2\n  \
           selector:\n    \
             matchLabels:\n      \
               app: {name}\n  \
           template:\n    \
             metadata:\n      \
               labels:\n        \
                 app: {name}\n    \
             spec:\n      \
               containers:\n      \
               - name: pause\n        \
                 image: registry.k8s.io/pause:3.10\n",
        name = scratch.name
    );

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Apply {
            kind: deployments(),
            key: scratch.key(),
            yaml: Arc::from(yaml.as_str()),
            dry_run: false,
        },
    );
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if periscope_e2e::deployment_replicas("default", &scratch.name) == Some(2) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the apply did not take effect");
}

#[test]
#[ignore = "needs a cluster"]
fn renaming_an_object_in_the_editor_is_refused() {
    let scratch = Scratch::new("rename");
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);

    let yaml = "apiVersion: apps/v1\n\
                kind: Deployment\n\
                metadata:\n  \
                  name: something-else\n  \
                  namespace: default\n\
                spec:\n  \
                  replicas: 1\n";

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Apply {
            kind: deployments(),
            key: scratch.key(),
            yaml: Arc::from(yaml),
            dry_run: false,
        },
    );

    match &outcome {
        MutationOutcome::Failed { reason } => {
            // Applying it would have created a second object rather than
            // changing this one.
            assert!(reason.contains("something-else"), "{reason}");
            assert!(reason.contains("Renaming"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    assert!(!periscope_e2e::deployment_exists(
        "default",
        "something-else"
    ));
}

#[test]
#[ignore = "needs a cluster"]
fn deleting_a_deployment_removes_it() {
    let scratch = Scratch::new("delete");
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Delete {
            kind: deployments(),
            key: scratch.key(),
            grace_period: Some(0),
        },
    );
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if !periscope_e2e::deployment_exists("default", &scratch.name) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the deployment is still there");
}

#[test]
#[ignore = "needs a cluster"]
fn cordoning_a_node_and_letting_it_back() {
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);
    let node = periscope_e2e::first_node().expect("the cluster has a node");

    let cordon = |cordon: bool| Mutation::Cordon {
        node: Arc::from(node.as_str()),
        cordon,
    };

    let outcome = mutate(&runtime, &stream, &cluster, cordon(true));
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(periscope_e2e::node_unschedulable(&node), Some(true));

    // Always put it back: a cordoned control plane would break every later
    // test on this cluster.
    let outcome = mutate(&runtime, &stream, &cluster, cordon(false));
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_ne!(periscope_e2e::node_unschedulable(&node), Some(true));
}

#[test]
#[ignore = "needs a cluster"]
fn a_failure_from_the_apiserver_is_reported_verbatim_and_audited() {
    let audit = periscope_e2e::exec::Scratch::new("audit-failure");
    let log = AuditLog::at(audit.path().join("audit.log"));
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), Some(log.clone()));

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Delete {
            kind: deployments(),
            key: ResourceKey::new("default", "periscope-does-not-exist"),
            grace_period: None,
        },
    );

    match &outcome {
        MutationOutcome::Failed { reason } => {
            assert!(
                reason.contains("not found") || reason.contains("NotFound"),
                "{reason}"
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    let entries = log.read().expect("readable");
    assert_eq!(entries[0].outcome, AuditOutcome::Failed);
    assert!(!entries[0].reason.is_empty());
}

#[test]
#[ignore = "needs a cluster"]
fn restarting_a_deployment_rolls_its_pods() {
    let scratch = Scratch::new("restart");
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), None);

    let outcome = mutate(
        &runtime,
        &stream,
        &cluster,
        Mutation::Restart {
            kind: deployments(),
            key: scratch.key(),
        },
    );
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );

    // The annotation `kubectl rollout restart` uses is what triggers the roll,
    // so its presence is what proves the operation is the same one.
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if periscope_e2e::deployment_has_restart_annotation("default", &scratch.name) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the restart annotation was never set");
}
