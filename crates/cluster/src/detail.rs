//! Fetching one object in full, for the detail view.
//!
//! Objects are fetched on demand rather than cached: the tables hold projected
//! rows, and holding every object's full body as well would multiply memory by
//! the size of the largest ConfigMap in the cluster. One `get` when the user
//! opens something is cheap, and it is also *fresher* than anything cached.
//!
//! See `docs/DECISIONS.md` ADR-0018.

use std::sync::Arc;

use kube::api::{Api, DynamicObject, ListParams};
use kube::{Client, ResourceExt as _};
use periscope_bridge::{EventLine, KindId, ObjectDetail, OwnerRef, ResourceKey};
use serde_json::Value;

use crate::watch::api_for;

/// Keys under `metadata` that say nothing to a human reading YAML.
const NOISY_METADATA: [&str; 3] = ["managedFields", "generation", "resourceVersion"];

/// Annotation kubectl writes with an entire copy of the object in it.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Fetches an object, its events and its owners.
pub async fn fetch(
    client: Client,
    kind: &KindId,
    namespaced: bool,
    key: &ResourceKey,
) -> Result<ObjectDetail, kube::Error> {
    let api = api_for(
        client.clone(),
        kind,
        namespaced,
        key.is_namespaced().then(|| &*key.namespace),
    );
    let object = api.get(&key.name).await?;

    let owners = owners_of(&object);
    let events = match events_for(client, &object, key).await {
        Ok(events) => events,
        Err(error) => {
            // The object is the point; events are context. A cluster that
            // refuses to list events must not blank the whole detail view.
            tracing::warn!(%kind, %key, %error, "could not list events");
            Vec::new()
        }
    };

    Ok(ObjectDetail {
        key: key.clone(),
        yaml: Arc::from(to_yaml(&object)?.as_str()),
        events: Arc::from(events),
        owners: Arc::from(owners),
    })
}

/// Renders an object as YAML, with the noise removed.
///
/// `managedFields` is usually longer than the object itself and is never what
/// the reader wants; `last-applied-configuration` is a second copy of the
/// object hidden in an annotation.
fn to_yaml(object: &DynamicObject) -> Result<String, kube::Error> {
    let mut value = serde_json::to_value(object).map_err(kube::Error::SerdeError)?;

    if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        for key in NOISY_METADATA {
            metadata.remove(key);
        }
        if let Some(annotations) = metadata
            .get_mut("annotations")
            .and_then(Value::as_object_mut)
        {
            annotations.remove(LAST_APPLIED);
            if annotations.is_empty() {
                metadata.remove("annotations");
            }
        }
    }

    Ok(yaml::render(&value))
}

/// Owner references, in the order the object lists them.
fn owners_of(object: &DynamicObject) -> Vec<OwnerRef> {
    object
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|owner| OwnerRef {
            api_version: Arc::from(owner.api_version.as_str()),
            kind: Arc::from(owner.kind.as_str()),
            name: Arc::from(owner.name.as_str()),
            controller: owner.controller.unwrap_or(false),
        })
        .collect()
}

/// Events referring to this object, oldest first.
async fn events_for(
    client: Client,
    object: &DynamicObject,
    key: &ResourceKey,
) -> Result<Vec<EventLine>, kube::Error> {
    let Some(uid) = object.uid() else {
        return Ok(Vec::new());
    };

    let events: Api<DynamicObject> = api_for(
        client,
        &KindId::new("", "v1", "Event", "events"),
        true,
        key.is_namespaced().then(|| &*key.namespace),
    );

    // Field selectors are what the apiserver indexes events by; filtering
    // client-side would mean listing every event in the namespace.
    let params = ListParams::default().fields(&format!("involvedObject.uid={uid}"));
    let list = events.list(&params).await?;

    let mut lines: Vec<EventLine> = list.items.iter().map(event_line).collect();
    lines.sort_by_key(|line| line.last_seen);
    Ok(lines)
}

fn event_line(event: &DynamicObject) -> EventLine {
    let text =
        |path: &str| -> Arc<str> { Arc::from(event.data[path].as_str().unwrap_or_default()) };

    EventLine {
        kind: text("type"),
        reason: text("reason"),
        message: text("message"),
        last_seen: last_seen(event),
        count: event.data["count"].as_u64().unwrap_or(1) as u32,
    }
}

