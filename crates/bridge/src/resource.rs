//! Cluster data as the UI needs it.
//!
//! These are *projections*, not Kubernetes objects. The cluster layer turns
//! whatever the apiserver sent — a typed `Pod`, a `DynamicObject` for a CRD
//! nobody has ever seen — into rows of strings plus a state, and that is what
//! the store and the UI work with. Consequences:
//!
//! * `k8s-openapi` and `kube` stop at the cluster layer, so the whole rendering
//!   path is testable with hand-written fixtures.
//! * A new resource kind needs no new types here. Columns travel *with* the
//!   data, which is what lets a CRD's own printer columns be rendered without
//!   the UI knowing anything about that CRD.
//!
//! Full objects are deliberately absent: the detail view fetches the one object
//! it is showing on demand (`docs/DECISIONS.md` ADR-0018) rather than keeping
//! every object's YAML in memory.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Identifies an object within a cluster.
///
/// Ordered by namespace then name, which is the order tables render in, so a
/// `BTreeMap` keyed by this needs no separate sort. Cluster-scoped objects
/// carry an empty namespace.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceKey {
    /// Namespace the object lives in, empty for cluster-scoped kinds.
    pub namespace: Arc<str>,
    /// Object name, unique within the namespace for a given kind.
    pub name: Arc<str>,
}

impl ResourceKey {
    /// Builds a key from anything string-shaped.
    pub fn new(namespace: impl AsRef<str>, name: impl AsRef<str>) -> Self {
        Self {
            namespace: Arc::from(namespace.as_ref()),
            name: Arc::from(name.as_ref()),
        }
    }

    /// A key for a cluster-scoped object.
    pub fn cluster_scoped(name: impl AsRef<str>) -> Self {
        Self::new("", name)
    }

    /// Whether this object lives in a namespace.
    pub fn is_namespaced(&self) -> bool {
        !self.namespace.is_empty()
    }
}

impl std::fmt::Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_namespaced() {
            write!(f, "{}/{}", self.namespace, self.name)
        } else {
            f.write_str(&self.name)
        }
    }
}

/// Identifies a resource kind: group, version and kind, as discovery reports it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KindId {
    /// API group, empty for the core group.
    pub group: Arc<str>,
    /// API version, e.g. `v1`.
    pub version: Arc<str>,
    /// Singular PascalCase kind, e.g. `Deployment`.
    pub kind: Arc<str>,
    /// Lowercase plural, e.g. `deployments`. This is what users type.
    pub plural: Arc<str>,
}

impl KindId {
    /// Builds a kind identifier.
    pub fn new(
        group: impl AsRef<str>,
        version: impl AsRef<str>,
        kind: impl AsRef<str>,
        plural: impl AsRef<str>,
    ) -> Self {
        Self {
            group: Arc::from(group.as_ref()),
            version: Arc::from(version.as_ref()),
            kind: Arc::from(kind.as_ref()),
            plural: Arc::from(plural.as_ref()),
        }
    }

    /// Whether this kind is in the core (empty) group.
    pub fn is_core(&self) -> bool {
        self.group.is_empty()
    }

    /// How `kubectl` names this kind: `pods`, or `deployments.apps`.
    pub fn label(&self) -> String {
        if self.is_core() {
            self.plural.to_string()
        } else {
            format!("{}.{}", self.plural, self.group)
        }
    }

