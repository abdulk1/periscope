//! What each kind's table looks like, and how to fill it in.
//!
//! A kind is projected by a plain function from the object to a row of cells
//! plus a state. Core kinds get the columns `kubectl` prints. A CRD gets the
//! columns its own CustomResourceDefinition declares, read by [`crate::printer`]
//! and evaluated here. Everything else — a kind neither this module nor its CRD
//! has anything to say about — falls back to a listing that works without
//! knowing anything about the kind. That fallback is what makes "lists all
//! custom resources without special-casing" true rather than aspirational.
//!
//! Columns are data, not code in the UI: they travel with the rows, so a kind
//! this module has never heard of still renders correctly.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::DynamicObject;
use periscope_bridge::{ColumnSpec, KindId, ResourceKey, ResourceRow, RowState};
use serde_json::Value;

use crate::pods;
use crate::printer::{self, PrinterColumn};

/// How to render one kind.
#[derive(Clone)]
pub struct KindColumns {
    /// The kind-specific columns, between NAME and AGE.
    pub columns: Arc<[ColumnSpec]>,
    /// Fills those columns in for one object.
    project: Projection,
}

/// Where a row's cells come from.
///
/// Built-ins stay a bare function pointer — a projector this module wrote has
/// nothing to capture — while a CRD's columns carry the parsed JSONPaths they
/// are evaluated from, which a function pointer cannot.
#[derive(Clone)]
enum Projection {
    /// A kind this module knows the fields of.
    BuiltIn(fn(&DynamicObject) -> (Vec<Arc<str>>, RowState)),
    /// The printer columns a CustomResourceDefinition declared.
    Declared(Arc<[PrinterColumn]>),
}

impl std::fmt::Debug for KindColumns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KindColumns")
            .field("columns", &self.columns)
            .finish_non_exhaustive()
    }
}

impl KindColumns {
    /// Projects one object into a table row.
    pub fn row(&self, object: &DynamicObject) -> ResourceRow {
        self.row_at(object, SystemTime::now())
    }

    /// Projects one object into a table row, as of `now`.
    ///
    /// The clock is a parameter because a CRD may declare a `date` column, and
    /// a cell that renders an age is only reproducible against a fixed one.
    pub fn row_at(&self, object: &DynamicObject, now: SystemTime) -> ResourceRow {
        let (cells, state) = match &self.project {
            Projection::BuiltIn(project) => project(object),
            Projection::Declared(columns) => (
                printer::cells(columns, &object.data, now),
                conventional_state(&object.data),
            ),
        };

        ResourceRow {
            key: key_of(object),
            uid: object.metadata.uid.as_deref().map(Arc::from),
            cells: Arc::from(cells),
            state,
            created: object
                .metadata
                .creation_timestamp
                .as_ref()
                .map(to_system_time),
        }
    }
}

/// The key an object is stored under.
pub fn key_of(object: &DynamicObject) -> ResourceKey {
    ResourceKey::new(
        object.metadata.namespace.as_deref().unwrap_or_default(),
        object.metadata.name.as_deref().unwrap_or_default(),
    )
}

/// Converts a Kubernetes timestamp to a `SystemTime`.
pub(crate) fn to_system_time(time: &Time) -> SystemTime {
    let seconds = time.0.as_second();
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

fn cell(text: impl AsRef<str>) -> Arc<str> {
    Arc::from(text.as_ref())
}

fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for step in path {
        current = &current[step];
    }
    current.as_str().unwrap_or_default()
}

fn number(value: &Value, path: &[&str]) -> i64 {
    let mut current = value;
    for step in path {
        current = &current[step];
    }
    current.as_i64().unwrap_or(0)
}

fn count(value: &Value, path: &[&str]) -> usize {
    let mut current = value;
    for step in path {
        current = &current[step];
    }
    current.as_array().map_or(0, Vec::len)
}

/// `ready/desired`, and whether that is all of them.
fn ratio(ready: i64, desired: i64) -> (Arc<str>, RowState) {
    let state = if desired == 0 {
        RowState::Neutral
    } else if ready >= desired {
        RowState::Healthy
    } else {
        RowState::Transient
    };
    (cell(format!("{ready}/{desired}")), state)
}

/// Whether a condition is `True`.
fn condition_is_true(data: &Value, kind: &str) -> bool {
    data["status"]["conditions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .any(|condition| {
            text(condition, &["type"]) == kind && text(condition, &["status"]) == "True"
        })
}

