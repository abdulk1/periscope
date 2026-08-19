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

/// What a masked secret value is replaced with.
///
/// The length is kept: "this is a 40-character token" is useful, and it is not
/// the secret. `base64` says which encoding the value arrived in, because the
/// two fields a Secret carries do not agree on one.
fn masked(value: &str, base64: bool) -> String {
    // Report the decoded size, which is what the user would have got: four
    // base64 characters carry three bytes, less one for every byte of padding.
    // `stringData` is the plain text itself and needs none of that.
    let bytes = if base64 {
        let padding = value.chars().rev().take_while(|byte| *byte == '=').count();
        (value.len() * 3 / 4).saturating_sub(padding)
    } else {
        value.len()
    };
    format!("<hidden, {bytes} bytes>")
}

/// Whether a kind's values are hidden unless the user asks for them.
fn is_maskable(kind: &KindId) -> bool {
    kind.is_core() && &*kind.kind == "Secret"
}

/// Fetches an object, its events and its owners.
///
/// `reveal` only does anything for kinds that mask: a Secret's values are
/// replaced with their size unless it is set.
pub async fn fetch(
    client: Client,
    kind: &KindId,
    namespaced: bool,
    key: &ResourceKey,
    reveal: bool,
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

    let maskable = is_maskable(kind);
    Ok(ObjectDetail {
        key: key.clone(),
        yaml: Arc::from(to_yaml(&object, maskable && !reveal)?.as_str()),
        maskable,
        revealed: !maskable || reveal,
        events: Arc::from(events),
        owners: Arc::from(owners),
    })
}

