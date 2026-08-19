//! The pod projector.
//!
//! The STATUS column is not `status.phase`. `kubectl get pods` computes it from
//! init-container state, waiting reasons, termination reasons and the deletion
//! timestamp, which is why a crash-looping pod reads `CrashLoopBackOff` rather
//! than `Running`. Anyone coming from `kubectl` or k9s expects those exact
//! strings, so this module reimplements that logic rather than inventing its
//! own vocabulary.
//!
//! Reference: `printPod` in `kubectl/pkg/printers/internalversion/printers.go`.
//!
//! It reads JSON rather than typed `k8s-openapi` structs because every kind
//! arrives as a `DynamicObject` — one watch path for pods and for a CRD nobody
//! has seen before.

use std::sync::Arc;

use kube::api::DynamicObject;
use serde_json::Value;

/// The reason the apiserver reports for a pod on a node that stopped answering.
const NODE_UNREACHABLE: &str = "NodeLost";

/// Waiting reason that means "nothing is wrong, init is simply not done".
const POD_INITIALIZING: &str = "PodInitializing";

/// READY, RESTARTS and the node a pod is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PodCounts {
    /// Containers currently ready.
    pub ready: u32,
    /// Containers in the pod, including sidecars.
    pub total: u32,
    /// Total restarts.
    pub restarts: u32,
}

fn array<'a>(value: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut current = value;
    for step in path {
        current = &current[step];
    }
    current.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for step in path {
        current = &current[step];
    }
    current.as_str().unwrap_or_default()
}

/// Whether an init container is a sidecar — one that keeps running alongside
/// the main containers, and so counts towards the READY total.
fn is_restartable(container: Option<&Value>) -> bool {
    container.is_some_and(|container| text(container, &["restartPolicy"]) == "Always")
}

fn init_container(data: &Value, name: &str) -> Option<Value> {
    array(data, &["spec", "initContainers"])
        .iter()
        .find(|container| text(container, &["name"]) == name)
        .cloned()
}

fn condition_is_true(data: &Value, kind: &str) -> bool {
    array(data, &["status", "conditions"])
        .iter()
        .any(|condition| {
            text(condition, &["type"]) == kind && text(condition, &["status"]) == "True"
        })
}

/// The READY and RESTARTS columns.
pub fn counts(data: &Value) -> PodCounts {
    let init = array(data, &["spec", "initContainers"]);
    let total = array(data, &["spec", "containers"]).len() as u32
        + init.iter().filter(|c| is_restartable(Some(c))).count() as u32;

    // While a pod is initializing, kubectl reports the init containers'
    // restarts and no ready containers at all.
    if initializing_reason(data).is_some() && !condition_is_true(data, "Initialized") {
        return PodCounts {
            ready: 0,
            total,
            restarts: array(data, &["status", "initContainerStatuses"])
                .iter()
                .map(|status| status["restartCount"].as_u64().unwrap_or(0) as u32)
                .sum(),
        };
    }

    let mut ready = 0;
    let mut restarts = 0;

    // Sidecars are counted in `total` above, and their statuses live in
    // `initContainerStatuses` rather than beside the ordinary containers — so
    // reading only the latter meant a healthy sidecar never made the pod look
    // ready, and one that was crash-looping every ten seconds showed no
    // restarts at all. That is the whole reason the RESTARTS column exists.
    let counted = array(data, &["status", "initContainerStatuses"])
        .iter()
        .filter(|status| is_restartable(init_container(data, text(status, &["name"])).as_ref()))
        .chain(array(data, &["status", "containerStatuses"]).iter());

    for status in counted {
        restarts += status["restartCount"].as_u64().unwrap_or(0) as u32;
        if status["ready"].as_bool().unwrap_or(false) && !status["state"]["running"].is_null() {
            ready += 1;
        }
    }

    PodCounts {
        ready,
        total,
        restarts,
    }
}

/// One container of a pod, as a container selector offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodContainer {
    /// The name, which is what `kubectl exec -c` and [`crate::exec`] take.
    pub name: Arc<str>,
    /// Whether it is an init container.
    ///
    /// Worth saying out loud next to a Run button: an exec only reaches one of
    /// these while it is still running, which for a sidecar is the pod's whole
    /// life and for an ordinary init container is a few seconds near the start
    /// of it.
    pub init: bool,
}

