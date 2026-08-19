//! Watching one kind for one cluster.
//!
//! The stream mechanics live in `kube`; what this module owns is the
//! translation into [`ClusterEvent`]s and the connection state machine around
//! it — including the rule that a rejected credential ends the session with a
//! stated reason rather than retrying silently forever.
//!
//! Objects arrive as `DynamicObject`, so pods and a CRD nobody has heard of
//! take exactly the same path; what differs is the projector that turns them
//! into rows.

use std::sync::Arc;

use futures::StreamExt as _;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Client, ResourceExt as _};
use periscope_bridge::{ClusterEvent, ClusterId, ConnectionState, EventSink, KindId, ResourceRow};

use crate::columns::{self, KindColumns};
use crate::errors::{Failure, attribute_plugin, classify_watch};
use crate::printer::PrinterColumn;

/// What a watch needs to know about the kind it is watching.
///
/// Bundled rather than passed as three arguments because all three come from
/// the same answer — discovery's — and a watch that had the kind but not its
/// declared columns would quietly render the generic table instead.
#[derive(Clone, Debug)]
pub struct Watched {
    /// Which kind to watch.
    pub kind: KindId,
    /// Whether its objects live in namespaces, so the URL is built correctly.
    pub namespaced: bool,
    /// The printer columns its CustomResourceDefinition declared, if any.
    pub printer_columns: Arc<[PrinterColumn]>,
}

/// What to do after handling a watch error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterError {
    /// Keep watching; `kube`'s backoff decides when to retry.
    Retry,
    /// Stop. The credential was rejected and retrying would just hammer the
    /// apiserver with a token it has already refused.
    Stop,
}

/// Which objects a watch covers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// One namespace, or `None` for every namespace.
    pub namespace: Option<Arc<str>>,
    /// A label selector, e.g. `app=web,tier!=cache`.
    pub selector: Option<Arc<str>>,
}

/// Accumulates watch events into the events the UI understands.
///
/// Kept separate from the stream loop so the whole translation — including the
/// buffering that turns a resync into one atomic replacement — is testable
/// without a cluster.
#[derive(Debug)]
pub struct ResourceStream {
    cluster: ClusterId,
    kind: KindId,
    table: KindColumns,
    /// Objects seen since the last `Init`, held back until `InitDone` so the
    /// UI swaps the table in one go rather than growing it row by row.
    pending: Vec<ResourceRow>,
    /// The credential plugin this cluster authenticates with, if any.
    credential_plugin: Option<String>,
    /// Whether the last thing this stream saw was a failure, so a recovery is
    /// reported once rather than on every event afterwards.
    degraded: bool,
}

impl ResourceStream {
    /// A stream translator for one kind on one cluster.
    pub fn new(cluster: ClusterId, kind: KindId) -> Self {
        let table = columns::for_kind(&kind);
        Self {
            cluster,
            kind,
            table,
            pending: Vec::new(),
            credential_plugin: None,
            degraded: false,
        }
    }

    /// Whether the last thing this stream saw was a failure.
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Renders this kind on the printer columns its CRD declared.
    ///
    /// An empty slice — every built-in kind, and every CRD that declares none —
    /// leaves the table exactly as [`ResourceStream::new`] built it.
    pub fn with_printer_columns(mut self, columns: Arc<[PrinterColumn]>) -> Self {
        self.table = columns::for_kind_with(&self.kind, columns);
        self
    }

    /// Notes that the stream is producing events again, if it was not.
    ///
    /// Any successful event means the apiserver answered, and that is the only
    /// signal available: `kube`'s watcher resumes an interrupted watch from the
    /// last resource version it saw, so a brief outage produces no fresh list
    /// and no `InitDone` — on a quiet namespace it produces nothing at all for
    /// a while. Waiting for a resync to declare recovery therefore left the
    /// cluster marked degraded indefinitely, with a stale error on screen,
    /// while the watch was in fact healthy. The e2e fault-injection test is
    /// what caught it.
    pub fn on_success(&mut self) -> Option<ClusterEvent> {
        if !self.degraded {
            return None;
        }
        self.degraded = false;

        Some(ClusterEvent::Status {
            cluster: self.cluster.clone(),
            state: ConnectionState::Connected,
        })
    }