fn pods_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("READY", 72),
            ColumnSpec::fixed("STATUS", 180),
            ColumnSpec::fixed("RESTARTS", 90),
            ColumnSpec::fixed("NODE", 200),
        ]),
        project: Projection::BuiltIn(|object| {
            let counts = pods::counts(&object.data);
            let status = pods::status(object);
            let state = pod_state(&status, counts.ready == counts.total);

            (
                vec![
                    cell(format!("{}/{}", counts.ready, counts.total)),
                    cell(&status),
                    cell(counts.restarts.to_string()),
                    cell(pods::node(object).unwrap_or("—")),
                ],
                state,
            )
        }),
    }
}

/// Classifies a pod STATUS string.
///
/// Driven by the strings `kubectl` produces, so anything unrecognised is
/// deliberately neutral rather than guessed at.
fn pod_state(status: &str, all_ready: bool) -> RowState {
    if let Some(rest) = status.strip_prefix("Init:") {
        // `Init:Error` is a failure; `Init:0/2` is just progress.
        return match rest.chars().next() {
            Some(first) if first.is_ascii_digit() => RowState::Transient,
            _ => pod_state(rest, all_ready),
        };
    }

    match status {
        "Running" if all_ready => RowState::Healthy,
        "Running" => RowState::Transient,
        "Completed" | "Succeeded" => RowState::Healthy,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "SchedulingGated"
        | "NotReady" => RowState::Transient,
        "CrashLoopBackOff"
        | "Error"
        | "Failed"
        | "Evicted"
        | "OOMKilled"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "CreateContainerConfigError"
        | "CreateContainerError"
        | "InvalidImageName"
        | "Unknown" => RowState::Failing,
        other if other.starts_with("Signal:") || other.starts_with("ExitCode:") => {
            RowState::Failing
        }
        _ => RowState::Neutral,
    }
}

fn deployment_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("READY", 90),
            ColumnSpec::fixed("UP-TO-DATE", 110),
            ColumnSpec::fixed("AVAILABLE", 110),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let desired = number(data, &["spec", "replicas"]);
            let (ready, state) = ratio(number(data, &["status", "readyReplicas"]), desired);

            (
                vec![
                    ready,
                    cell(number(data, &["status", "updatedReplicas"]).to_string()),
                    cell(number(data, &["status", "availableReplicas"]).to_string()),
                ],
                state,
            )
        }),
    }
}

fn statefulset_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([ColumnSpec::fixed("READY", 90)]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let (ready, state) = ratio(
                number(data, &["status", "readyReplicas"]),
                number(data, &["spec", "replicas"]),
            );
            (vec![ready], state)
        }),
    }
}

fn replicaset_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("DESIRED", 90),
            ColumnSpec::fixed("CURRENT", 90),
            ColumnSpec::fixed("READY", 90),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let desired = number(data, &["spec", "replicas"]);
            let ready = number(data, &["status", "readyReplicas"]);
            let (_, state) = ratio(ready, desired);

            (
                vec![
                    cell(desired.to_string()),
                    cell(number(data, &["status", "replicas"]).to_string()),
                    cell(ready.to_string()),
                ],
                state,
            )
        }),
    }
}

fn daemonset_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("DESIRED", 90),
            ColumnSpec::fixed("READY", 90),
            ColumnSpec::fixed("AVAILABLE", 110),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let desired = number(data, &["status", "desiredNumberScheduled"]);
            let ready = number(data, &["status", "numberReady"]);
            let (_, state) = ratio(ready, desired);

            (
                vec![
                    cell(desired.to_string()),
                    cell(ready.to_string()),
                    cell(number(data, &["status", "numberAvailable"]).to_string()),
                ],
                state,
            )
        }),
    }
}

