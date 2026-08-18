//! Everything the UI renders, and the only place that decides what is true.
//!
//! The view holds one of these and asks it questions; it never infers state
//! from what happens to be in a table. That separation is what makes "the
//! table is empty" and "the token expired" impossible to confuse, and it is why
//! every rule below is testable without a window or a cluster.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use periscope_bridge::{
    ClusterEvent, ClusterId, ColumnSpec, ContextInfo, KindId, KindInfo, ObjectDetail, ResourceKey,
    ResourceRow,
};

use crate::connections::{Connection, ConnectionRegistry};
use crate::table::ResourceTable;

/// What the detail pane is showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    /// A fetch is in flight.
    Loading {
        /// Kind of the object being fetched.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
    },
    /// The object, its events and its owners.
    Ready {
        /// Kind of the object.
        kind: KindId,
        /// What was fetched.
        object: Arc<ObjectDetail>,
    },
    /// The fetch failed. Shown in place of the object, never as a blank pane.
    Failed {
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
        /// Verbatim underlying reason.
        reason: String,
    },
}

impl Detail {
    /// The object this pane is about.
    pub fn key(&self) -> &ResourceKey {
        match self {
            Self::Loading { key, .. } | Self::Failed { key, .. } => key,
            Self::Ready { object, .. } => &object.key,
        }
    }

    /// The kind of that object.
    pub fn kind(&self) -> &KindId {
        match self {
            Self::Loading { kind, .. } | Self::Failed { kind, .. } | Self::Ready { kind, .. } => {
                kind
            }
        }
    }
}

/// Which objects the active view covers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// One namespace, or `None` for every namespace.
    pub namespace: Option<Arc<str>>,
    /// A label selector applied by the apiserver.
    pub selector: Option<Arc<str>>,
    /// A substring matched against namespace and name, applied here.
    pub search: Option<Arc<str>>,
}

impl Filters {
    /// Whether a row survives the client-side part of the filter.
    fn matches(&self, row: &ResourceRow) -> bool {
        let Some(search) = self.search.as_deref() else {
            return true;
        };
        if search.is_empty() {
            return true;
        }

        let search = search.to_lowercase();
        row.key.name.to_lowercase().contains(&search)
            || row.key.namespace.to_lowercase().contains(&search)
            || row
                .cells
                .iter()
                .any(|cell| cell.to_lowercase().contains(&search))
    }
}

/// The application's state.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    connections: ConnectionRegistry,
    contexts: Vec<ContextInfo>,
    current: Option<ClusterId>,
    config_error: Option<String>,
    active: Option<ClusterId>,
    kinds: BTreeMap<ClusterId, Arc<[KindInfo]>>,
    kind: Option<KindId>,
    filters: Filters,
    tables: BTreeMap<(ClusterId, KindId), ResourceTable>,
    detail: Option<Detail>,
    /// Materialised rows for the active view, for indexed access by the
    /// virtualised list. Rebuilt only when something it shows changes, and
    /// shared as an `Arc` so a render that captures them costs one refcount
    /// bump rather than a copy of ten thousand pointers.
    rows: Arc<[Arc<ResourceRow>]>,
    columns: Arc<[ColumnSpec]>,
}

