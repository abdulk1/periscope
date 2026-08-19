//! Performing mutations.
//!
//! This is the last gate before the apiserver, and it repeats the read-only
//! check the store already made. That is deliberate duplication: the store's
//! check is what the UI asks, and this one is what actually stands between a
//! protected cluster and a request. A bug in the view layer must not be enough
//! to change a cluster somebody marked read-only.
//!
//! Every attempt is written to the audit log — applied, refused or failed —
//! before the outcome goes back to the UI.

use std::collections::BTreeSet;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, EvictParams, ListParams, Patch, PatchParams};
use kube::{Client, api::DynamicObject};
use periscope_bridge::{ClusterId, KindId, Mutation, MutationOutcome, ResourceKey};
use periscope_config::{AuditEntry, AuditLog, AuditOutcome};
use serde_json::json;

use crate::errors::describe;
use crate::watch::api_for;

/// The field manager Periscope applies as.
///
/// Server-side apply records this on every field it owns, so `kubectl` users
/// can see which changes came from here.
const FIELD_MANAGER: &str = "periscope";

/// Which clusters this process refuses to change.
///
/// Held separately from the store's copy on purpose: the two are set from the
/// same settings but checked independently.
#[derive(Clone, Debug, Default)]
pub struct WritePolicy {
    read_only: BTreeSet<ClusterId>,
    read_only_by_default: bool,
    writable: BTreeSet<ClusterId>,
}

impl WritePolicy {
    /// A policy allowing every cluster.
    pub fn permissive() -> Self {
        Self::default()
    }

    /// A policy built from settings.
    pub fn from_access(access: &periscope_config::Access) -> Self {
        Self {
            read_only: access.read_only.iter().map(ClusterId::new).collect(),
            read_only_by_default: access.read_only_by_default,
            writable: access.writable.iter().map(ClusterId::new).collect(),
        }
    }

    /// Marks a cluster read-only.
    pub fn deny(&mut self, cluster: ClusterId) {
        self.writable.remove(&cluster);
        self.read_only.insert(cluster);
    }

    /// Whether a cluster may be changed.
    pub fn may_mutate(&self, cluster: &ClusterId) -> bool {
        if self.read_only.contains(cluster) {
            return false;
        }
        if self.read_only_by_default {
            return self.writable.contains(cluster);
        }
        true
    }
}

/// Runs one mutation and reports what happened.
///
/// `namespaced` comes from discovery, as it does for watches: without it the
/// URL would be wrong for cluster-scoped kinds.
pub async fn run(
    cluster: ClusterId,
    client: Client,
    mutation: Arc<Mutation>,
    namespaced: bool,
    policy: &WritePolicy,
    audit: Option<&AuditLog>,
) -> MutationOutcome {
    let outcome = if policy.may_mutate(&cluster) {
        perform(&client, &mutation, namespaced).await
    } else {
        // The store should already have refused this. Reaching here means a
        // caller skipped that check, which is exactly what this gate is for.
        tracing::warn!(%cluster, verb = mutation.verb(), "refused: cluster is read-only");
        MutationOutcome::Refused {
            reason: format!("{cluster} is read-only"),
        }
    };

    record(&cluster, &mutation, &outcome, audit);
    outcome
}

/// Writes one line of the audit log, whatever happened.
fn record(
    cluster: &ClusterId,
    mutation: &Mutation,
    outcome: &MutationOutcome,
    audit: Option<&AuditLog>,
) {
    let Some(audit) = audit else {
        return;
    };
    let key = mutation.key();

    let (result, reason) = match outcome {
        MutationOutcome::Applied { detail } => (AuditOutcome::Applied, detail.clone()),
        MutationOutcome::DryRun { .. } => (AuditOutcome::DryRun, String::new()),
        MutationOutcome::Refused { reason } => (AuditOutcome::Refused, reason.clone()),
        MutationOutcome::Failed { reason } => (AuditOutcome::Failed, reason.clone()),
    };

    // Redacted here rather than at the source: the outcome on screen keeps the
    // apiserver's exact words, and only the copy that outlives the session is
    // stripped of hostnames and anything token-shaped.
    let entry = AuditEntry::new(
        cluster.as_str(),
        &*key.namespace,
        mutation.kind().label(),
        &*key.name,
        mutation.verb(),
    )
    .detail(mutation.detail())
    .outcome(result, crate::redact::text(&reason));

    if let Err(error) = audit.append(&entry) {
        // Never fatal — but never silent either.
        tracing::error!(%cluster, %error, "could not write the audit log");
    }
}