fn service_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("TYPE", 130),
            ColumnSpec::fixed("CLUSTER-IP", 140),
            ColumnSpec::fixed("EXTERNAL-IP", 160),
            ColumnSpec::fixed("PORTS", 180),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let kind = text(data, &["spec", "type"]);
            let ports: Vec<String> = data["spec"]["ports"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|port| {
                    let protocol = match text(port, &["protocol"]) {
                        "" => "TCP".to_owned(),
                        protocol => protocol.to_owned(),
                    };
                    match number(port, &["nodePort"]) {
                        0 => format!("{}/{protocol}", number(port, &["port"])),
                        node => format!("{}:{node}/{protocol}", number(port, &["port"])),
                    }
                })
                .collect();

            // A LoadBalancer with no address yet is the one service state worth
            // flagging: it is why traffic is not arriving.
            let external = external_ips(data);
            let state = if kind == "LoadBalancer" && external == "<pending>" {
                RowState::Transient
            } else {
                RowState::Neutral
            };

            (
                vec![
                    cell(kind),
                    cell(cluster_ip(data)),
                    cell(external),
                    cell(ports.join(",")),
                ],
                state,
            )
        }),
    }
}

fn cluster_ip(data: &Value) -> &str {
    match text(data, &["spec", "clusterIP"]) {
        "" => "<none>",
        ip => ip,
    }
}

fn external_ips(data: &Value) -> String {
    if let Some(external) = data["spec"]["externalIPs"].as_array()
        && !external.is_empty()
    {
        return external
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(",");
    }

    let ingress = data["status"]["loadBalancer"]["ingress"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !ingress.is_empty() {
        return ingress
            .iter()
            .map(|entry| match text(entry, &["ip"]) {
                "" => text(entry, &["hostname"]).to_owned(),
                ip => ip.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(",");
    }

    if text(data, &["spec", "type"]) == "LoadBalancer" {
        return "<pending>".to_owned();
    }
    "<none>".to_owned()
}

fn node_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("STATUS", 140),
            ColumnSpec::fixed("ROLES", 160),
            ColumnSpec::fixed("VERSION", 120),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let ready = condition_is_true(data, "Ready");
            let schedulable = !data["spec"]["unschedulable"].as_bool().unwrap_or(false);

            let status = match (ready, schedulable) {
                (true, true) => "Ready".to_owned(),
                (true, false) => "Ready,SchedulingDisabled".to_owned(),
                (false, true) => "NotReady".to_owned(),
                (false, false) => "NotReady,SchedulingDisabled".to_owned(),
            };

            let roles: Vec<String> = object
                .metadata
                .labels
                .as_ref()
                .map(|labels| {
                    labels
                        .keys()
                        .filter_map(|label| label.strip_prefix("node-role.kubernetes.io/"))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            let state = if !ready {
                RowState::Failing
            } else if !schedulable {
                RowState::Transient
            } else {
                RowState::Healthy
            };

            (
                vec![
                    cell(status),
                    cell(if roles.is_empty() {
                        "<none>".to_owned()
                    } else {
                        roles.join(",")
                    }),
                    cell(text(data, &["status", "nodeInfo", "kubeletVersion"])),
                ],
                state,
            )
        }),
    }
}

fn job_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("COMPLETIONS", 130),
            ColumnSpec::fixed("STATUS", 130),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let succeeded = number(data, &["status", "succeeded"]);
            let wanted = match number(data, &["spec", "completions"]) {
                0 => 1,
                completions => completions,
            };
            let failed = number(data, &["status", "failed"]);

            let (status, state) = if failed > 0 {
                ("Failed", RowState::Failing)
            } else if succeeded >= wanted {
                ("Complete", RowState::Healthy)
            } else {
                ("Running", RowState::Transient)
            };

            (
                vec![cell(format!("{succeeded}/{wanted}")), cell(status)],
                state,
            )
        }),
    }
}

fn cronjob_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("SCHEDULE", 140),
            ColumnSpec::fixed("SUSPEND", 90),
            ColumnSpec::fixed("ACTIVE", 90),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let suspended = data["spec"]["suspend"].as_bool().unwrap_or(false);

            (
                vec![
                    cell(text(data, &["spec", "schedule"])),
                    cell(suspended.to_string()),
                    cell(count(data, &["status", "active"]).to_string()),
                ],
                // A suspended CronJob is not broken, but it is not running
                // either, and that is worth seeing at a glance.
                if suspended {
                    RowState::Transient
                } else {
                    RowState::Neutral
                },
            )
        }),
    }
}

fn configmap_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([ColumnSpec::fixed("DATA", 90)]),
        project: Projection::BuiltIn(|object| {
            let keys = object.data["data"].as_object().map_or(0, |data| data.len())
                + object.data["binaryData"]
                    .as_object()
                    .map_or(0, |data| data.len());
            (vec![cell(keys.to_string())], RowState::Neutral)
        }),
    }
}