/// When an event last happened, preferring the fields that are actually set.
///
/// `lastTimestamp` is empty on events written through the `events.k8s.io` API,
/// which falls back to `eventTime`, then to the object's creation.
fn last_seen(event: &DynamicObject) -> Option<std::time::SystemTime> {
    for field in ["lastTimestamp", "eventTime", "firstTimestamp"] {
        if let Some(text) = event.data[field].as_str()
            && let Ok(timestamp) = text.parse::<k8s_openapi::jiff::Timestamp>()
        {
            return Some(to_system_time(timestamp));
        }
    }

    event
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|time| to_system_time(time.0))
}

fn to_system_time(timestamp: k8s_openapi::jiff::Timestamp) -> std::time::SystemTime {
    let seconds = timestamp.as_second();
    if seconds >= 0 {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64)
    } else {
        std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(seconds.unsigned_abs())
    }
}

/// A small YAML writer.
///
/// Kubernetes objects are JSON-shaped — maps, arrays, strings, numbers, bools,
/// null — so the general problem does not arise, and this avoids a dependency
/// whose output we would have to post-process anyway. It quotes anything that
/// could be misread as another type, which is the only part that is subtle.
mod yaml {
    use serde_json::Value;

    /// Renders a value as YAML.
    pub fn render(value: &Value) -> String {
        let mut out = String::new();
        write(value, 0, &mut out);
        out
    }