/// Does the work, once it is allowed.
async fn perform(client: &Client, mutation: &Mutation, namespaced: bool) -> MutationOutcome {
    let key = mutation.key();
    let api = api_for(
        client.clone(),
        &mutation.kind(),
        namespaced,
        key.is_namespaced().then(|| &*key.namespace),
    );

    match mutation {
        Mutation::Delete { grace_period, .. } => delete(&api, &key, *grace_period).await,
        Mutation::Scale { replicas, .. } => scale(&api, &key, *replicas).await,
        Mutation::Restart { .. } => restart(&api, &key).await,
        Mutation::Cordon { cordon, .. } => cordon_node(&api, &key, *cordon).await,
        Mutation::Drain { node, grace_period } => drain(client, &api, node, *grace_period).await,
        Mutation::Apply { yaml, dry_run, .. } => apply(&api, &key, yaml, *dry_run).await,
    }
}

async fn delete(
    api: &kube::Api<DynamicObject>,
    key: &ResourceKey,
    grace_period: Option<u32>,
) -> MutationOutcome {
    let mut params = DeleteParams::default();
    if let Some(seconds) = grace_period {
        params = params.grace_period(seconds);
    }

    match api.delete(&key.name, &params).await {
        // The apiserver answers with the object while it is terminating, and
        // with a `Status` once it is gone. Neither is a failure, and the
        // difference is worth saying. `either::Either` is not re-exported by
        // `kube`, so this asks the value rather than naming its type.
        Ok(result) => MutationOutcome::Applied {
            detail: if result.is_left() {
                format!("{} is terminating", key.name)
            } else {
                format!("{} deleted", key.name)
            },
        },
        Err(error) => MutationOutcome::Failed {
            reason: describe(&error),
        },
    }
}

async fn scale(
    api: &kube::Api<DynamicObject>,
    key: &ResourceKey,
    replicas: u32,
) -> MutationOutcome {
    let patch = json!({ "spec": { "replicas": replicas } });

    match api
        .patch_scale(&key.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => MutationOutcome::Applied {
            detail: format!("{} scaled to {replicas}", key.name),
        },
        Err(error) => MutationOutcome::Failed {
            reason: describe(&error),
        },
    }
}

/// Rolls a workload's pods.
///
/// There is no rollout API: `kubectl rollout restart` stamps an annotation on
/// the pod template, and the controller does the rest. Doing the same thing
/// means a Periscope restart and a `kubectl` restart are the same operation.
async fn restart(api: &kube::Api<DynamicObject>, key: &ResourceKey) -> MutationOutcome {
    let now = periscope_config::audit::rfc3339_now();
    let patch = json!({
        "spec": {
            "template": {
                "metadata": {
                    "annotations": { "kubectl.kubernetes.io/restartedAt": now }
                }
            }
        }
    });

    match api
        .patch(&key.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => MutationOutcome::Applied {
            detail: format!("{} restarting", key.name),
        },
        Err(error) => MutationOutcome::Failed {
            reason: describe(&error),
        },
    }
}