fn secret_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("TYPE", 240),
            ColumnSpec::fixed("DATA", 90),
        ]),
        project: Projection::BuiltIn(|object| {
            // Only the key count, never a value: secrets are masked by default
            // and the table is not where a reveal belongs.
            let keys = object.data["data"].as_object().map_or(0, |data| data.len());
            (
                vec![cell(text(&object.data, &["type"])), cell(keys.to_string())],
                RowState::Neutral,
            )
        }),
    }
}

fn ingress_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("CLASS", 140),
            ColumnSpec::fixed("HOSTS", 220),
            ColumnSpec::fixed("ADDRESS", 180),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let hosts: Vec<&str> = data["spec"]["rules"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|rule| text(rule, &["host"]))
                .filter(|host| !host.is_empty())
                .collect();

            let address = data["status"]["loadBalancer"]["ingress"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|entry| match text(entry, &["ip"]) {
                    "" => text(entry, &["hostname"]),
                    ip => ip,
                })
                .collect::<Vec<_>>()
                .join(",");

            (
                vec![
                    cell(match text(data, &["spec", "ingressClassName"]) {
                        "" => "<none>",
                        class => class,
                    }),
                    cell(if hosts.is_empty() {
                        "*".to_owned()
                    } else {
                        hosts.join(",")
                    }),
                    cell(address),
                ],
                RowState::Neutral,
            )
        }),
    }
}

fn pvc_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("STATUS", 120),
            ColumnSpec::fixed("VOLUME", 220),
            ColumnSpec::fixed("CAPACITY", 110),
            ColumnSpec::fixed("STORAGECLASS", 160),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let phase = text(data, &["status", "phase"]);
            let state = match phase {
                "Bound" => RowState::Healthy,
                "Pending" => RowState::Transient,
                "Lost" => RowState::Failing,
                _ => RowState::Neutral,
            };

            (
                vec![
                    cell(phase),
                    cell(text(data, &["spec", "volumeName"])),
                    cell(text(data, &["status", "capacity", "storage"])),
                    cell(text(data, &["spec", "storageClassName"])),
                ],
                state,
            )
        }),
    }
}

fn event_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("TYPE", 100),
            ColumnSpec::fixed("REASON", 180),
            ColumnSpec::fixed("OBJECT", 240),
            ColumnSpec::flexible("MESSAGE"),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            let kind = text(data, &["type"]);
            let involved = format!(
                "{}/{}",
                text(data, &["involvedObject", "kind"]),
                text(data, &["involvedObject", "name"])
            );

            (
                vec![
                    cell(kind),
                    cell(text(data, &["reason"])),
                    cell(involved),
                    cell(text(data, &["message"])),
                ],
                if kind == "Warning" {
                    RowState::Failing
                } else {
                    RowState::Neutral
                },
            )
        }),
    }
}

fn namespace_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([ColumnSpec::fixed("STATUS", 140)]),
        project: Projection::BuiltIn(|object| {
            let phase = text(&object.data, &["status", "phase"]);
            let state = match phase {
                "Active" => RowState::Healthy,
                "Terminating" => RowState::Transient,
                _ => RowState::Neutral,
            };
            (vec![cell(phase)], state)
        }),
    }
}