    /// Names the credential plugin, so auth failures can say which binary was
    /// involved. `kube` reports a plugin that will not start as "No such file
    /// or directory" and never mentions what it tried to run.
    pub fn with_credential_plugin(mut self, plugin: Option<String>) -> Self {
        self.credential_plugin = plugin;
        self
    }

    /// Translates one watch event, if it produces anything for the UI.
    pub fn apply(&mut self, event: watcher::Event<DynamicObject>) -> Option<ClusterEvent> {
        match event {
            watcher::Event::Init => {
                self.pending.clear();
                None
            }
            watcher::Event::InitApply(object) => {
                self.pending.push(self.table.row(&object));
                None
            }
            watcher::Event::InitDone => Some(ClusterEvent::ResourceReset {
                cluster: self.cluster.clone(),
                kind: self.kind.clone(),
                columns: Arc::clone(&self.table.columns),
                rows: Arc::from(std::mem::take(&mut self.pending)),
            }),
            watcher::Event::Apply(object) => Some(ClusterEvent::ResourceApplied {
                cluster: self.cluster.clone(),
                kind: self.kind.clone(),
                row: Arc::new(self.table.row(&object)),
            }),
            watcher::Event::Delete(object) => Some(ClusterEvent::ResourceDeleted {
                cluster: self.cluster.clone(),
                kind: self.kind.clone(),
                key: columns::key_of(&object),
            }),
        }
    }

    /// Translates a watch failure into a connection state and a verdict on
    /// whether the session can continue.
    pub fn on_error(&mut self, error: &watcher::Error) -> (ClusterEvent, AfterError) {
        // Whatever we had buffered belongs to a list that will now be redone.
        self.pending.clear();
        self.degraded = true;

        let (state, after) = match classify_watch(error) {
            Failure::Auth(reason) => (
                ConnectionState::AuthFailed {
                    reason: attribute_plugin(reason, self.credential_plugin.as_deref()),
                },
                AfterError::Stop,
            ),
            Failure::Other(reason) => (
                ConnectionState::Degraded {
                    // A watch failure is about one kind; say which, or the user
                    // reads it as the whole cluster being broken.
                    reason: format!("{}: {reason}", self.kind),
                },
                AfterError::Retry,
            ),
        };

        (
            ClusterEvent::Status {
                cluster: self.cluster.clone(),
                state,
            },
            after,
        )
    }

    /// The cluster this stream belongs to.
    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }

    /// The kind this stream carries.
    pub fn kind(&self) -> &KindId {
        &self.kind
    }
}

/// The `ApiResource` handle for a kind.
pub fn api_resource(kind: &KindId) -> ApiResource {
    ApiResource {
        group: kind.group.to_string(),
        version: kind.version.to_string(),
        api_version: kind.api_version(),
        kind: kind.kind.to_string(),
        plural: kind.plural.to_string(),
    }
}

/// Builds the API handle for a kind, honouring the namespace filter.
pub fn api_for(
    client: Client,
    kind: &KindId,
    namespaced: bool,
    namespace: Option<&str>,
) -> Api<DynamicObject> {
    let resource = api_resource(kind);

    match namespace {
        // A namespace filter on a cluster-scoped kind is meaningless rather
        // than an error: nodes do not live anywhere.
        Some(namespace) if namespaced => Api::namespaced_with(client, namespace, &resource),
        _ => Api::all_with(client, &resource),
    }
}

/// How often a degraded watch asks the apiserver whether it is back.
///
/// Only ever runs while something is already broken, so the cost is zero on a
/// healthy cluster, and it is deliberately slower than `kube`'s own retry —
/// this answers the UI's question, it does not do the reconnecting.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether the apiserver will answer for this kind right now.
///
/// One object, not a listing: the question is whether the request completes at
/// all, and asking for a whole namespace to answer it would be its own load
/// problem on a large cluster.
async fn is_reachable(api: &Api<DynamicObject>) -> bool {
    api.list(&kube::api::ListParams::default().limit(1))
        .await
        .is_ok()
}