async fn cordon_node(
    api: &kube::Api<DynamicObject>,
    key: &ResourceKey,
    cordon: bool,
) -> MutationOutcome {
    let patch = json!({ "spec": { "unschedulable": cordon } });

    match api
        .patch(&key.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => MutationOutcome::Applied {
            detail: format!(
                "{} {}",
                key.name,
                if cordon { "cordoned" } else { "uncordoned" }
            ),
        },
        Err(error) => MutationOutcome::Failed {
            reason: describe(&error),
        },
    }
}

/// Cordons a node and evicts the pods already on it.
///
/// The same two steps `kubectl drain` takes, and the same exclusions: pods
/// owned by a DaemonSet come straight back, so evicting them is churn, and
/// mirror pods are the kubelet's own and cannot be evicted at all.
///
/// It does **not** wait for the pods to finish terminating. Eviction is a
/// request the apiserver either accepts or refuses — a PodDisruptionBudget can
/// refuse it, and that refusal is what the user needs to see — while waiting
/// for termination can take as long as the longest `terminationGracePeriod` on
/// the node. What is reported is what the apiserver said, immediately.
async fn drain(
    client: &Client,
    nodes: &kube::Api<DynamicObject>,
    node: &str,
    grace_period: Option<u32>,
) -> MutationOutcome {
    // Cordon first: draining a node that still accepts pods is a treadmill.
    let cordoned = cordon_node(nodes, &ResourceKey::cluster_scoped(node), true).await;
    if let MutationOutcome::Failed { reason } = cordoned {
        return MutationOutcome::Failed {
            reason: format!("could not cordon {node}: {reason}"),
        };
    }

    let pods: kube::Api<Pod> = kube::Api::all(client.clone());
    let on_node = ListParams::default().fields(&format!("spec.nodeName={node}"));
    let list = match pods.list(&on_node).await {
        Ok(list) => list,
        Err(error) => {
            return MutationOutcome::Failed {
                reason: format!(
                    "cordoned {node}, but could not list its pods: {}",
                    describe(&error)
                ),
            };
        }
    };

    let mut evicted = 0;
    let mut skipped = 0;
    let mut refused: Vec<String> = Vec::new();

    for pod in list.items {
        let (Some(name), Some(namespace)) =
            (pod.metadata.name.clone(), pod.metadata.namespace.clone())
        else {
            continue;
        };

        if is_daemonset_pod(&pod) || is_mirror_pod(&pod) {
            skipped += 1;
            continue;
        }

        let api: kube::Api<Pod> = kube::Api::namespaced(client.clone(), &namespace);
        let mut delete = DeleteParams::default();
        if let Some(seconds) = grace_period {
            delete = delete.grace_period(seconds);
        }

        match api
            .evict(
                &name,
                &EvictParams {
                    delete_options: Some(delete),
                    ..EvictParams::default()
                },
            )
            .await
        {
            Ok(_) => evicted += 1,
            Err(error) => {
                // A PodDisruptionBudget refusing an eviction is the single most
                // useful thing a drain can tell you, so it is reported per pod
                // rather than as one failure.
                refused.push(format!("{namespace}/{name}: {}", describe(&error)));
            }
        }
    }

    let summary = format!(
        "{node} cordoned, {evicted} pod(s) evicted, {skipped} skipped (DaemonSet or mirror pods)"
    );

    if refused.is_empty() {
        MutationOutcome::Applied { detail: summary }
    } else {
        MutationOutcome::Failed {
            reason: format!(
                "{summary}; {} refused — {}",
                refused.len(),
                refused.join("; ")
            ),
        }
    }
}

/// Whether a pod is managed by a DaemonSet, which would recreate it at once.
fn is_daemonset_pod(pod: &Pod) -> bool {
    pod.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|owner| owner.kind == "DaemonSet")
}

/// Whether a pod is a mirror of a static kubelet pod, which cannot be evicted.
fn is_mirror_pod(pod: &Pod) -> bool {
    pod.metadata
        .annotations
        .as_ref()
        .is_some_and(|annotations| annotations.contains_key("kubernetes.io/config.mirror"))
}