/// Renders an object as YAML, with the noise removed.
///
/// `managedFields` is usually longer than the object itself and is never what
/// the reader wants; `last-applied-configuration` is a second copy of the
/// object hidden in an annotation.
///
/// `mask` replaces the values under `data` and `stringData` with their sizes,
/// and is what the detail view passes for an unrevealed Secret. Callers that
/// are showing an object the user just asked to see in full — the dry-run
/// preview — pass `false`.
pub fn to_yaml(object: &DynamicObject, mask: bool) -> Result<String, kube::Error> {
    let mut value = serde_json::to_value(object).map_err(kube::Error::SerdeError)?;

    if mask {
        // A Secret's YAML is where its values would otherwise be printed in
        // full, which is exactly what "masked by default" has to prevent.
        for (field, base64) in [("data", true), ("stringData", false)] {
            if let Some(data) = value.get_mut(field).and_then(Value::as_object_mut) {
                for entry in data.values_mut() {
                    let hidden = masked(entry.as_str().unwrap_or_default(), base64);
                    *entry = Value::String(hidden);
                }
            }
        }
    }

    if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        // `shift_remove`, not `remove`: with `preserve_order` the latter fills
        // the hole with whichever key happened to be last, so dropping the
        // noise would shuffle everything the reader came to look at.
        for key in NOISY_METADATA {
            metadata.shift_remove(key);
        }
        if let Some(annotations) = metadata
            .get_mut("annotations")
            .and_then(Value::as_object_mut)
        {
            annotations.shift_remove(LAST_APPLIED);
            if annotations.is_empty() {
                metadata.shift_remove("annotations");
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
/// whose output we would have to post-process anyway. Two parts are subtle:
/// quoting anything that could be misread as another type, and giving a block
/// scalar a margin its own content cannot climb out of. `tests/golden/` pins
/// both against real objects.
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
                    // A key is always one line, however the value is written.
                    out.push_str(&plain_or_quoted(key));
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
                            out.push_str(&scalar(scalar_value, indent + 1));
                            out.push('\n');
                        }
                    }
                }
            }
            other => {
                out.push_str(&scalar(other, indent));
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
                // Empty collections and scalars finish the key's own line, but
                // a block scalar's body still runs on below it — one level in
                // from the key, which is why the depth is carried through
                // rather than reset.
                write(value, indent + 1, out);
            }
        }
    }

    fn pad(indent: usize, out: &mut String) {
        out.push_str(&"  ".repeat(indent));
    }

    /// Renders a scalar, quoting when leaving it bare would change its meaning.
    ///
    /// `indent` is the depth the value's *body* would sit at, which only a
    /// block scalar spends.
    fn scalar(value: &Value, indent: usize) -> String {
        match value {
            Value::Null => "null".to_owned(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number(number) => number.to_string(),
            Value::String(text) => block(text, indent).unwrap_or_else(|| plain_or_quoted(text)),
            _ => unreachable!("collections are written by `write`"),
        }
    }

    /// A literal block scalar, when the text can survive being written as one.
    ///
    /// Block scalars keep certificates and scripts readable, but YAML infers
    /// their margin from the first non-empty line: text whose first line is
    /// empty or itself indented would be read back with the wrong one. Nor can
    /// a block hold a carriage return or any other control character, since
    /// the reader would take it for a line break or reject it outright. Those
    /// go back as a quoted scalar, which is uglier and always correct.
    fn block(text: &str, indent: usize) -> Option<String> {
        // At the top of a document there is no margin to indent into.
        if indent == 0 || !text.contains('\n') {
            return None;
        }
        if text.starts_with(['\n', ' ', '\t'])
            || text
                .chars()
                .any(|character| character != '\n' && character.is_control())
        {
            return None;
        }

        // How the reader is told to treat the newlines the text ends with:
        // strip them, keep the one that terminates the last line, or keep the
        // lot. Without this a value ending in a newline would lose it.
        let chomp = match text.strip_suffix('\n') {
            None => "-",
            Some(rest) if rest.ends_with('\n') => "+",
            Some(_) => "",
        };

        // The newline that ends the final line is written by the caller, so it
        // is not one of the lines here.
        let mut rendered = format!("|{chomp}");
        for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
            rendered.push('\n');
            if !line.is_empty() {
                pad(indent, &mut rendered);
                rendered.push_str(line);
            }
        }
        Some(rendered)
    }

    /// A one-line scalar, quoted when leaving it bare would change its meaning.
    fn plain_or_quoted(text: &str) -> String {
        if needs_quotes(text) {
            quoted(text)
        } else {
            text.to_owned()
        }
    }

    /// Whether a plain scalar would be read back as something other than this
    /// exact string.
    fn needs_quotes(text: &str) -> bool {
        text.is_empty()
            || text.parse::<f64>().is_ok()
            || matches!(
                text.to_ascii_lowercase().as_str(),
                "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
            )
            // A leading indicator makes the reader expect a collection, an
            // alias, a tag or a comment instead of a string.
            || text.starts_with([
                ' ', '\t', '"', '\'', '&', '*', '!', '|', '>', '%', '@', '`', '#', '{', '}', '[',
                ']', ',',
            ])
            // `-`, `?` and `:` only introduce something else when a space
            // follows, or when they are the whole scalar.
            || matches!(text, "-" | "?" | ":")
            || text.starts_with("- ")
            || text.starts_with("? ")
            || text.starts_with(": ")
            || text.contains(": ")
            // A trailing colon opens a mapping; trailing space is eaten.
            || text.ends_with(':')
            || text.ends_with([' ', '\t'])
            || text.contains(" #")
            || text.chars().any(char::is_control)
    }

    /// A double-quoted scalar, with the escapes YAML defines for it.
    fn quoted(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for character in text.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                // Everything else `char::is_control` matches is below U+00A0,
                // so two hex digits always suffice.
                other if other.is_control() => out.push_str(&format!("\\x{:02x}", other as u32)),
                other => out.push(other),
            }
        }
        out.push('"');
        out
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
    fn a_block_scalar_is_indented_below_the_key_it_belongs_to() {
        // Written at a fixed margin, a ConfigMap's script would sit level with
        // its own key and the document would no longer parse at all.
        let rendered = yaml::render(&json!({ "data": { "run.sh": "set -e\necho hi" } }));
        assert_eq!(rendered, "data:\n  run.sh: |-\n    set -e\n    echo hi\n");
        assert_eq!(
            parsed(&rendered)["data"]["run.sh"],
            json!("set -e\necho hi")
        );
    }

    #[test]
    fn a_block_scalar_inside_a_list_clears_the_dash() {
        let rendered = yaml::render(&json!({ "command": ["set -e\necho hi"] }));
        assert_eq!(rendered, "command:\n- |-\n  set -e\n  echo hi\n");
        assert_eq!(parsed(&rendered)["command"][0], json!("set -e\necho hi"));
    }

    #[test]
    fn a_value_that_ends_in_a_newline_keeps_it() {
        // `|-` would silently drop it, so a file read out of a ConfigMap would
        // come back one byte shorter than it went in.
        let one = yaml::render(&json!({ "data": { "a": "hello\n" } }));
        assert_eq!(one, "data:\n  a: |\n    hello\n");
        assert_eq!(parsed(&one)["data"]["a"], json!("hello\n"));

        let several = yaml::render(&json!({ "data": { "a": "hello\n\n" } }));
        assert_eq!(several, "data:\n  a: |+\n    hello\n\n");
        assert_eq!(parsed(&several)["data"]["a"], json!("hello\n\n"));
    }

    #[test]
    fn text_no_block_scalar_can_hold_is_quoted_and_escaped() {
        // A block infers its margin from the first line, so one that is itself
        // indented cannot be written as a block at all; nor can a carriage
        // return, which the reader would take for a line break.
        let indented = yaml::render(&json!({ "data": { "a": "  first\nsecond" } }));
        assert_eq!(indented, "data:\n  a: \"  first\\nsecond\"\n");
        assert_eq!(parsed(&indented)["data"]["a"], json!("  first\nsecond"));

        let carriage = yaml::render(&json!({ "data": { "a": "one\r\ntwo" } }));
        assert_eq!(parsed(&carriage)["data"]["a"], json!("one\r\ntwo"));
    }

    #[test]
    fn strings_that_would_open_something_else_are_quoted() {
        let rendered = yaml::render(&json!({
            "trailing": "note:",
            "flow": "[1, 2]",
            "dash": "- item",
            "tab": "\tindented",
            "alone": "-"
        }));

        assert!(rendered.contains("trailing: \"note:\""), "{rendered}");
        assert!(rendered.contains("flow: \"[1, 2]\""), "{rendered}");
        assert!(rendered.contains("dash: \"- item\""), "{rendered}");
        assert!(rendered.contains("tab: \"\\tindented\""), "{rendered}");
        assert!(rendered.contains("alone: \"-\""), "{rendered}");
        assert_eq!(parsed(&rendered)["flow"], json!("[1, 2]"));
        assert_eq!(parsed(&rendered)["dash"], json!("- item"));
    }

    #[test]
    fn a_masked_secret_reports_the_size_the_user_would_have_got() {
        // `hunter2` is seven bytes; its base64 is twelve characters, two of
        // them padding. `stringData` is not base64 at all.
        assert_eq!(masked("aHVudGVyMg==", true), "<hidden, 7 bytes>");
        assert_eq!(masked("hunter2", false), "<hidden, 7 bytes>");
    }

    #[test]
    fn dropping_the_noisy_metadata_leaves_the_rest_in_order() {
        // `serde_json::Map::remove` fills the hole with the last entry, which
        // would shuffle the annotations every time one was dropped.
        let object = object(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "api-0",
                "annotations": {
                    "a": "1",
                    "kubectl.kubernetes.io/last-applied-configuration": "{}",
                    "y": "2",
                    "z": "3"
                }
            }
        }));

        let rendered = to_yaml(&object, false).expect("renders");
        let annotations = rendered
            .lines()
            .filter_map(|line| line.trim().split(':').next())
            .filter(|key| matches!(*key, "a" | "y" | "z"))
            .collect::<Vec<_>>();
        assert_eq!(annotations, ["a", "y", "z"]);
    }

    /// Reads a rendering back, which is the only proof it was ever YAML.
    fn parsed(rendered: &str) -> Value {
        crate::yaml::parse(rendered).unwrap_or_else(|error| panic!("{error}\n{rendered}"))
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

        let rendered = to_yaml(&object, false).expect("renders");
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

        let rendered = to_yaml(&object, false).expect("renders");
        assert!(!rendered.contains("annotations"), "{rendered}");
    }

    #[test]
    fn a_secrets_values_are_hidden_unless_revealed() {
        let secret = object(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "db", "namespace": "default" },
            "type": "Opaque",
            "data": { "password": "aHVudGVyMg==", "token": "c2hoaGho" }
        }));

        let masked = to_yaml(&secret, true).expect("renders");
        assert!(!masked.contains("aHVudGVyMg"), "{masked}");
        assert!(!masked.contains("c2hoaGho"), "{masked}");
        // The keys and the sizes stay: knowing a secret has a `password` key is
        // the point of opening it.
        assert!(masked.contains("password:"), "{masked}");
        assert!(masked.contains("bytes"), "{masked}");

        let revealed = to_yaml(&secret, false).expect("renders");
        assert!(revealed.contains("aHVudGVyMg"), "{revealed}");
    }

    #[test]
    fn string_data_is_masked_too() {
        let secret = object(json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "db", "namespace": "default" },
            "stringData": { "password": "hunter2" }
        }));

        let masked = to_yaml(&secret, true).expect("renders");
        assert!(!masked.contains("hunter2"), "{masked}");
    }

    #[test]
    fn only_secrets_mask() {
        assert!(is_maskable(&KindId::new("", "v1", "Secret", "secrets")));
        assert!(!is_maskable(&KindId::new(
            "",
            "v1",
            "ConfigMap",
            "configmaps"
        )));
        // A custom resource that happens to be called Secret is not the one
        // Kubernetes means.
        assert!(!is_maskable(&KindId::new(
            "example.com",
            "v1",
            "Secret",
            "secrets"
        )));
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
