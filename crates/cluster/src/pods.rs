//! Projecting `Pod` objects into the rows the table renders.
//!
//! The STATUS column is not `status.phase`. `kubectl get pods` computes it from
//! init-container state, waiting reasons, termination reasons and the deletion
//! timestamp, which is why a crash-looping pod reads `CrashLoopBackOff` rather
//! than `Running`. Anyone coming from `kubectl` or k9s expects those exact
//! strings, so this module reimplements that logic rather than inventing its
//! own vocabulary.
//!
//! Reference: `printPod` in `kubectl/pkg/printers/internalversion/printers.go`.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use k8s_openapi::api::core::v1::{Container, ContainerStatus, Pod, PodCondition};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use periscope_bridge::{PodSnapshot, ResourceKey};

/// The reason the apiserver reports for a pod on a node that stopped answering.
const NODE_UNREACHABLE: &str = "NodeLost";

/// Waiting reason that means "nothing is wrong, init is simply not done".
const POD_INITIALIZING: &str = "PodInitializing";

/// Reduces a pod to the columns the table shows.
pub fn project(pod: &Pod) -> PodSnapshot {
    let namespace = pod.metadata.namespace.as_deref().unwrap_or_default();
    let name = pod.metadata.name.as_deref().unwrap_or_default();
    let counts = counts(pod);

    PodSnapshot {
        key: ResourceKey::new(namespace, name),
        uid: pod.metadata.uid.as_deref().map(Arc::from),
        status: Arc::from(status(pod).as_str()),
        ready: counts.ready,
        containers: counts.total,
        restarts: counts.restarts,
        node: pod
            .spec
            .as_ref()
            .and_then(|spec| spec.node_name.as_deref())
            .map(Arc::from),
        created: pod.metadata.creation_timestamp.as_ref().map(to_system_time),
    }
}

/// Converts a Kubernetes timestamp to a `SystemTime`.
///
/// Pre-1970 timestamps cannot occur on a real object, but the arithmetic is
/// written so one would produce a sane value rather than an overflow.
fn to_system_time(time: &Time) -> SystemTime {
    let seconds = time.0.as_second();
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

/// The READY and RESTARTS columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    ready: u32,
    total: u32,
    restarts: u32,
}

/// Whether an init container is a sidecar — one that keeps running alongside
/// the main containers, and so counts towards the READY total.
fn is_restartable(container: Option<&Container>) -> bool {
    container.is_some_and(|c| c.restart_policy.as_deref() == Some("Always"))
}

fn container_statuses(pod: &Pod) -> &[ContainerStatus] {
    pod.status
        .as_ref()
        .and_then(|status| status.container_statuses.as_deref())
        .unwrap_or_default()
}

fn init_container_statuses(pod: &Pod) -> &[ContainerStatus] {
    pod.status
        .as_ref()
        .and_then(|status| status.init_container_statuses.as_deref())
        .unwrap_or_default()
}

fn init_containers(pod: &Pod) -> &[Container] {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_deref())
        .unwrap_or_default()
}

fn conditions(pod: &Pod) -> &[PodCondition] {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_deref())
        .unwrap_or_default()
}

fn condition_is_true(pod: &Pod, kind: &str) -> bool {
    conditions(pod)
        .iter()
        .any(|condition| condition.type_ == kind && condition.status == "True")
}

fn counts(pod: &Pod) -> Counts {
    let init = init_containers(pod);
    let mut total = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.containers.len().try_into().ok())
        .unwrap_or(0);
    total += init.iter().filter(|c| is_restartable(Some(c))).count() as u32;

    let initializing = initializing_reason(pod).is_some();

    // While a pod is initializing, kubectl reports the init containers'
    // restarts and no ready containers at all.
    if initializing && !condition_is_true(pod, "Initialized") {
        return Counts {
            ready: 0,
            total,
            restarts: init_container_statuses(pod)
                .iter()
                .map(|status| status.restart_count.max(0) as u32)
                .sum(),
        };
    }

    let mut ready = 0;
    let mut restarts = 0;
    for status in container_statuses(pod) {
        restarts += status.restart_count.max(0) as u32;
        let running = status
            .state
            .as_ref()
            .is_some_and(|state| state.running.is_some());
        if status.ready && running {
            ready += 1;
        }
    }

    Counts {
        ready,
        total,
        restarts,
    }
}

/// The `Init:...` status for a pod still working through its init containers,
/// or `None` once they are all done.
fn initializing_reason(pod: &Pod) -> Option<String> {
    let init = init_containers(pod);
    let by_name = |name: &str| init.iter().find(|container| container.name == name);

    for (index, status) in init_container_statuses(pod).iter().enumerate() {
        let state = status.state.as_ref();
        let terminated = state.and_then(|state| state.terminated.as_ref());
        let waiting = state.and_then(|state| state.waiting.as_ref());

        // A finished init container, or a running sidecar, is not blocking.
        if terminated.is_some_and(|terminated| terminated.exit_code == 0) {
            continue;
        }
        if is_restartable(by_name(&status.name)) && status.started == Some(true) {
            continue;
        }

        return Some(match (terminated, waiting) {
            (Some(terminated), _) => match terminated.reason.as_deref() {
                Some(reason) if !reason.is_empty() => format!("Init:{reason}"),
                _ => match terminated.signal {
                    Some(signal) if signal != 0 => format!("Init:Signal:{signal}"),
                    _ => format!("Init:ExitCode:{}", terminated.exit_code),
                },
            },
            (None, Some(waiting)) => match waiting.reason.as_deref() {
                Some(reason) if !reason.is_empty() && reason != POD_INITIALIZING => {
                    format!("Init:{reason}")
                }
                _ => format!("Init:{index}/{}", init.len()),
            },
            (None, None) => format!("Init:{index}/{}", init.len()),
        });
    }

    None
}