    /// `apiVersion` as it appears on an object.
    pub fn api_version(&self) -> String {
        if self.is_core() {
            self.version.to_string()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

impl std::fmt::Display for KindId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// What discovery said about a kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindInfo {
    /// Which kind this is.
    pub id: KindId,
    /// Whether objects live in namespaces.
    pub namespaced: bool,
    /// Whether the apiserver will let us list and watch it. Kinds that cannot
    /// be watched are still listed — hiding them would be a lie — but they
    /// cannot be opened.
    pub watchable: bool,
    /// Whether this kind came from a CustomResourceDefinition.
    pub custom: bool,
}

impl KindInfo {
    /// Sort key: built-in kinds first, then alphabetically, which is the order
    /// the picker renders in.
    pub fn sort_key(&self) -> (bool, String) {
        (self.custom, self.id.label())
    }
}

/// A column in a resource table.
///
/// Columns travel with the data rather than being compiled into the UI, so a
/// custom resource's own printer columns render like any other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    /// Heading, upper-case by convention: `READY`, `STATUS`.
    pub name: Arc<str>,
    /// Fixed width in pixels, or `None` to take the remaining space.
    pub width: Option<u32>,
}

impl ColumnSpec {
    /// A fixed-width column.
    pub fn fixed(name: impl AsRef<str>, width: u32) -> Self {
        Self {
            name: Arc::from(name.as_ref()),
            width: Some(width),
        }
    }

    /// A column that takes whatever space is left.
    pub fn flexible(name: impl AsRef<str>) -> Self {
        Self {
            name: Arc::from(name.as_ref()),
            width: None,
        }
    }
}

/// How a row reads at a glance.
///
/// Decided by the projector, which is the only place that knows what a given
/// kind's fields mean. Anything unrecognised is [`RowState::Neutral`] rather
/// than guessed at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowState {
    /// Nothing to say about it.
    #[default]
    Neutral,
    /// Working as intended.
    Healthy,
    /// On its way somewhere: scheduling, pulling, initialising, terminating.
    Transient,
    /// Needs attention.
    Failing,
}

/// One row of a resource table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRow {
    /// Namespace and name.
    pub key: ResourceKey,
    /// `metadata.uid`, which distinguishes a replacement object from an update
    /// to the same one.
    pub uid: Option<Arc<str>>,
    /// The kind-specific cells, in the order of the kind's [`ColumnSpec`]s.
    pub cells: Arc<[Arc<str>]>,
    /// How the row reads at a glance.
    pub state: RowState,
    /// `metadata.creationTimestamp`, for the age column.
    pub created: Option<SystemTime>,
}

impl ResourceRow {
    /// How long the object has existed, as of `now`.
    ///
    /// `None` when the object carried no creation timestamp; clocks that
    /// disagree yield [`Duration::ZERO`] rather than a negative age.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        self.created
            .map(|created| now.duration_since(created).unwrap_or(Duration::ZERO))
    }

    /// The cell at `index`, or an empty string when a projector produced fewer
    /// cells than the kind has columns.
    pub fn cell(&self, index: usize) -> &str {
        self.cells.get(index).map(|cell| &**cell).unwrap_or("")
    }
}

/// One kubeconfig context, as offered in the cluster picker.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContextInfo {
    /// Context name. This is also the [`crate::ClusterId`].
    pub name: Arc<str>,
    /// The cluster entry the context points at.
    pub cluster: Arc<str>,
    /// The user entry the context authenticates as, when it names one.
    pub user: Option<Arc<str>>,
    /// The context's default namespace, when it sets one.
    pub namespace: Option<Arc<str>>,
}

/// A full object, fetched on demand for the detail view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectDetail {
    /// Which object this is.
    pub key: ResourceKey,
    /// The object as YAML, ready to render.
    pub yaml: Arc<str>,
    /// Whether this object carries values that are masked unless revealed —
    /// true for Secrets, so the UI can offer the reveal and say what it did.
    pub maskable: bool,
    /// Whether the values in `yaml` are the real ones.
    pub revealed: bool,
    /// Events referring to this object, newest last.
    pub events: Arc<[EventLine]>,
    /// Owner references, for navigating up the chain.
    pub owners: Arc<[OwnerRef]>,
}

/// One line of the events table in the detail view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventLine {
    /// `Normal` or `Warning`.
    pub kind: Arc<str>,
    /// Short reason, e.g. `BackOff`.
    pub reason: Arc<str>,
    /// Human-readable message.
    pub message: Arc<str>,
    /// When it last happened.
    pub last_seen: Option<SystemTime>,
    /// How many times it has happened.
    pub count: u32,
}