/// Applies edited YAML, optionally as a dry run.
///
/// Server-side apply, so the apiserver merges and validates rather than this
/// code doing either, and a dry run is the same request with one flag flipped —
/// the preview is therefore exactly what the real apply would produce.
async fn apply(
    api: &kube::Api<DynamicObject>,
    key: &ResourceKey,
    yaml: &str,
    dry_run: bool,
) -> MutationOutcome {
    let object: serde_json::Value = match crate::yaml::parse(yaml) {
        Ok(object) => object,
        Err(reason) => {
            return MutationOutcome::Failed {
                reason: format!("that is not valid YAML: {reason}"),
            };
        }
    };

    if let Some(name) = object["metadata"]["name"].as_str()
        && name != &*key.name
    {
        // Renaming an object in the editor would create a second one rather
        // than change this one, which is never what an edit means.
        return MutationOutcome::Failed {
            reason: format!(
                "the edited object is named `{name}`, but you are editing `{}`. Renaming an \
                 object creates a new one; delete and recreate it instead.",
                key.name
            ),
        };
    }

    let mut params = PatchParams::apply(FIELD_MANAGER).force();
    if dry_run {
        params = params.dry_run();
    }

    match api.patch(&key.name, &params, &Patch::Apply(&object)).await {
        Ok(result) => {
            if dry_run {
                let preview = crate::detail::to_yaml(&result, false)
                    .unwrap_or_else(|error| format!("could not render the result: {error}"));
                MutationOutcome::DryRun {
                    preview: Arc::from(preview.as_str()),
                }
            } else {
                MutationOutcome::Applied {
                    detail: format!("{} applied", key.name),
                }
            }
        }
        Err(error) => MutationOutcome::Failed {
            reason: describe(&error),
        },
    }
}

/// Whether a kind can be scaled, for the UI to offer the action.
pub fn is_scalable(kind: &KindId) -> bool {
    matches!(
        (&*kind.group, &*kind.kind),
        ("apps", "Deployment")
            | ("apps", "StatefulSet")
            | ("apps", "ReplicaSet")
            | ("", "ReplicationController")
    )
}