/// The `Ready` condition's status, when the object carries one.
fn ready_condition(data: &Value) -> Option<&str> {
    data["status"]["conditions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .find(|condition| text(condition, &["type"]) == "Ready")
        .map(|condition| text(condition, &["status"]))
}

/// How a row reads when no projector knows what the kind's fields mean.
///
/// Two conventions are worth trying — `status.phase` and a `Ready` condition —
/// and anything else is [`RowState::Neutral`] rather than guessed at. A CRD's
/// own printer columns say what to *print*, never how healthy the object is, so
/// a row built from them takes its colour from here.
fn conventional_state(data: &Value) -> RowState {
    match (text(data, &["status", "phase"]), ready_condition(data)) {
        (_, Some("True")) => RowState::Healthy,
        (_, Some("False")) => RowState::Failing,
        ("Active" | "Bound" | "Running" | "Succeeded", _) => RowState::Healthy,
        ("Pending" | "Terminating", _) => RowState::Transient,
        ("Failed" | "Lost", _) => RowState::Failing,
        _ => RowState::Neutral,
    }
}

/// The fallback for kinds this module knows nothing about.
///
/// It shows what every object has — namespace, name, age — plus two fields that
/// are conventional enough to be worth trying: `status.phase`, and whether a
/// `Ready` condition is true. Both render empty when absent, which is honest.
///
/// This is what a CRD gets when it declares no printer columns of its own, and
/// what every CRD got before those were read.
fn generic_columns() -> KindColumns {
    KindColumns {
        columns: Arc::from([
            ColumnSpec::fixed("STATUS", 140),
            ColumnSpec::fixed("READY", 90),
        ]),
        project: Projection::BuiltIn(|object| {
            let data = &object.data;
            (
                vec![
                    cell(text(data, &["status", "phase"])),
                    cell(ready_condition(data).unwrap_or_default()),
                ],
                conventional_state(data),
            )
        }),
    }
}

/// The table a CRD's own printer columns describe.
///
/// `None` when the CRD declared none that survived [`crate::printer::compile`],
/// which is the caller's cue to fall back to [`for_kind`].
pub fn from_printer_columns(columns: Arc<[PrinterColumn]>) -> Option<KindColumns> {
    if columns.is_empty() {
        return None;
    }

    Some(KindColumns {
        columns: printer::specs(&columns),
        project: Projection::Declared(columns),
    })
}

/// The projector written for this kind, if this module has one.
fn built_in(kind: &KindId) -> Option<KindColumns> {
    Some(match (&*kind.group, &*kind.kind) {
        ("", "Pod") => pods_columns(),
        ("", "Service") => service_columns(),
        ("", "Node") => node_columns(),
        ("", "ConfigMap") => configmap_columns(),
        ("", "Secret") => secret_columns(),
        ("", "PersistentVolumeClaim") => pvc_columns(),
        ("", "Event") | ("events.k8s.io", "Event") => event_columns(),
        ("", "Namespace") => namespace_columns(),
        ("apps", "Deployment") => deployment_columns(),
        ("apps", "StatefulSet") => statefulset_columns(),
        ("apps", "ReplicaSet") => replicaset_columns(),
        ("apps", "DaemonSet") => daemonset_columns(),
        ("batch", "Job") => job_columns(),
        ("batch", "CronJob") => cronjob_columns(),
        ("networking.k8s.io", "Ingress") => ingress_columns(),
        _ => return None,
    })
}

/// The table definition for a kind, ignoring anything its CRD declared.
pub fn for_kind(kind: &KindId) -> KindColumns {
    built_in(kind).unwrap_or_else(generic_columns)
}

/// The table definition for a kind, given the printer columns its CRD declared.
///
/// A projector written for the kind wins over declared columns: that only
/// arises when an aggregated apiserver serves a kind this module also knows,
/// and a hand-written projection of a Pod is better than any CRD's. Discovery
/// hands over an empty slice for every kind that has no CRD, so the ordinary
/// case falls straight through to [`for_kind`]'s answer.
pub fn for_kind_with(kind: &KindId, printer_columns: Arc<[PrinterColumn]>) -> KindColumns {
    built_in(kind)
        .or_else(|| from_printer_columns(printer_columns))
        .unwrap_or_else(generic_columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("fixture is a valid object")
    }

    fn row_of(kind: &KindId, value: Value) -> ResourceRow {
        for_kind(kind).row(&object(value))
    }

    fn cells(row: &ResourceRow) -> Vec<String> {
        row.cells.iter().map(|cell| cell.to_string()).collect()
    }

    fn kind(group: &str, kind: &str, plural: &str) -> KindId {
        KindId::new(group, "v1", kind, plural)
    }

    #[test]
    fn a_pod_renders_the_columns_kubectl_prints() {
        let row = row_of(
            &kind("", "Pod", "pods"),
            json!({
                "metadata": { "name": "api-0", "namespace": "payments", "uid": "1f0d" },
                "spec": { "nodeName": "node-1", "containers": [{ "name": "api" }] },
                "status": {
                    "phase": "Running",
                    "conditions": [{ "type": "Ready", "status": "True" }],
                    "containerStatuses": [{
                        "name": "api", "ready": true, "restartCount": 2,
                        "state": { "running": {} }
                    }]
                }
            }),
        );

        assert_eq!(row.key, ResourceKey::new("payments", "api-0"));
        assert_eq!(row.uid.as_deref(), Some("1f0d"));
        assert_eq!(cells(&row), ["1/1", "Running", "2", "node-1"]);
        assert_eq!(row.state, RowState::Healthy);
    }

    #[test]
    fn a_pod_that_is_running_but_not_ready_is_not_healthy() {
        let row = row_of(
            &kind("", "Pod", "pods"),
            json!({
                "metadata": { "name": "api-0", "namespace": "default" },
                "spec": { "containers": [{ "name": "api" }, { "name": "sidecar" }] },
                "status": {
                    "phase": "Running",
                    "containerStatuses": [
                        { "name": "api", "ready": true, "restartCount": 0, "state": { "running": {} } },
                        { "name": "sidecar", "ready": false, "restartCount": 0, "state": { "running": {} } }
                    ]
                }
            }),
        );

        assert_eq!(cells(&row)[0], "1/2");
        assert_eq!(row.state, RowState::Transient);
    }

    #[test]
    fn a_deployment_shows_ready_up_to_date_and_available() {
        let row = row_of(
            &KindId::new("apps", "v1", "Deployment", "deployments"),
            json!({
                "metadata": { "name": "web", "namespace": "default" },
                "spec": { "replicas": 3 },
                "status": { "readyReplicas": 2, "updatedReplicas": 3, "availableReplicas": 2 }
            }),
        );

        assert_eq!(cells(&row), ["2/3", "3", "2"]);
        assert_eq!(row.state, RowState::Transient);
    }

    #[test]
    fn a_fully_rolled_out_deployment_is_healthy() {
        let row = row_of(
            &KindId::new("apps", "v1", "Deployment", "deployments"),
            json!({
                "metadata": { "name": "web", "namespace": "default" },
                "spec": { "replicas": 3 },
                "status": { "readyReplicas": 3, "updatedReplicas": 3, "availableReplicas": 3 }
            }),
        );
        assert_eq!(row.state, RowState::Healthy);
    }

    #[test]
    fn a_scaled_to_zero_deployment_is_neither_healthy_nor_broken() {
        let row = row_of(
            &KindId::new("apps", "v1", "Deployment", "deployments"),
            json!({
                "metadata": { "name": "web", "namespace": "default" },
                "spec": { "replicas": 0 },
                "status": {}
            }),
        );
        assert_eq!(cells(&row)[0], "0/0");
        assert_eq!(row.state, RowState::Neutral);
    }

    #[test]
    fn a_service_renders_ports_the_way_kubectl_does() {
        let row = row_of(
            &kind("", "Service", "services"),
            json!({
                "metadata": { "name": "web", "namespace": "default" },
                "spec": {
                    "type": "NodePort",
                    "clusterIP": "10.0.0.1",
                    "ports": [
                        { "port": 80, "nodePort": 30080, "protocol": "TCP" },
                        { "port": 443 }
                    ]
                }
            }),
        );

        assert_eq!(
            cells(&row),
            ["NodePort", "10.0.0.1", "<none>", "80:30080/TCP,443/TCP"]
        );
    }

    #[test]
    fn a_load_balancer_without_an_address_is_pending() {
        let row = row_of(
            &kind("", "Service", "services"),
            json!({
                "metadata": { "name": "web", "namespace": "default" },
                "spec": { "type": "LoadBalancer", "clusterIP": "10.0.0.1", "ports": [] }
            }),
        );

        assert_eq!(cells(&row)[2], "<pending>");
        assert_eq!(row.state, RowState::Transient);
    }

    #[test]
    fn a_node_shows_its_roles_status_and_version() {
        let row = row_of(
            &kind("", "Node", "nodes"),
            json!({
                "metadata": {
                    "name": "cp-0",
                    "labels": { "node-role.kubernetes.io/control-plane": "" }
                },
                "spec": {},
                "status": {
                    "conditions": [{ "type": "Ready", "status": "True" }],
                    "nodeInfo": { "kubeletVersion": "v1.36.1" }
                }
            }),
        );

        assert_eq!(cells(&row), ["Ready", "control-plane", "v1.36.1"]);
        assert_eq!(row.state, RowState::Healthy);
        // Cluster-scoped: no namespace.
        assert!(!row.key.is_namespaced());
    }

    #[test]
    fn a_cordoned_node_says_scheduling_is_disabled() {
        let row = row_of(
            &kind("", "Node", "nodes"),
            json!({
                "metadata": { "name": "worker-0" },
                "spec": { "unschedulable": true },
                "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
            }),
        );

        assert_eq!(cells(&row)[0], "Ready,SchedulingDisabled");
        assert_eq!(row.state, RowState::Transient);
    }

    #[test]
    fn a_not_ready_node_is_a_failure() {
        let row = row_of(
            &kind("", "Node", "nodes"),
            json!({
                "metadata": { "name": "worker-0" },
                "spec": {},
                "status": { "conditions": [{ "type": "Ready", "status": "False" }] }
            }),
        );
        assert_eq!(row.state, RowState::Failing);
    }

    #[test]
    fn a_job_counts_completions() {
        let complete = row_of(
            &KindId::new("batch", "v1", "Job", "jobs"),
            json!({
                "metadata": { "name": "migrate", "namespace": "default" },
                "spec": { "completions": 3 },
                "status": { "succeeded": 3 }
            }),
        );
        assert_eq!(cells(&complete), ["3/3", "Complete"]);
        assert_eq!(complete.state, RowState::Healthy);

        let failed = row_of(
            &KindId::new("batch", "v1", "Job", "jobs"),
            json!({
                "metadata": { "name": "migrate", "namespace": "default" },
                "spec": {},
                "status": { "failed": 1 }
            }),
        );
        assert_eq!(cells(&failed), ["0/1", "Failed"]);
        assert_eq!(failed.state, RowState::Failing);
    }

    #[test]
    fn a_secret_shows_how_many_keys_it_has_and_never_a_value() {
        let row = row_of(
            &kind("", "Secret", "secrets"),
            json!({
                "metadata": { "name": "db", "namespace": "default" },
                "type": "Opaque",
                "data": { "password": "aHVudGVyMg==", "user": "cm9vdA==" }
            }),
        );

        assert_eq!(cells(&row), ["Opaque", "2"]);
        // The masking rule starts here: no projected cell may carry a value.
        assert!(!cells(&row).iter().any(|cell| cell.contains("aHVudGVy")));
    }

    #[test]
    fn a_warning_event_is_a_failure_row() {
        let row = row_of(
            &kind("", "Event", "events"),
            json!({
                "metadata": { "name": "api-0.17f", "namespace": "default" },
                "type": "Warning",
                "reason": "BackOff",
                "message": "Back-off restarting failed container",
                "involvedObject": { "kind": "Pod", "name": "api-0" }
            }),
        );

        assert_eq!(
            cells(&row),
            [
                "Warning",
                "BackOff",
                "Pod/api-0",
                "Back-off restarting failed container"
            ]
        );
        assert_eq!(row.state, RowState::Failing);
    }

    #[test]
    fn a_pvc_reports_its_binding() {
        let row = row_of(
            &kind("", "PersistentVolumeClaim", "persistentvolumeclaims"),
            json!({
                "metadata": { "name": "data", "namespace": "default" },
                "spec": { "volumeName": "pvc-123", "storageClassName": "standard" },
                "status": { "phase": "Bound", "capacity": { "storage": "10Gi" } }
            }),
        );

        assert_eq!(cells(&row), ["Bound", "pvc-123", "10Gi", "standard"]);
        assert_eq!(row.state, RowState::Healthy);
    }

    #[test]
    fn an_unknown_kind_still_renders_something_useful() {
        // The acceptance criterion: a CRD nobody has heard of lists without
        // special-casing.
        let row = row_of(
            &KindId::new("argoproj.io", "v1alpha1", "Application", "applications"),
            json!({
                "metadata": { "name": "web", "namespace": "argocd" },
                "spec": {},
                "status": { "conditions": [{ "type": "Ready", "status": "True" }] }
            }),
        );

        assert_eq!(cells(&row), ["", "True"]);
        assert_eq!(row.state, RowState::Healthy);
    }

    /// Argo CD's Application columns, as its CRD declares them: two ordinary
    /// ones and two the CRD marks as only worth showing in a wide listing.
    fn application_columns() -> Arc<[PrinterColumn]> {
        let column = |name: &str, json_path: &str, priority| {
            k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceColumnDefinition {
                name: name.to_owned(),
                type_: "string".to_owned(),
                json_path: json_path.to_owned(),
                priority,
                ..Default::default()
            }
        };

        printer::compile(
            &applications(),
            &[
                column("Sync Status", ".status.sync.status", None),
                column("Health Status", ".status.health.status", None),
                column("Revision", ".status.sync.revision", Some(10)),
                column("Project", ".spec.project", Some(10)),
            ],
        )
    }

    fn applications() -> KindId {
        KindId::new("argoproj.io", "v1alpha1", "Application", "applications")
    }

    #[test]
    fn a_crd_renders_the_columns_its_own_definition_declared() {
        let table = for_kind_with(&applications(), application_columns());
        let row = table.row(&object(json!({
            "metadata": { "name": "web", "namespace": "argocd" },
            "spec": { "project": "default" },
            "status": {
                "sync": { "status": "Synced", "revision": "0f9a2c1" },
                "health": { "status": "Healthy" }
            }
        })));

        let headings: Vec<String> = table.columns.iter().map(|c| c.name.to_string()).collect();
        assert_eq!(headings, ["SYNC STATUS", "HEALTH STATUS"]);
        assert_eq!(cells(&row), ["Synced", "Healthy"]);
    }

    #[test]
    fn a_crd_that_declares_nothing_still_gets_the_generic_columns() {
        // The fallback is not gone, it is just no longer everybody's answer.
        let table = for_kind_with(&applications(), Arc::from([]));
        let headings: Vec<String> = table.columns.iter().map(|c| c.name.to_string()).collect();

        assert_eq!(headings, ["STATUS", "READY"]);
    }

    #[test]
    fn a_kind_with_a_projector_of_its_own_ignores_declared_columns() {
        // Only reachable through an aggregated apiserver serving a kind this
        // module also knows; a hand-written projection of a Pod beats any
        // declaration.
        let table = for_kind_with(&kind("", "Pod", "pods"), application_columns());
        let headings: Vec<String> = table.columns.iter().map(|c| c.name.to_string()).collect();

        assert_eq!(headings, ["READY", "STATUS", "RESTARTS", "NODE"]);
    }

    #[test]
    fn a_row_built_from_declared_columns_still_reads_as_healthy_or_not() {
        // Printer columns say what to print, never how the object is doing.
        let table = for_kind_with(&applications(), application_columns());
        let failing = table.row(&object(json!({
            "metadata": { "name": "web", "namespace": "argocd" },
            "status": { "conditions": [{ "type": "Ready", "status": "False" }] }
        })));

        assert_eq!(failing.state, RowState::Failing);
    }

    #[test]
    fn an_unknown_kind_with_nothing_conventional_renders_empty_cells() {
        let row = row_of(
            &KindId::new("example.com", "v1", "Widget", "widgets"),
            json!({
                "metadata": { "name": "sprocket", "namespace": "default" },
                "spec": { "size": 3 }
            }),
        );

        assert_eq!(cells(&row), ["", ""]);
        assert_eq!(row.state, RowState::Neutral);
    }

    #[test]
    fn every_projector_produces_one_cell_per_column() {
        // A mismatch would silently shift every column in the table.
        let kinds = [
            kind("", "Pod", "pods"),
            kind("", "Service", "services"),
            kind("", "Node", "nodes"),
            kind("", "ConfigMap", "configmaps"),
            kind("", "Secret", "secrets"),
            kind("", "PersistentVolumeClaim", "persistentvolumeclaims"),
            kind("", "Event", "events"),
            kind("", "Namespace", "namespaces"),
            KindId::new("apps", "v1", "Deployment", "deployments"),
            KindId::new("apps", "v1", "StatefulSet", "statefulsets"),
            KindId::new("apps", "v1", "ReplicaSet", "replicasets"),
            KindId::new("apps", "v1", "DaemonSet", "daemonsets"),
            KindId::new("batch", "v1", "Job", "jobs"),
            KindId::new("batch", "v1", "CronJob", "cronjobs"),
            KindId::new("networking.k8s.io", "v1", "Ingress", "ingresses"),
            KindId::new("example.com", "v1", "Widget", "widgets"),
        ];

        let empty = json!({ "metadata": { "name": "x", "namespace": "default" } });
        for kind in kinds {
            let table = for_kind(&kind);
            let row = table.row(&object(empty.clone()));
            assert_eq!(
                row.cells.len(),
                table.columns.len(),
                "{kind} projects {} cells for {} columns",
                row.cells.len(),
                table.columns.len()
            );
        }

        // And a CRD's declared columns, against an object with none of the
        // fields they name.
        let declared = for_kind_with(&applications(), application_columns());
        let row = declared.row(&object(empty));
        assert_eq!(row.cells.len(), declared.columns.len());
    }
}