/// The containers of a pod, in the order a selector should offer them.
///
/// `spec.containers` first, because the apiserver's default is the first of
/// those, then `spec.initContainers`. Reads the object rather than the pod's
/// statuses, so a pod that has not started yet still lists what it will run.
pub fn containers(data: &Value) -> Vec<PodContainer> {
    let declared = |field: &str, init: bool| {
        array(data, &["spec", field])
            .iter()
            .map(|container| text(container, &["name"]))
            // A container with no name cannot be exec'd into and cannot be
            // labelled, so offering it would be offering nothing.
            .filter(|name| !name.is_empty())
            .map(|name| PodContainer {
                name: Arc::from(name),
                init,
            })
            .collect::<Vec<_>>()
    };

    let mut containers = declared("containers", false);
    containers.extend(declared("initContainers", true));
    containers
}

/// The `Init:...` status for a pod still working through its init containers,
/// or `None` once they are all done.
fn initializing_reason(data: &Value) -> Option<String> {
    let init_count = array(data, &["spec", "initContainers"]).len();

    for (index, status) in array(data, &["status", "initContainerStatuses"])
        .iter()
        .enumerate()
    {
        let terminated = &status["state"]["terminated"];
        let waiting = &status["state"]["waiting"];

        // A finished init container, or a running sidecar, is not blocking.
        if !terminated.is_null() && terminated["exitCode"].as_i64() == Some(0) {
            continue;
        }
        if is_restartable(init_container(data, text(status, &["name"])).as_ref())
            && status["started"].as_bool() == Some(true)
        {
            continue;
        }

        return Some(if !terminated.is_null() {
            match text(terminated, &["reason"]) {
                "" => match terminated["signal"].as_i64() {
                    Some(signal) if signal != 0 => format!("Init:Signal:{signal}"),
                    _ => format!(
                        "Init:ExitCode:{}",
                        terminated["exitCode"].as_i64().unwrap_or_default()
                    ),
                },
                reason => format!("Init:{reason}"),
            }
        } else if !waiting.is_null() {
            match text(waiting, &["reason"]) {
                "" | POD_INITIALIZING => format!("Init:{index}/{init_count}"),
                reason => format!("Init:{reason}"),
            }
        } else {
            format!("Init:{index}/{init_count}")
        });
    }

    None
}

/// The STATUS column.
pub fn status(object: &DynamicObject) -> String {
    let data = &object.data;
    let phase = text(data, &["status", "phase"]);
    let status_reason = text(data, &["status", "reason"]);

    let mut reason = if status_reason.is_empty() {
        phase.to_owned()
    } else {
        status_reason.to_owned()
    };

    // A gated pod is not pending on resources; say which it is.
    if array(data, &["status", "conditions"])
        .iter()
        .any(|condition| {
            text(condition, &["type"]) == "PodScheduled"
                && text(condition, &["reason"]) == "SchedulingGated"
        })
    {
        reason = "SchedulingGated".to_owned();
    }

    match initializing_reason(data) {
        Some(init) if !condition_is_true(data, "Initialized") => reason = init,
        _ => {
            let mut has_running = false;
            // Last container wins, matching kubectl's reverse iteration.
            for status in array(data, &["status", "containerStatuses"]).iter().rev() {
                let waiting = &status["state"]["waiting"];
                let terminated = &status["state"]["terminated"];

                if !waiting.is_null() && !text(waiting, &["reason"]).is_empty() {
                    reason = text(waiting, &["reason"]).to_owned();
                } else if !terminated.is_null() {
                    reason = match text(terminated, &["reason"]) {
                        "" => match terminated["signal"].as_i64() {
                            Some(signal) if signal != 0 => format!("Signal:{signal}"),
                            _ => format!(
                                "ExitCode:{}",
                                terminated["exitCode"].as_i64().unwrap_or_default()
                            ),
                        },
                        text => text.to_owned(),
                    };
                } else if status["ready"].as_bool().unwrap_or(false)
                    && !status["state"]["running"].is_null()
                {
                    has_running = true;
                }
            }

            // A pod whose last container completed but which still has running
            // containers is not "Completed".
            if reason == "Completed" && has_running {
                reason = if condition_is_true(data, "Ready") {
                    "Running".to_owned()
                } else {
                    "NotReady".to_owned()
                };
            }
        }
    }

    if object.metadata.deletion_timestamp.is_some() {
        // A pod on a lost node is not shutting down; nobody knows what it is.
        if status_reason == NODE_UNREACHABLE {
            return "Unknown".to_owned();
        }
        if !matches!(phase, "Succeeded" | "Failed") {
            return "Terminating".to_owned();
        }
    }

    reason
}