impl AppState {
    /// Empty state, before kubeconfig has been read.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one event in, reporting whether the UI needs a repaint.
    pub fn apply(&mut self, event: &ClusterEvent, now: Instant) -> bool {
        let mut changed = self.connections.apply(event, now);

        match event {
            ClusterEvent::Contexts { contexts, current } => {
                self.config_error = None;
                self.contexts = contexts.to_vec();
                self.current = current.clone();

                for context in &self.contexts {
                    self.connections.track(ClusterId::new(&*context.name), now);
                }

                // Selecting the current context on first read is what the user
                // means by "open my cluster"; re-reading kubeconfig later must
                // not yank the view away from whatever they are looking at.
                if self.active.is_none() {
                    self.active = current.clone();
                }
                changed = true;
            }

            ClusterEvent::ConfigFailed { reason } => {
                // Do not clear the contexts: the last good read is still the
                // best information available, and the banner explains the rest.
                self.config_error = Some(reason.clone());
                changed = true;
            }

            ClusterEvent::Kinds { cluster, kinds } => {
                self.kinds.insert(cluster.clone(), Arc::clone(kinds));
                changed = true;
            }

            ClusterEvent::ResourceReset {
                cluster,
                kind,
                columns,
                rows,
            } => {
                let touched = self
                    .table_mut(cluster, kind)
                    .reset(Arc::clone(columns), rows);
                // A completed resync is the moment dropped events stop
                // mattering: the table was just rebuilt from scratch.
                self.connections.mark_fresh(cluster);
                changed |= self.touch(cluster, kind, touched);
            }

            ClusterEvent::ResourceApplied { cluster, kind, row } => {
                let touched = self.table_mut(cluster, kind).apply(Arc::clone(row));
                changed |= self.touch(cluster, kind, touched);
            }

            ClusterEvent::ResourceDeleted { cluster, kind, key } => {
                let touched = self.table_mut(cluster, kind).remove(key);
                changed |= self.touch(cluster, kind, touched);
            }

            ClusterEvent::Object {
                kind,
                detail: object,
                ..
            } => {
                // Ignore an answer to a question the user has moved on from.
                if self.awaiting(kind, &object.key) {
                    self.detail = Some(Detail::Ready {
                        kind: kind.clone(),
                        object: Arc::clone(object),
                    });
                    changed = true;
                }
            }

            ClusterEvent::ObjectFailed {
                kind, key, reason, ..
            } => {
                if self.awaiting(kind, key) {
                    self.detail = Some(Detail::Failed {
                        kind: kind.clone(),
                        key: key.clone(),
                        reason: reason.clone(),
                    });
                    changed = true;
                }
            }

            ClusterEvent::Status { .. }
            | ClusterEvent::Pong { .. }
            | ClusterEvent::Stale { .. } => {}
        }

        changed
    }