    fn write(value: &Value, indent: usize, out: &mut String) {
        match value {
            Value::Object(map) if map.is_empty() => out.push_str("{}\n"),
            Value::Object(map) => {
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 || indent > 0 {
                        pad(indent, out);
                    }
                    out.push_str(&scalar(&Value::String(key.clone())));
                    out.push(':');
                    write_child(value, indent, out);
                }
            }
            Value::Array(items) if items.is_empty() => out.push_str("[]\n"),
            Value::Array(items) => {
                for item in items {
                    pad(indent, out);
                    out.push_str("- ");
                    match item {
                        Value::Object(_) | Value::Array(_) => {
                            // Nested collections continue on the next line at a
                            // deeper indent; the dash already provided two
                            // columns of it.
                            let mut nested = String::new();
                            write(item, indent + 1, &mut nested);
                            out.push_str(nested.trim_start());
                        }
                        scalar_value => {
                            out.push_str(&scalar(scalar_value));
                            out.push('\n');
                        }
                    }
                }
            }
            other => {
                out.push_str(&scalar(other));
                out.push('\n');
            }
        }
    }

    /// Writes the value of a mapping key, on the same line or the next.
    fn write_child(value: &Value, indent: usize, out: &mut String) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                out.push('\n');
                write(value, indent + 1, out);
            }
            Value::Array(items) if !items.is_empty() => {
                out.push('\n');
                // Sequence items sit at the *same* indent as their key, which
                // is what `kubectl -o yaml` prints; the alternative is valid
                // YAML that no longer diffs against what users are used to.
                write(value, indent, out);
            }
            _ => {
                out.push(' ');
                write(value, 0, out);
            }
        }
    }

    fn pad(indent: usize, out: &mut String) {
        out.push_str(&"  ".repeat(indent));
    }

    /// Renders a scalar, quoting when leaving it bare would change its meaning.
    fn scalar(value: &Value) -> String {
        match value {
            Value::Null => "null".to_owned(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number(number) => number.to_string(),
            Value::String(text) => string(text),
            _ => unreachable!("collections are written by `write`"),
        }
    }

    fn string(text: &str) -> String {
        if text.contains('\n') {
            // Block scalars keep certificates and scripts readable.
            let body: String = text
                .lines()
                .map(|line| format!("\n  {line}"))
                .collect::<Vec<_>>()
                .join("");
            return format!("|-{body}");
        }

        let needs_quotes = text.is_empty()
            || text.parse::<f64>().is_ok()
            || matches!(
                text.to_ascii_lowercase().as_str(),
                "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
            )
            || text.starts_with([' ', '"', '\'', '&', '*', '!', '|', '>', '%', '@', '`', '#'])
            || text.ends_with(' ')
            || text.contains(": ")
            || text.contains(" #");

        if needs_quotes {
            format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            text.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> DynamicObject {
        serde_json::from_value(value).expect("fixture is a valid object")
    }

    #[test]
    fn yaml_renders_nested_maps_and_lists() {
        let rendered = yaml::render(&json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "api-0", "labels": { "app": "api" } },
            "spec": { "containers": [{ "name": "api", "ports": [{ "containerPort": 8080 }] }] }
        }));

        assert_eq!(
            rendered,
            "apiVersion: v1\n\
             kind: Pod\n\
             metadata:\n  \
               name: api-0\n  \
               labels:\n    \
                 app: api\n\
             spec:\n  \
               containers:\n  \
               - name: api\n    \
                 ports:\n    \
                 - containerPort: 8080\n"
        );
    }

    #[test]
    fn strings_that_look_like_other_types_are_quoted() {
        // Unquoted, these would round-trip as a number, a bool and nothing.
        let rendered = yaml::render(&json!({
            "version": "1.20",
            "enabled": "true",
            "empty": "",
            "port": 8080,
            "flag": true,
            "nothing": null
        }));

        assert!(rendered.contains("version: \"1.20\""), "{rendered}");
        assert!(rendered.contains("enabled: \"true\""), "{rendered}");
        assert!(rendered.contains("empty: \"\""), "{rendered}");
        assert!(rendered.contains("port: 8080"), "{rendered}");
        assert!(rendered.contains("flag: true"), "{rendered}");
        assert!(rendered.contains("nothing: null"), "{rendered}");
    }

    #[test]
    fn multi_line_strings_become_block_scalars() {
        let rendered = yaml::render(&json!({ "ca.crt": "-----BEGIN\nMIIC\n-----END" }));
        assert_eq!(rendered, "ca.crt: |-\n  -----BEGIN\n  MIIC\n  -----END\n");
    }

    #[test]
    fn empty_collections_render_inline() {
        let rendered = yaml::render(&json!({ "labels": {}, "args": [] }));
        assert_eq!(rendered, "labels: {}\nargs: []\n");
    }

    #[test]
    fn the_yaml_view_drops_managed_fields_and_the_last_applied_annotation() {
        let object = object(json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "settings",
                "namespace": "default",
                "resourceVersion": "12345",
                "generation": 3,
                "managedFields": [{ "manager": "kubectl", "operation": "Apply" }],
                "annotations": {
                    "kubectl.kubernetes.io/last-applied-configuration": "{\"a\":1}",
                    "team": "payments"
                }
            },
            "data": { "key": "value" }
        }));

        let rendered = to_yaml(&object).expect("renders");
        assert!(!rendered.contains("managedFields"), "{rendered}");
        assert!(!rendered.contains("last-applied"), "{rendered}");
        assert!(!rendered.contains("resourceVersion"), "{rendered}");
        // Everything a reader actually wants survives.
        assert!(rendered.contains("name: settings"), "{rendered}");
        assert!(rendered.contains("team: payments"), "{rendered}");
        assert!(rendered.contains("key: value"), "{rendered}");
    }

    #[test]
    fn an_annotations_block_that_becomes_empty_is_removed_entirely() {
        let object = object(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-0",
                "annotations": { "kubectl.kubernetes.io/last-applied-configuration": "{}" }
            }
        }));

        let rendered = to_yaml(&object).expect("renders");
        assert!(!rendered.contains("annotations"), "{rendered}");
    }

    #[test]
    fn owner_references_are_carried_with_the_controller_flag() {
        let object = object(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-0",
                "ownerReferences": [
                    { "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api-7d4", "uid": "1", "controller": true },
                    { "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "uid": "2" }
                ]
            }
        }));

        let owners = owners_of(&object);
        assert_eq!(owners.len(), 2);
        assert_eq!(&*owners[0].kind, "ReplicaSet");
        assert!(owners[0].controller);
        assert!(!owners[1].controller);
    }

    #[test]
    fn an_object_with_no_owners_has_none() {
        let object =
            object(json!({ "apiVersion": "v1", "kind": "Pod", "metadata": { "name": "x" } }));
        assert!(owners_of(&object).is_empty());
    }

    #[test]
    fn an_event_line_carries_what_the_table_shows() {
        let event = object(json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": { "name": "api-0.17f", "namespace": "default" },
            "type": "Warning",
            "reason": "BackOff",
            "message": "Back-off restarting failed container",
            "count": 12,
            "lastTimestamp": "2026-08-17T10:00:00Z"
        }));

        let line = event_line(&event);
        assert!(line.is_warning());
        assert_eq!(&*line.reason, "BackOff");
        assert_eq!(line.count, 12);
        assert_eq!(
            line.last_seen,
            Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_786_960_800))
        );
    }

    #[test]
    fn an_event_without_a_last_timestamp_falls_back_to_event_time() {
        let event = object(json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": { "name": "api-0.17f", "namespace": "default" },
            "type": "Normal",
            "eventTime": "2026-08-17T10:00:00Z"
        }));

        assert!(event_line(&event).last_seen.is_some());
        // Events written through events.k8s.io have no count field at all.
        assert_eq!(event_line(&event).count, 1);
    }
}