/// The STATUS column.
fn status(pod: &Pod) -> String {
    let pod_status = pod.status.as_ref();
    let phase = pod_status
        .and_then(|status| status.phase.as_deref())
        .unwrap_or_default();
    let status_reason = pod_status
        .and_then(|status| status.reason.as_deref())
        .unwrap_or_default();

    let mut reason = if status_reason.is_empty() {
        phase.to_owned()
    } else {
        status_reason.to_owned()
    };

    // A gated pod is not pending on resources; say which it is.
    if conditions(pod).iter().any(|condition| {
        condition.type_ == "PodScheduled" && condition.reason.as_deref() == Some("SchedulingGated")
    }) {
        reason = "SchedulingGated".to_owned();
    }

    let initializing = initializing_reason(pod);
    match &initializing {
        Some(init) if !condition_is_true(pod, "Initialized") => reason = init.clone(),
        _ => {
            let mut has_running = false;
            // Last container wins, matching kubectl's reverse iteration.
            for status in container_statuses(pod).iter().rev() {
                let state = status.state.as_ref();
                let waiting = state.and_then(|state| state.waiting.as_ref());
                let terminated = state.and_then(|state| state.terminated.as_ref());

                if let Some(waiting_reason) = waiting
                    .and_then(|waiting| waiting.reason.as_deref())
                    .filter(|reason| !reason.is_empty())
                {
                    reason = waiting_reason.to_owned();
                } else if let Some(terminated) = terminated {
                    reason = match terminated.reason.as_deref() {
                        Some(text) if !text.is_empty() => text.to_owned(),
                        _ => match terminated.signal {
                            Some(signal) if signal != 0 => format!("Signal:{signal}"),
                            _ => format!("ExitCode:{}", terminated.exit_code),
                        },
                    };
                } else if status.ready && state.is_some_and(|state| state.running.is_some()) {
                    has_running = true;
                }
            }

            // A pod whose last container completed but which still has running
            // containers is not "Completed".
            if reason == "Completed" && has_running {
                reason = if condition_is_true(pod, "Ready") {
                    "Running".to_owned()
                } else {
                    "NotReady".to_owned()
                };
            }
        }
    }

    if pod.metadata.deletion_timestamp.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).expect("fixture is a valid pod")
    }

    fn running_pod() -> Pod {
        pod(json!({
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
    fn a_running_pod_projects_every_column() {
        let snapshot = project(&running_pod());

        assert_eq!(snapshot.key, ResourceKey::new("payments", "api-0"));
        assert_eq!(snapshot.uid.as_deref(), Some("1f0d"));
        assert_eq!(&*snapshot.status, "Running");
        assert_eq!((snapshot.ready, snapshot.containers), (1, 1));
        assert_eq!(snapshot.restarts, 0);
        assert_eq!(snapshot.node.as_deref(), Some("node-1"));
        // 2026-08-17T10:00:00Z.
        assert_eq!(
            snapshot.created,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_960_800))
        );
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

        let snapshot = project(&pod);
        assert_eq!(&*snapshot.status, "CrashLoopBackOff");
        assert_eq!(snapshot.restarts, 7);
        assert_eq!((snapshot.ready, snapshot.containers), (0, 1));
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

        let snapshot = project(&pod);
        assert_eq!(&*snapshot.status, "Init:0/2");
        // Init-container restarts are what matters while initializing.
        assert_eq!(snapshot.restarts, 1);
        assert_eq!((snapshot.ready, snapshot.containers), (0, 1));
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

        assert_eq!(&*project(&pod).status, "Init:ImagePullBackOff");
    }

    #[test]
    fn an_init_container_that_exited_nonzero_shows_its_exit_code() {
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

        assert_eq!(&*project(&pod).status, "Init:Error");
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

        let snapshot = project(&pod);
        assert_eq!(&*snapshot.status, "Running");
        // The sidecar is part of the total, but only main containers are
        // counted ready — exactly what `kubectl get pods` prints.
        assert_eq!((snapshot.ready, snapshot.containers), (1, 2));
    }

    #[test]
    fn a_deleting_pod_reads_terminating() {
        let mut pod = running_pod();
        pod.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH));

        assert_eq!(&*project(&pod).status, "Terminating");
    }

    #[test]
    fn a_pod_on_a_lost_node_reads_unknown_rather_than_terminating() {
        let mut pod = running_pod();
        pod.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH));
        pod.status.as_mut().unwrap().reason = Some(NODE_UNREACHABLE.to_owned());

        assert_eq!(&*project(&pod).status, "Unknown");
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
        assert_eq!(&*project(&pod).status, "Completed");
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

        assert_eq!(&*project(&pod).status, "SchedulingGated");
    }

    #[test]
    fn an_evicted_pod_reports_the_status_reason() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": { "phase": "Failed", "reason": "Evicted" }
        }));

        assert_eq!(&*project(&pod).status, "Evicted");
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

        assert_eq!(&*project(&pod).status, "Signal:9");
    }

    #[test]
    fn an_unscheduled_pod_has_no_node() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] },
            "status": { "phase": "Pending" }
        }));

        let snapshot = project(&pod);
        assert_eq!(snapshot.node, None);
        assert_eq!(&*snapshot.status, "Pending");
        assert_eq!((snapshot.ready, snapshot.containers), (0, 1));
    }
}