/// The node a pod is scheduled on, once it is scheduled.
pub fn node(object: &DynamicObject) -> Option<&str> {
    match text(&object.data, &["spec", "nodeName"]) {
        "" => None,
        node => Some(node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> DynamicObject {
        serde_json::from_value(value).expect("fixture is a valid object")
    }

    fn running_pod() -> DynamicObject {
        pod(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-0",
                "namespace": "payments",
                "uid": "1f0d",
                "creationTimestamp": "2026-08-17T10:00:00Z"
            },
            "spec": { "nodeName": "node-1", "containers": [{ "name": "api" }] },
            "status": {
                "phase": "Running",
                "conditions": [{ "type": "Ready", "status": "True" }],
                "containerStatuses": [{
                    "name": "api",
                    "ready": true,
                    "restartCount": 0,
                    "state": { "running": { "startedAt": "2026-08-17T10:00:05Z" } }
                }]
            }
        }))
    }

    #[test]
    fn a_running_pod_reads_as_running() {
        let pod = running_pod();
        assert_eq!(status(&pod), "Running");
        assert_eq!(
            counts(&pod.data),
            PodCounts {
                ready: 1,
                total: 1,
                restarts: 0
            }
        );
        assert_eq!(node(&pod), Some("node-1"));
    }

    #[test]
    fn a_crash_looping_pod_reports_the_waiting_reason_not_the_phase() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": {
                "phase": "Running",
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 7,
                    "state": { "waiting": { "reason": "CrashLoopBackOff" } }
                }]
            }
        }));

        assert_eq!(status(&pod), "CrashLoopBackOff");
        assert_eq!(
            counts(&pod.data),
            PodCounts {
                ready: 0,
                total: 1,
                restarts: 7
            }
        );
    }

    #[test]
    fn an_initializing_pod_reports_progress_through_its_init_containers() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }],
                "initContainers": [{ "name": "migrate" }, { "name": "seed" }]
            },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "migrate",
                    "ready": false,
                    "restartCount": 1,
                    "state": { "waiting": { "reason": "PodInitializing" } }
                }],
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "waiting": { "reason": "PodInitializing" } }
                }]
            }
        }));

        assert_eq!(status(&pod), "Init:0/2");
        // Init-container restarts are what matters while initializing.
        assert_eq!(counts(&pod.data).restarts, 1);
        assert_eq!(counts(&pod.data).ready, 0);
    }

    #[test]
    fn a_failing_init_container_names_the_reason() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }], "initContainers": [{ "name": "migrate" }] },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "migrate",
                    "ready": false,
                    "restartCount": 3,
                    "state": { "waiting": { "reason": "ImagePullBackOff" } }
                }]
            }
        }));

        assert_eq!(status(&pod), "Init:ImagePullBackOff");
    }

    #[test]
    fn an_init_container_that_exited_nonzero_shows_its_reason() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }], "initContainers": [{ "name": "migrate" }] },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "migrate",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "terminated": { "exitCode": 2, "reason": "Error" } }
                }]
            }
        }));

        assert_eq!(status(&pod), "Init:Error");
    }

    #[test]
    fn an_init_container_killed_without_a_reason_shows_its_exit_code() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }], "initContainers": [{ "name": "migrate" }] },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{
                    "name": "migrate",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "terminated": { "exitCode": 137 } }
                }]
            }
        }));

        assert_eq!(status(&pod), "Init:ExitCode:137");
    }

    #[test]
    fn a_running_sidecar_counts_towards_ready_and_does_not_block_init() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }],
                "initContainers": [{ "name": "proxy", "restartPolicy": "Always" }]
            },
            "status": {
                "phase": "Running",
                "conditions": [
                    { "type": "Initialized", "status": "True" },
                    { "type": "Ready", "status": "True" }
                ],
                "initContainerStatuses": [{
                    "name": "proxy",
                    "ready": true,
                    "started": true,
                    "restartCount": 0,
                    "state": { "running": {} }
                }],
                "containerStatuses": [{
                    "name": "api",
                    "ready": true,
                    "restartCount": 0,
                    "state": { "running": {} }
                }]
            }
        }));

        assert_eq!(status(&pod), "Running");
        // A running sidecar is ready, and it is in the total, so a healthy pod
        // reads 2/2. Counting it in the total and not in the ready count left
        // every pod with a sidecar looking permanently one container short.
        assert_eq!(
            counts(&pod.data),
            PodCounts {
                ready: 2,
                total: 2,
                restarts: 0
            }
        );
    }

    #[test]
    fn a_crash_looping_sidecar_is_visible_in_the_row() {
        // A sidecar's status lives in `initContainerStatuses`, and the counts
        // read only `containerStatuses` — so a sidecar restarting every ten
        // seconds showed RESTARTS 0 and READY 1/2 with no hint of which
        // container was the problem. RESTARTS exists for exactly this.
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }],
                "initContainers": [{ "name": "proxy", "restartPolicy": "Always" }]
            },
            "status": {
                "phase": "Running",
                "conditions": [{ "type": "Initialized", "status": "True" }],
                "initContainerStatuses": [{
                    "name": "proxy",
                    "ready": false,
                    "started": false,
                    "restartCount": 14,
                    "state": { "waiting": { "reason": "CrashLoopBackOff" } }
                }],
                "containerStatuses": [{
                    "name": "api",
                    "ready": true,
                    "restartCount": 0,
                    "state": { "running": {} }
                }]
            }
        }));

        assert_eq!(
            counts(&pod.data),
            PodCounts {
                ready: 1,
                total: 2,
                restarts: 14
            }
        );
    }

    #[test]
    fn an_ordinary_init_containers_restarts_stop_counting_once_it_is_done() {
        // Only sidecars keep contributing after initialization; `kubectl`
        // drops an ordinary init container's restarts at that point, and a
        // RESTARTS column that disagrees with `kubectl` is worse than none.
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }],
                "initContainers": [{ "name": "migrate" }]
            },
            "status": {
                "phase": "Running",
                "conditions": [{ "type": "Initialized", "status": "True" }],
                "initContainerStatuses": [{
                    "name": "migrate",
                    "restartCount": 3,
                    "state": { "terminated": { "exitCode": 0 } }
                }],
                "containerStatuses": [{
                    "name": "api",
                    "ready": true,
                    "restartCount": 1,
                    "state": { "running": {} }
                }]
            }
        }));

        assert_eq!(
            counts(&pod.data),
            PodCounts {
                ready: 1,
                total: 1,
                restarts: 1
            }
        );
    }

    #[test]
    fn a_deleting_pod_reads_terminating() {
        let mut pod = running_pod();
        pod.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
            ));

        assert_eq!(status(&pod), "Terminating");
    }

    #[test]
    fn a_pod_on_a_lost_node_reads_unknown_rather_than_terminating() {
        let mut pod = running_pod();
        pod.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
            ));
        pod.data["status"]["reason"] = json!(NODE_UNREACHABLE);

        assert_eq!(status(&pod), "Unknown");
    }

    #[test]
    fn a_completed_pod_keeps_its_terminated_reason_when_deleted() {
        let pod = pod(json!({
            "metadata": { "name": "job-0", "namespace": "default", "deletionTimestamp": "2026-08-17T10:00:00Z" },
            "spec": { "containers": [{ "name": "run" }] },
            "status": {
                "phase": "Succeeded",
                "containerStatuses": [{
                    "name": "run",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "terminated": { "exitCode": 0, "reason": "Completed" } }
                }]
            }
        }));

        // Terminal pods are not "Terminating": there is nothing left to stop.
        assert_eq!(status(&pod), "Completed");
    }

    #[test]
    fn a_gated_pod_says_so_instead_of_pending() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": {
                "phase": "Pending",
                "conditions": [{
                    "type": "PodScheduled",
                    "status": "False",
                    "reason": "SchedulingGated"
                }]
            }
        }));

        assert_eq!(status(&pod), "SchedulingGated");
    }

    #[test]
    fn an_evicted_pod_reports_the_status_reason() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": { "phase": "Failed", "reason": "Evicted" }
        }));

        assert_eq!(status(&pod), "Evicted");
    }

    #[test]
    fn a_killed_container_without_a_reason_shows_its_signal() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": {
                "phase": "Running",
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 0,
                    "state": { "terminated": { "exitCode": 137, "signal": 9 } }
                }]
            }
        }));

        assert_eq!(status(&pod), "Signal:9");
    }

    #[test]
    fn an_unscheduled_pod_has_no_node() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": { "phase": "Pending" }
        }));

        assert_eq!(node(&pod), None);
        assert_eq!(status(&pod), "Pending");
        assert_eq!(counts(&pod.data).total, 1);
    }

    #[test]
    fn a_pods_containers_are_listed_before_its_init_containers() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }, { "name": "envoy" }],
                "initContainers": [{ "name": "migrate" }]
            }
        }));

        let containers = containers(&pod.data);
        let names: Vec<_> = containers.iter().map(|c| &*c.name).collect();
        assert_eq!(names, ["api", "envoy", "migrate"]);
        // The first of `spec.containers` is what the apiserver would have
        // picked, so it is also the first thing a selector offers.
        assert!(!containers[0].init);
        assert!(containers[2].init);
    }

    #[test]
    fn a_pod_that_has_not_started_still_lists_what_it_will_run() {
        // The containers come from the spec, not from the statuses, so a
        // pending pod can still have a command aimed at one of them.
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": { "phase": "Pending" }
        }));

        assert_eq!(containers(&pod.data).len(), 1);
    }

    #[test]
    fn an_object_that_is_not_a_pod_offers_no_containers() {
        let service = pod(json!({
            "metadata": { "name": "api", "namespace": "default" },
            "spec": { "ports": [{ "port": 80 }] }
        }));

        assert!(containers(&service.data).is_empty());
    }
}