/// Whether a kind can be rolled.
pub fn is_restartable(kind: &KindId) -> bool {
    matches!(
        (&*kind.group, &*kind.kind),
        ("apps", "Deployment") | ("apps", "StatefulSet") | ("apps", "DaemonSet")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prod() -> ClusterId {
        ClusterId::new("prod")
    }

    #[test]
    fn the_default_policy_allows_changes() {
        assert!(WritePolicy::permissive().may_mutate(&prod()));
    }

    #[test]
    fn a_denied_cluster_is_refused_here_too() {
        // The store refuses first; this is the gate that holds when something
        // above it does not.
        let mut policy = WritePolicy::permissive();
        policy.deny(prod());

        assert!(!policy.may_mutate(&prod()));
        assert!(policy.may_mutate(&ClusterId::new("staging")));
    }

    #[test]
    fn the_policy_matches_the_settings_it_came_from() {
        let mut access = periscope_config::Access::default();
        access.deny("prod");
        let policy = WritePolicy::from_access(&access);

        assert!(!policy.may_mutate(&prod()));
        assert!(policy.may_mutate(&ClusterId::new("kind-local")));
    }

    #[test]
    fn read_only_by_default_needs_an_explicit_allow() {
        let access = periscope_config::Access {
            read_only_by_default: true,
            writable: ["scratch".to_owned()].into_iter().collect(),
            ..periscope_config::Access::default()
        };
        let policy = WritePolicy::from_access(&access);

        assert!(policy.may_mutate(&ClusterId::new("scratch")));
        assert!(!policy.may_mutate(&prod()));
    }

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).expect("fixture is a valid pod")
    }

    #[test]
    fn daemonset_pods_are_skipped_by_a_drain() {
        // Evicting one is churn: the DaemonSet controller puts it straight back.
        let managed = pod(serde_json::json!({
            "metadata": {
                "name": "kube-proxy-abc",
                "namespace": "kube-system",
                "ownerReferences": [
                    { "apiVersion": "apps/v1", "kind": "DaemonSet", "name": "kube-proxy", "uid": "1" }
                ]
            }
        }));
        assert!(is_daemonset_pod(&managed));

        let ordinary = pod(serde_json::json!({
            "metadata": {
                "name": "api-0",
                "namespace": "default",
                "ownerReferences": [
                    { "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api", "uid": "2" }
                ]
            }
        }));
        assert!(!is_daemonset_pod(&ordinary));
    }

    #[test]
    fn mirror_pods_are_skipped_because_they_cannot_be_evicted() {
        let mirror = pod(serde_json::json!({
            "metadata": {
                "name": "kube-apiserver-cp",
                "namespace": "kube-system",
                "annotations": { "kubernetes.io/config.mirror": "abc123" }
            }
        }));
        assert!(is_mirror_pod(&mirror));

        let ordinary = pod(serde_json::json!({
            "metadata": { "name": "api-0", "namespace": "default" }
        }));
        assert!(!is_mirror_pod(&ordinary));
    }

    #[test]
    fn only_workloads_offer_scale_and_restart() {
        let deployments = KindId::new("apps", "v1", "Deployment", "deployments");
        let daemonsets = KindId::new("apps", "v1", "DaemonSet", "daemonsets");
        let pods = KindId::new("", "v1", "Pod", "pods");

        assert!(is_scalable(&deployments));
        assert!(is_restartable(&deployments));
        // A DaemonSet has no replica count, but it can be rolled.
        assert!(!is_scalable(&daemonsets));
        assert!(is_restartable(&daemonsets));
        assert!(!is_scalable(&pods));
        assert!(!is_restartable(&pods));
    }

    // --- the gate and the audit log ------------------------------------------
    //
    // Everything below runs without a cluster, because a refused mutation never
    // reaches the apiserver — which is the point of the gate and also what lets
    // it be tested here rather than only in `tests/e2e`. Before these existed,
    // `cargo test --workspace` passed with `run`'s policy check and its call to
    // `record` both deleted.

    /// A path that removes itself, file or directory.
    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "periscope-mutate-{}-{:?}-{name}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A client that is never used.
    ///
    /// `run` takes one, and a refused mutation never builds a request with it,
    /// so this points at a port nothing is listening on: if the gate ever stops
    /// refusing, the test fails rather than quietly talking to something.
    fn unused_client() -> Client {
        let config = kube::Config::new("https://127.0.0.1:1/".parse().expect("a URL"));
        Client::try_from(config).expect("a client can be built without connecting")
    }

    fn delete_api() -> Arc<Mutation> {
        Arc::new(Mutation::Delete {
            kind: KindId::new("apps", "v1", "Deployment", "deployments"),
            key: ResourceKey::new("payments", "api"),
            grace_period: Some(30),
        })
    }

    #[tokio::test]
    async fn the_last_gate_refuses_a_read_only_cluster_before_it_builds_a_request() {
        // ADR-0028: the store refuses first, and this refuses again. A bug in
        // the view must not be enough to change a protected cluster, so the
        // check is asserted here and not only where the UI asks.
        let mut policy = WritePolicy::permissive();
        policy.deny(prod());

        let outcome = run(prod(), unused_client(), delete_api(), true, &policy, None).await;

        match outcome {
            MutationOutcome::Refused { reason } => {
                assert!(
                    reason.contains("prod"),
                    "a refusal names the cluster: {reason}"
                );
                assert!(reason.contains("read-only"), "{reason}");
            }
            other => panic!("a read-only cluster must refuse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_audit_log_with_what_was_asked_for() {
        // "Every attempt is written to the audit log — applied, dry-run,
        // refused or failed." A refusal is the one worth being able to prove
        // afterwards, and the only one no request leaves a trace of anywhere
        // else.
        let file = TempPath::new("refusal");
        let audit = AuditLog::at(&file.0);
        let mut policy = WritePolicy::permissive();
        policy.deny(prod());

        run(
            prod(),
            unused_client(),
            delete_api(),
            true,
            &policy,
            Some(&audit),
        )
        .await;

        let entries = audit.read().expect("the log reads back");
        assert_eq!(entries.len(), 1, "{entries:?}");
        let entry = &entries[0];
        assert_eq!(entry.outcome, AuditOutcome::Refused);
        assert_eq!(entry.cluster, "prod");
        assert_eq!(entry.namespace, "payments");
        assert_eq!(entry.kind, "deployments.apps");
        assert_eq!(entry.name, "api");
        assert_eq!(entry.action, "delete");
        assert_eq!(entry.detail, "grace=30s");
        assert!(entry.reason.contains("read-only"), "{}", entry.reason);
    }

    #[test]
    fn every_outcome_a_mutation_can_have_is_recorded() {
        // The four are recorded by one `match`, and a fifth added to
        // `MutationOutcome` without a line here would be an attempt nobody
        // could account for afterwards.
        let file = TempPath::new("outcomes");
        let audit = AuditLog::at(&file.0);
        let mutation = delete_api();

        for outcome in [
            MutationOutcome::Applied {
                detail: "api deleted".to_owned(),
            },
            MutationOutcome::DryRun {
                preview: Arc::from("kind: Deployment"),
            },
            MutationOutcome::Refused {
                reason: "prod is read-only".to_owned(),
            },
            MutationOutcome::Failed {
                reason: "404 Not Found".to_owned(),
            },
        ] {
            record(&prod(), &mutation, &outcome, Some(&audit));
        }

        let recorded: Vec<AuditOutcome> = audit
            .read()
            .expect("the log reads back")
            .into_iter()
            .map(|entry| entry.outcome)
            .collect();
        assert_eq!(
            recorded,
            [
                AuditOutcome::Applied,
                AuditOutcome::DryRun,
                AuditOutcome::Refused,
                AuditOutcome::Failed,
            ]
        );
    }

    #[tokio::test]
    async fn an_audit_log_that_cannot_be_written_does_not_swallow_the_mutation() {
        // Failing to write the log is never fatal. The alternative — refusing
        // to act because the record could not be kept — would make a full disk
        // look like a permissions problem on the cluster.
        let file = TempPath::new("unwritable");
        std::fs::write(&file.0, "not a directory").expect("fixture written");
        let audit = AuditLog::at(file.0.join("audit.log"));
        assert!(
            audit
                .append(&AuditEntry::new("prod", "", "pods", "api-0", "delete"))
                .is_err(),
            "the fixture has to be a log that really cannot be written"
        );

        let mut policy = WritePolicy::permissive();
        policy.deny(prod());

        let outcome = run(
            prod(),
            unused_client(),
            delete_api(),
            true,
            &policy,
            Some(&audit),
        )
        .await;

        assert!(
            matches!(outcome, MutationOutcome::Refused { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_cordon_is_audited_against_the_node_it_names() {
        // A cordon carries a node rather than a key, so the audit line is built
        // from `Mutation::key`; taking the table's selection instead would file
        // it under whatever kind happened to be on screen.
        let file = TempPath::new("cordon");
        let audit = AuditLog::at(&file.0);
        let mutation = Arc::new(Mutation::Cordon {
            node: Arc::from("worker-0"),
            cordon: true,
        });

        record(
            &prod(),
            &mutation,
            &MutationOutcome::Applied {
                detail: "worker-0 cordoned".to_owned(),
            },
            Some(&audit),
        );

        let entries = audit.read().expect("the log reads back");
        assert_eq!(entries[0].kind, "nodes");
        assert_eq!(entries[0].name, "worker-0");
        assert_eq!(entries[0].namespace, "");
        assert_eq!(entries[0].action, "cordon");
    }
}