/// Watches a kind until the task is cancelled, the credential is rejected, or
/// the UI goes away.
pub async fn run(
    cluster: ClusterId,
    watched: Watched,
    client: Client,
    filter: Filter,
    credential_plugin: Option<String>,
    events: EventSink,
) {
    let Watched {
        kind,
        namespaced,
        printer_columns,
    } = watched;
    let api = api_for(client, &kind, namespaced, filter.namespace.as_deref());
    let config = watcher::Config {
        label_selector: filter.selector.as_deref().map(str::to_owned),
        ..watcher::Config::default()
    };
    // Kept for the liveness probe below; the watcher takes ownership of its own.
    let probe = api.clone();

    let mut stream = Box::pin(
        watcher(api, config)
            // Managed fields are most of the bytes in a typical object and
            // nothing renders them; dropping them as they arrive keeps a
            // 10k-object listing out of the hundreds of megabytes.
            .modify(|object| {
                object.managed_fields_mut().clear();
            })
            .default_backoff(),
    );
    let mut translator = ResourceStream::new(cluster.clone(), kind.clone())
        .with_printer_columns(printer_columns)
        .with_credential_plugin(credential_plugin);

    loop {
        // While degraded, the stream is raced against a cheap liveness probe.
        // A watch that `kube` resumed from its last resource version emits
        // nothing at all until something in the namespace changes, so on a
        // quiet cluster there is no event to recover on — and the UI would sit
        // on a stale error indefinitely. Asking the apiserver directly is the
        // only thing that actually answers "is it back".
        let next = if translator.is_degraded() {
            tokio::select! {
                next = stream.next() => next,
                _ = tokio::time::sleep(PROBE_INTERVAL) => {
                    if is_reachable(&probe).await
                        && let Some(recovered) = translator.on_success()
                    {
                        tracing::info!(%cluster, %kind, "watch recovered");
                        if events.send(recovered).is_closed() {
                            return;
                        }
                    }
                    continue;
                }
            }
        } else {
            stream.next().await
        };

        let Some(next) = next else {
            break;
        };

        match next {
            Ok(event) => {
                // Reported before the event itself, and before translating:
                // most watch events produce nothing for the UI, and a recovery
                // that waits for one that does may wait forever on a quiet
                // namespace.
                if let Some(recovered) = translator.on_success() {
                    tracing::info!(%cluster, %kind, "watch recovered");
                    if events.send(recovered).is_closed() {
                        return;
                    }
                }

                let Some(event) = translator.apply(event) else {
                    continue;
                };

                if events.send(event).is_closed() {
                    return;
                }
            }
            Err(error) => {
                let (event, after) = translator.on_error(&error);
                tracing::warn!(%cluster, %kind, %error, ?after, "watch failed");

                if events.send(event).is_closed() {
                    return;
                }
                if after == AfterError::Stop {
                    return;
                }
            }
        }
    }

    tracing::info!(%cluster, %kind, "watch ended");
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::ResourceKey;
    use serde_json::json;

    fn pods() -> KindId {
        KindId::new("", "v1", "Pod", "pods")
    }

    fn pod(name: &str) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": name, "namespace": "default" },
            "spec": { "containers": [{ "name": "app" }] },
            "status": { "phase": "Running" }
        }))
        .expect("fixture is a valid object")
    }

    fn api_error(code: u16) -> watcher::Error {
        watcher::Error::WatchStartFailed(kube::Error::Api(Box::new(kube::core::Status {
            code,
            message: "token has expired".to_owned(),
            ..kube::core::Status::default()
        })))
    }

    #[test]
    fn an_initial_list_is_held_back_until_it_is_complete() {
        let mut stream = ResourceStream::new("prod".into(), pods());

        assert_eq!(stream.apply(watcher::Event::Init), None);
        assert_eq!(stream.apply(watcher::Event::InitApply(pod("a"))), None);
        assert_eq!(stream.apply(watcher::Event::InitApply(pod("b"))), None);

        // A half-listed table would flash rows in and out; the reset lands once.
        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::ResourceReset {
                cluster,
                kind,
                columns,
                rows,
            }) => {
                assert_eq!(cluster.as_str(), "prod");
                assert_eq!(kind, pods());
                assert_eq!(rows.len(), 2);
                // Columns travel with the data, so the UI needs no table of its own.
                assert_eq!(&*columns[0].name, "READY");
                assert_eq!(rows[0].cells.len(), columns.len());
            }
            other => panic!("expected a reset, got {other:?}"),
        }
    }

    #[test]
    fn a_second_list_does_not_inherit_the_first_ones_objects() {
        let mut stream = ResourceStream::new("prod".into(), pods());
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("a")));

        // Watch dropped mid-list and restarted.
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("b")));

        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::ResourceReset { rows, .. }) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(&*rows[0].key.name, "b");
            }
            other => panic!("expected a reset, got {other:?}"),
        }
    }

    #[test]
    fn updates_and_deletes_map_to_their_own_events() {
        let mut stream = ResourceStream::new("prod".into(), pods());

        match stream.apply(watcher::Event::Apply(pod("a"))) {
            Some(ClusterEvent::ResourceApplied { row, kind, .. }) => {
                assert_eq!(row.key, ResourceKey::new("default", "a"));
                assert_eq!(kind, pods());
            }
            other => panic!("expected an apply, got {other:?}"),
        }

        match stream.apply(watcher::Event::Delete(pod("a"))) {
            Some(ClusterEvent::ResourceDeleted { key, .. }) => {
                assert_eq!(key, ResourceKey::new("default", "a"));
            }
            other => panic!("expected a delete, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_credential_stops_the_session_and_says_why() {
        let mut stream = ResourceStream::new("prod".into(), pods());
        let (event, after) = stream.on_error(&api_error(401));

        assert_eq!(after, AfterError::Stop);
        match event {
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { reason },
                ..
            } => assert!(reason.contains("token has expired"), "{reason}"),
            other => panic!("expected an auth failure, got {other:?}"),
        }
    }

    #[test]
    fn an_auth_failure_names_the_credential_plugin_that_was_run() {
        // Without this the user sees "No such file or directory" and has no way
        // to know which binary kubeconfig asked for.
        let mut stream = ResourceStream::new("prod".into(), pods())
            .with_credential_plugin(Some("gke-gcloud-auth-plugin".to_owned()));
        let (event, _) = stream.on_error(&watcher::Error::WatchStartFailed(kube::Error::Auth(
            kube::client::AuthError::AuthExecStart(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )),
        )));

        match event {
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { reason },
                ..
            } => assert!(reason.contains("gke-gcloud-auth-plugin"), "{reason}"),
            other => panic!("expected an auth failure, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_already_named_in_the_error_is_not_repeated() {
        let mut stream = ResourceStream::new("prod".into(), pods())
            .with_credential_plugin(Some("aws".to_owned()));
        // kube names the command itself when the plugin runs and fails; saying
        // it twice reads like two different problems.
        let error =
            watcher::Error::WatchStartFailed(kube::Error::Api(Box::new(kube::core::Status {
                code: 401,
                message: "auth exec command 'aws' failed with status 255".to_owned(),
                ..kube::core::Status::default()
            })));
        let (event, _) = stream.on_error(&error);

        let ClusterEvent::Status {
            state: ConnectionState::AuthFailed { reason },
            ..
        } = event
        else {
            panic!("expected an auth failure")
        };
        assert!(
            !reason.contains("credential plugin:"),
            "the plugin was named twice: {reason}"
        );
    }

    #[test]
    fn a_transient_failure_degrades_and_names_the_kind_that_failed() {
        let mut stream = ResourceStream::new("prod".into(), pods());
        let (event, after) = stream.on_error(&api_error(500));

        assert_eq!(after, AfterError::Retry);
        match event {
            ClusterEvent::Status {
                state: ConnectionState::Degraded { reason },
                ..
            } => assert!(reason.starts_with("pods:"), "{reason}"),
            other => panic!("expected a degraded status, got {other:?}"),
        }
    }

    #[test]
    fn a_healthy_stream_reports_no_recovery() {
        // Every event would otherwise carry a "connected" behind it.
        let mut stream = ResourceStream::new("prod".into(), pods());
        assert!(stream.on_success().is_none());

        stream.apply(watcher::Event::Init);
        assert!(stream.on_success().is_none());
    }

    #[test]
    fn any_event_after_a_failure_counts_as_recovered() {
        // The regression this exists for: recovery used to wait for `InitDone`,
        // which a resumed watch never sends. A cluster that blinked stayed
        // marked degraded forever, with a stale reason on screen, while the
        // watch was actually healthy.
        let mut stream = ResourceStream::new("prod".into(), pods());
        stream.on_error(&api_error(500));

        match stream.on_success() {
            Some(ClusterEvent::Status { state, .. }) => {
                assert_eq!(state, ConnectionState::Connected);
            }
            other => panic!("expected a recovery, got {other:?}"),
        }
    }

    #[test]
    fn a_recovery_is_reported_once_not_on_every_event_afterwards() {
        let mut stream = ResourceStream::new("prod".into(), pods());
        stream.on_error(&api_error(500));

        assert!(stream.on_success().is_some());
        assert!(stream.on_success().is_none());
        assert!(stream.on_success().is_none());

        // A second failure arms it again.
        stream.on_error(&api_error(500));
        assert!(stream.on_success().is_some());
    }

    #[test]
    fn a_failure_discards_a_partial_list() {
        let mut stream = ResourceStream::new("prod".into(), pods());
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("a")));
        stream.on_error(&api_error(500));

        // The retry lists again from scratch; the half-list must not leak into
        // the next reset, or deleted objects would reappear.
        stream.apply(watcher::Event::Init);
        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::ResourceReset { rows, .. }) => assert!(rows.is_empty()),
            other => panic!("expected an empty reset, got {other:?}"),
        }
    }

    #[test]
    fn a_crds_declared_columns_reach_the_ui_with_its_rows() {
        use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceColumnDefinition;

        let certificates = KindId::new("cert-manager.io", "v1", "Certificate", "certificates");
        let declared = crate::printer::compile(
            &certificates,
            &[CustomResourceColumnDefinition {
                name: "Secret".to_owned(),
                type_: "string".to_owned(),
                json_path: ".spec.secretName".to_owned(),
                ..CustomResourceColumnDefinition::default()
            }],
        );

        let mut stream =
            ResourceStream::new("prod".into(), certificates.clone()).with_printer_columns(declared);
        let certificate: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1",
            "kind": "Certificate",
            "metadata": { "name": "web-tls", "namespace": "default" },
            "spec": { "secretName": "web-tls" }
        }))
        .expect("fixture is a valid object");

        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(certificate));

        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::ResourceReset { columns, rows, .. }) => {
                // The whole point: the UI is told the heading and the cell, and
                // needs to know nothing about cert-manager to render either.
                assert_eq!(&*columns[0].name, "SECRET");
                assert_eq!(&*rows[0].cells[0], "web-tls");
            }
            other => panic!("expected a reset, got {other:?}"),
        }
    }

    #[test]
    fn a_custom_resource_streams_through_the_same_path() {
        let widgets = KindId::new("example.com", "v1", "Widget", "widgets");
        let mut stream = ResourceStream::new("prod".into(), widgets.clone());

        let widget: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "example.com/v1",
            "kind": "Widget",
            "metadata": { "name": "sprocket", "namespace": "default" },
            "spec": { "size": 3 }
        }))
        .expect("fixture is a valid object");

        match stream.apply(watcher::Event::Apply(widget)) {
            Some(ClusterEvent::ResourceApplied { kind, row, .. }) => {
                assert_eq!(kind, widgets);
                assert_eq!(row.key, ResourceKey::new("default", "sprocket"));
            }
            other => panic!("expected an apply, got {other:?}"),
        }
    }
}