impl EventLine {
    /// Whether this event is a warning.
    pub fn is_warning(&self) -> bool {
        &*self.kind == "Warning"
    }
}

/// An owner reference, resolved enough to navigate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRef {
    /// The owner's kind, as `apiVersion` + `kind` name it.
    pub api_version: Arc<str>,
    /// Kind name, e.g. `ReplicaSet`.
    pub kind: Arc<str>,
    /// Owner name, in the same namespace as the owned object.
    pub name: Arc<str>,
    /// Whether this owner controls the object, as opposed to merely owning it.
    pub controller: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(namespace: &str, name: &str) -> ResourceRow {
        ResourceRow {
            key: ResourceKey::new(namespace, name),
            uid: None,
            cells: Arc::from([Arc::from("1/1"), Arc::from("Running")]),
            state: RowState::Healthy,
            created: None,
        }
    }

    #[test]
    fn keys_sort_by_namespace_then_name() {
        let mut keys = [
            ResourceKey::new("kube-system", "coredns"),
            ResourceKey::new("default", "web"),
            ResourceKey::new("default", "api"),
        ];
        keys.sort();

        let rendered: Vec<_> = keys.iter().map(ResourceKey::to_string).collect();
        assert_eq!(
            rendered,
            ["default/api", "default/web", "kube-system/coredns"]
        );
    }

    #[test]
    fn a_cluster_scoped_key_renders_as_a_bare_name() {
        let node = ResourceKey::cluster_scoped("node-1");
        assert!(!node.is_namespaced());
        assert_eq!(node.to_string(), "node-1");
    }

    #[test]
    fn kinds_render_the_way_kubectl_names_them() {
        let pods = KindId::new("", "v1", "Pod", "pods");
        let deployments = KindId::new("apps", "v1", "Deployment", "deployments");

        assert_eq!(pods.label(), "pods");
        assert_eq!(pods.api_version(), "v1");
        assert_eq!(deployments.label(), "deployments.apps");
        assert_eq!(deployments.api_version(), "apps/v1");
    }

    #[test]
    fn built_in_kinds_sort_before_custom_ones() {
        let built_in = KindInfo {
            id: KindId::new("apps", "v1", "Deployment", "deployments"),
            namespaced: true,
            watchable: true,
            custom: false,
        };
        let custom = KindInfo {
            id: KindId::new("argoproj.io", "v1alpha1", "Application", "applications"),
            namespaced: true,
            watchable: true,
            custom: true,
        };

        let mut kinds = [custom.clone(), built_in.clone()];
        kinds.sort_by_key(KindInfo::sort_key);
        assert_eq!(kinds, [built_in, custom]);
    }

    #[test]
    fn a_missing_cell_reads_as_empty_rather_than_panicking() {
        // A projector that returns fewer cells than the kind has columns is a
        // bug, but it must not take the window down mid-render.
        let row = row("default", "api");
        assert_eq!(row.cell(1), "Running");
        assert_eq!(row.cell(9), "");
    }

    #[test]
    fn a_clock_running_backwards_does_not_produce_a_negative_age() {
        let now = SystemTime::now();
        let mut row = row("default", "api");
        row.created = Some(now + Duration::from_secs(60));
        assert_eq!(row.age(now), Some(Duration::ZERO));
    }

    #[test]
    fn warnings_are_distinguishable_from_ordinary_events() {
        let warning = EventLine {
            kind: Arc::from("Warning"),
            reason: Arc::from("BackOff"),
            message: Arc::from("Back-off restarting failed container"),
            last_seen: None,
            count: 12,
        };
        assert!(warning.is_warning());

        let normal = EventLine {
            kind: Arc::from("Normal"),
            ..warning.clone()
        };
        assert!(!normal.is_warning());
    }
}