    /// Folds a whole flush batch in, rebuilding the rendered rows at most once.
    pub fn apply_batch<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a ClusterEvent>,
        now: Instant,
    ) -> bool {
        let mut changed = false;
        let mut rows_stale = false;

        for event in events {
            changed |= self.apply(event, now);
            rows_stale |= self.affects_active(event);
        }

        if rows_stale {
            self.refresh_rows();
        }
        changed
    }

    /// Switches which cluster is being viewed.
    pub fn select_cluster(&mut self, cluster: ClusterId) {
        if self.active.as_ref() == Some(&cluster) {
            return;
        }
        self.active = Some(cluster);
        self.detail = None;
        self.refresh_rows();
    }

    /// Switches which kind is being viewed.
    pub fn select_kind(&mut self, kind: KindId) {
        if self.kind.as_ref() == Some(&kind) {
            return;
        }
        self.kind = Some(kind);
        self.detail = None;
        self.refresh_rows();
    }

    /// Applies a namespace filter, or clears it with `None`.
    pub fn set_namespace(&mut self, namespace: Option<Arc<str>>) {
        self.filters.namespace = namespace.filter(|namespace| !namespace.is_empty());
        self.refresh_rows();
    }

    /// Applies a label selector, or clears it with `None`.
    pub fn set_selector(&mut self, selector: Option<Arc<str>>) {
        self.filters.selector = selector.filter(|selector| !selector.is_empty());
        self.refresh_rows();
    }

    /// Applies the client-side text filter.
    pub fn set_search(&mut self, search: Option<Arc<str>>) {
        self.filters.search = search.filter(|search| !search.is_empty());
        self.refresh_rows();
    }

    /// The filters currently applied.
    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    /// Records that a detail fetch is in flight.
    pub fn open_detail(&mut self, kind: KindId, key: ResourceKey) {
        self.detail = Some(Detail::Loading { kind, key });
    }

    /// Closes the detail pane.
    pub fn close_detail(&mut self) {
        self.detail = None;
    }

    /// What the detail pane is showing, if anything.
    pub fn detail(&self) -> Option<&Detail> {
        self.detail.as_ref()
    }

    /// The cluster currently being viewed.
    pub fn active(&self) -> Option<&ClusterId> {
        self.active.as_ref()
    }

    /// The kind currently being viewed.
    pub fn kind(&self) -> Option<&KindId> {
        self.kind.as_ref()
    }

    /// The kinds the active cluster serves.
    pub fn kinds(&self) -> &[KindInfo] {
        self.active
            .as_ref()
            .and_then(|cluster| self.kinds.get(cluster))
            .map(|kinds| &**kinds)
            .unwrap_or_default()
    }

    /// Looks up what discovery said about a kind on the active cluster.
    pub fn kind_info(&self, kind: &KindId) -> Option<&KindInfo> {
        self.kinds().iter().find(|info| &info.id == kind)
    }

    /// The rows the table renders, filtered and in namespace-then-name order.
    pub fn rows(&self) -> &[Arc<ResourceRow>] {
        &self.rows
    }

    /// The same rows, shared, for a virtualised list that must own what it
    /// renders from.
    pub fn rows_shared(&self) -> Arc<[Arc<ResourceRow>]> {
        Arc::clone(&self.rows)
    }

    /// The columns those rows carry cells for.
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// Every context kubeconfig defines.
    pub fn contexts(&self) -> &[ContextInfo] {
        &self.contexts
    }

    /// The kubeconfig `current-context`, when there is one.
    pub fn current_context(&self) -> Option<&ClusterId> {
        self.current.as_ref()
    }

    /// Why kubeconfig could not be read, if it could not be.
    pub fn config_error(&self) -> Option<&str> {
        self.config_error.as_deref()
    }

    /// Connection health for one cluster.
    pub fn connection(&self, cluster: &ClusterId) -> Option<&Connection> {
        self.connections.get(cluster)
    }

    /// Connection health for the cluster being viewed.
    pub fn active_connection(&self) -> Option<&Connection> {
        self.active.as_ref().and_then(|id| self.connections.get(id))
    }

    /// How many rows the active view holds before and after filtering.
    pub fn counts(&self) -> (usize, usize) {
        let total = self
            .active_table()
            .map_or(0, |(_, table): (_, &ResourceTable)| table.len());
        (total, self.rows.len())
    }

    /// How many rows a kind holds on the active cluster, for the picker.
    pub fn row_count(&self, kind: &KindId) -> usize {
        self.active
            .as_ref()
            .and_then(|cluster| self.tables.get(&(cluster.clone(), kind.clone())))
            .map_or(0, ResourceTable::len)
    }

    /// The namespaces present in the active table, for the namespace picker.
    pub fn namespaces(&self) -> Vec<Arc<str>> {
        self.active_table()
            .map(|(_, table)| table.namespaces())
            .unwrap_or_default()
    }

    fn active_table(&self) -> Option<(&KindId, &ResourceTable)> {
        let cluster = self.active.as_ref()?;
        let kind = self.kind.as_ref()?;
        let table = self.tables.get(&(cluster.clone(), kind.clone()))?;
        Some((kind, table))
    }

    fn table_mut(&mut self, cluster: &ClusterId, kind: &KindId) -> &mut ResourceTable {
        self.tables
            .entry((cluster.clone(), kind.clone()))
            .or_default()
    }

    /// Whether a detail event answers the fetch that is actually in flight.
    fn awaiting(&self, kind: &KindId, key: &ResourceKey) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|detail| detail.kind() == kind && detail.key() == key)
    }

    /// Reports a table change, and whether it is one the user can see.
    fn touch(&mut self, cluster: &ClusterId, kind: &KindId, changed: bool) -> bool {
        changed && self.active.as_ref() == Some(cluster) && self.kind.as_ref() == Some(kind)
    }

    fn affects_active(&self, event: &ClusterEvent) -> bool {
        let kind = match event {
            ClusterEvent::ResourceReset { kind, .. }
            | ClusterEvent::ResourceApplied { kind, .. }
            | ClusterEvent::ResourceDeleted { kind, .. } => kind,
            _ => return false,
        };
        event.cluster() == self.active.as_ref() && Some(kind) == self.kind.as_ref()
    }

    fn refresh_rows(&mut self) {
        let Some((_, table)) = self.active_table() else {
            self.rows = Arc::from([] as [Arc<ResourceRow>; 0]);
            self.columns = Arc::from([] as [ColumnSpec; 0]);
            return;
        };

        let columns = Arc::clone(table.columns());
        let namespace = self.filters.namespace.clone();
        let rows: Vec<_> = table
            .iter()
            // The namespace filter is applied by the apiserver too, but the
            // rows already held must follow it immediately rather than waiting
            // for the re-listing to arrive.
            .filter(|row| {
                namespace
                    .as_deref()
                    .is_none_or(|namespace| &*row.key.namespace == namespace)
            })
            .filter(|row| self.filters.matches(row))
            .cloned()
            .collect();

        self.columns = columns;
        self.rows = Arc::from(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::{ConnectionState, RowState};

    fn context(name: &str) -> ContextInfo {
        ContextInfo {
            name: Arc::from(name),
            cluster: Arc::from(format!("{name}-cluster").as_str()),
            user: None,
            namespace: None,
        }
    }

    fn contexts_event(names: &[&str], current: Option<&str>) -> ClusterEvent {
        ClusterEvent::Contexts {
            contexts: names.iter().copied().map(context).collect(),
            current: current.map(ClusterId::new),
        }
    }

    fn pods() -> KindId {
        KindId::new("", "v1", "Pod", "pods")
    }

    fn deployments() -> KindId {
        KindId::new("apps", "v1", "Deployment", "deployments")
    }

    fn columns() -> Arc<[ColumnSpec]> {
        Arc::from([ColumnSpec::fixed("STATUS", 100)])
    }

    fn row(namespace: &str, name: &str) -> ResourceRow {
        ResourceRow {
            key: ResourceKey::new(namespace, name),
            uid: None,
            cells: Arc::from([Arc::from("Running")]),
            state: RowState::Healthy,
            created: None,
        }
    }

    fn reset(cluster: &str, kind: KindId, rows: &[ResourceRow]) -> ClusterEvent {
        ClusterEvent::ResourceReset {
            cluster: cluster.into(),
            kind,
            columns: columns(),
            rows: Arc::from(rows.to_vec()),
        }
    }

    fn state_with_contexts() -> AppState {
        let mut state = AppState::new();
        state.apply_batch(
            &[contexts_event(&["prod", "staging"], Some("prod"))],
            Instant::now(),
        );
        state.select_kind(pods());
        state
    }

    fn row_names(state: &AppState) -> Vec<String> {
        state.rows().iter().map(|row| row.key.to_string()).collect()
    }

    #[test]
    fn reading_kubeconfig_selects_the_current_context() {
        let state = state_with_contexts();

        assert_eq!(state.contexts().len(), 2);
        assert_eq!(state.active(), Some(&ClusterId::new("prod")));
        // Every context is listed as idle so the picker can render it.
        assert_eq!(
            state.connection(&ClusterId::new("staging")).unwrap().state,
            ConnectionState::Idle
        );
    }

    #[test]
    fn re_reading_kubeconfig_does_not_move_the_user_off_their_cluster() {
        let mut state = state_with_contexts();
        state.select_cluster(ClusterId::new("staging"));

        state.apply_batch(
            &[contexts_event(&["prod", "staging"], Some("prod"))],
            Instant::now(),
        );

        assert_eq!(state.active(), Some(&ClusterId::new("staging")));
    }

    #[test]
    fn a_kubeconfig_failure_keeps_the_last_good_contexts_and_explains_itself() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[ClusterEvent::ConfigFailed {
                reason: "permission denied".into(),
            }],
            Instant::now(),
        );

        assert_eq!(state.config_error(), Some("permission denied"));
        assert_eq!(state.contexts().len(), 2);
    }

    #[test]
    fn discovered_kinds_belong_to_their_cluster() {
        let mut state = state_with_contexts();
        let kinds: Arc<[KindInfo]> = Arc::from([KindInfo {
            id: deployments(),
            namespaced: true,
            watchable: true,
            custom: false,
        }]);

        state.apply_batch(
            &[ClusterEvent::Kinds {
                cluster: "staging".into(),
                kinds: Arc::clone(&kinds),
            }],
            Instant::now(),
        );

        // Discovered for staging, so the prod picker must not show them.
        assert!(state.kinds().is_empty());
        state.select_cluster(ClusterId::new("staging"));
        assert_eq!(state.kinds().len(), 1);
        assert!(state.kind_info(&deployments()).is_some());
    }

    #[test]
    fn rows_for_the_active_cluster_and_kind_become_the_table() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[row("default", "web"), row("default", "api")],
            )],
            Instant::now(),
        );

        assert_eq!(row_names(&state), ["default/api", "default/web"]);
        assert_eq!(state.counts(), (2, 2));
        assert_eq!(&*state.columns()[0].name, "STATUS");
    }

    #[test]
    fn rows_for_another_kind_are_kept_but_not_rendered() {
        let mut state = state_with_contexts();

        let changed = state.apply_batch(
            &[reset("prod", deployments(), &[row("default", "web")])],
            Instant::now(),
        );

        // Nothing visible changed, so no repaint; the data is still there for
        // when the user switches kinds.
        assert!(!changed);
        assert!(state.rows().is_empty());
        assert_eq!(state.row_count(&deployments()), 1);
    }

    #[test]
    fn switching_kinds_shows_the_other_table_without_refetching() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "api-0")]),
                reset("prod", deployments(), &[row("default", "api")]),
            ],
            Instant::now(),
        );

        state.select_kind(deployments());
        assert_eq!(row_names(&state), ["default/api"]);

        state.select_kind(pods());
        assert_eq!(row_names(&state), ["default/api-0"]);
    }

    #[test]
    fn switching_clusters_shows_that_clusters_rows() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "prod-pod")]),
                reset("staging", pods(), &[row("default", "staging-pod")]),
            ],
            Instant::now(),
        );

        state.select_cluster(ClusterId::new("staging"));
        assert_eq!(row_names(&state), ["default/staging-pod"]);
    }

    #[test]
    fn a_namespace_filter_hides_rows_immediately() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[row("default", "api"), row("kube-system", "coredns")],
            )],
            Instant::now(),
        );

        state.set_namespace(Some(Arc::from("kube-system")));
        assert_eq!(row_names(&state), ["kube-system/coredns"]);
        // The unfiltered count stays visible, so the user can see what is hidden.
        assert_eq!(state.counts(), (2, 1));

        state.set_namespace(None);
        assert_eq!(state.counts(), (2, 2));
    }

    #[test]
    fn an_empty_namespace_filter_is_treated_as_no_filter() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "api")])],
            Instant::now(),
        );

        state.set_namespace(Some(Arc::from("")));
        assert_eq!(state.filters().namespace, None);
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn a_search_matches_name_namespace_and_cells() {
        let mut state = state_with_contexts();
        let mut failing = row("default", "worker");
        failing.cells = Arc::from([Arc::from("CrashLoopBackOff")]);

        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "api"), failing])],
            Instant::now(),
        );

        state.set_search(Some(Arc::from("crash")));
        assert_eq!(row_names(&state), ["default/worker"]);

        state.set_search(Some(Arc::from("API")));
        assert_eq!(row_names(&state), ["default/api"]);

        state.set_search(None);
        assert_eq!(state.rows().len(), 2);
    }

    #[test]
    fn namespaces_come_from_the_rows_the_table_holds() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[row("default", "api"), row("kube-system", "coredns")],
            )],
            Instant::now(),
        );

        let namespaces: Vec<_> = state
            .namespaces()
            .iter()
            .map(|namespace| namespace.to_string())
            .collect();
        assert_eq!(namespaces, ["default", "kube-system"]);
    }

    #[test]
    fn a_resync_that_changes_nothing_does_not_repaint() {
        let mut state = state_with_contexts();
        let event = reset("prod", pods(), &[row("default", "api")]);

        assert!(state.apply_batch(std::slice::from_ref(&event), Instant::now()));
        assert!(!state.apply_batch(std::slice::from_ref(&event), Instant::now()));
    }

    #[test]
    fn a_resync_clears_the_stale_marker() {
        let mut state = state_with_contexts();
        let now = Instant::now();
        state.apply_batch(
            &[ClusterEvent::Stale {
                cluster: Some("prod".into()),
                dropped: 42,
            }],
            now,
        );
        assert!(
            state
                .connection(&ClusterId::new("prod"))
                .unwrap()
                .is_stale()
        );

        state.apply_batch(&[reset("prod", pods(), &[row("default", "api")])], now);

        // The table was just rebuilt from a fresh listing, so whatever was
        // dropped no longer matters.
        assert!(
            !state
                .connection(&ClusterId::new("prod"))
                .unwrap()
                .is_stale()
        );
    }

    #[test]
    fn a_detail_fetch_is_shown_only_while_it_is_still_wanted() {
        let mut state = state_with_contexts();
        let key = ResourceKey::new("default", "api-0");
        state.open_detail(pods(), key.clone());

        let object = Arc::new(ObjectDetail {
            key: key.clone(),
            yaml: Arc::from("kind: Pod"),
            maskable: false,
            revealed: true,
            events: Arc::from([] as [periscope_bridge::EventLine; 0]),
            owners: Arc::from([] as [periscope_bridge::OwnerRef; 0]),
        });

        // An answer for something else must not replace what is on screen.
        state.apply_batch(
            &[ClusterEvent::Object {
                cluster: "prod".into(),
                kind: deployments(),
                detail: Arc::clone(&object),
            }],
            Instant::now(),
        );
        assert!(matches!(state.detail(), Some(Detail::Loading { .. })));

        state.apply_batch(
            &[ClusterEvent::Object {
                cluster: "prod".into(),
                kind: pods(),
                detail: object,
            }],
            Instant::now(),
        );
        assert!(matches!(state.detail(), Some(Detail::Ready { .. })));
    }

    #[test]
    fn a_failed_fetch_replaces_the_spinner_with_the_reason() {
        let mut state = state_with_contexts();
        let key = ResourceKey::new("default", "api-0");
        state.open_detail(pods(), key.clone());

        state.apply_batch(
            &[ClusterEvent::ObjectFailed {
                cluster: "prod".into(),
                kind: pods(),
                key,
                reason: "pods \"api-0\" not found".into(),
            }],
            Instant::now(),
        );

        match state.detail() {
            Some(Detail::Failed { reason, .. }) => assert!(reason.contains("not found")),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn changing_what_is_being_viewed_closes_the_detail_pane() {
        let mut state = state_with_contexts();
        state.open_detail(pods(), ResourceKey::new("default", "api-0"));

        state.select_kind(deployments());
        assert!(state.detail().is_none());

        state.open_detail(deployments(), ResourceKey::new("default", "api"));
        state.select_cluster(ClusterId::new("staging"));
        assert!(state.detail().is_none());
    }

    #[test]
    fn with_no_kind_selected_there_are_no_rows() {
        let mut state = AppState::new();
        state.apply_batch(&[contexts_event(&["prod"], Some("prod"))], Instant::now());

        assert_eq!(state.kind(), None);
        assert!(state.rows().is_empty());
        assert_eq!(state.counts(), (0, 0));
    }
}
