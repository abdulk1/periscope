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
    ClusterEvent, ClusterId, ColumnSpec, ConnectionState, ContextInfo, ExecStatus, ExecTarget,
    ForwardId, ForwardInfo, KindId, KindInfo, LogTarget, Mutation, MutationOutcome, ObjectDetail,
    ResourceKey, ResourceRow,
};

use crate::connections::{Connection, ConnectionRegistry};
use crate::logs::{FilterSpec, LogBuffer, LogOrder};
use crate::permissions::{Authorized, AuthorizedExec, Permissions, Refusal};
use crate::table::ResourceTable;

/// A live log tail.
#[derive(Debug)]
pub struct LogSession {
    /// Which cluster the lines come from.
    pub cluster: ClusterId,
    /// What is being tailed.
    pub target: Arc<LogTarget>,
    /// The lines, bounded and filtered.
    pub buffer: LogBuffer,
    /// Whether the view sticks to the bottom as lines arrive.
    pub following: bool,
}

/// A command running in a container, and what it has said so far.
///
/// The output reuses [`LogBuffer`], so it is bounded and filterable exactly
/// like a log tail: a command that prints a gigabyte cannot take the app down,
/// and `grep` over its output is already written.
#[derive(Debug)]
pub struct ExecSession {
    /// Which cluster the command is running in.
    pub cluster: ClusterId,
    /// What is being run, and where.
    pub target: Arc<ExecTarget>,
    /// The output, bounded and filtered.
    pub buffer: LogBuffer,
    /// How it ended, once it has. `None` means it is still running.
    pub status: Option<ExecStatus>,
}

impl ExecSession {
    /// Whether the command is still running.
    pub fn is_running(&self) -> bool {
        self.status.is_none()
    }

    /// The line under the output: what is happening, or how it ended.
    pub fn summary(&self) -> String {
        match &self.status {
            Some(status) => status.message(),
            None => "running…".to_owned(),
        }
    }
}

/// What the detail pane is showing.
///
/// Every variant names the cluster as well as the kind and the object. Two
/// clusters routinely hold objects with the same namespace and name — that is
/// what `kube-root-ca.crt` and `default/api-0` are — so an answer matched on
/// kind and key alone can be the right object from the wrong cluster. In a
/// split view that put one cluster's YAML under the other's heading, and the
/// reveal path is the same path, so it could do it with a Secret's values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    /// A fetch is in flight.
    Loading {
        /// Which cluster it was asked of.
        cluster: ClusterId,
        /// Kind of the object being fetched.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
    },
    /// The object, its events and its owners.
    Ready {
        /// Which cluster it came from.
        cluster: ClusterId,
        /// Kind of the object.
        kind: KindId,
        /// What was fetched.
        object: Arc<ObjectDetail>,
    },
    /// The fetch failed. Shown in place of the object, never as a blank pane.
    Failed {
        /// Which cluster it was asked of.
        cluster: ClusterId,
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

    /// The cluster it belongs to.
    pub fn cluster(&self) -> &ClusterId {
        match self {
            Self::Loading { cluster, .. }
            | Self::Failed { cluster, .. }
            | Self::Ready { cluster, .. } => cluster,
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

/// What a table is sorted by.
///
/// The structural columns are named rather than indexed because they are not
/// cells: a row's namespace, name and age come from its key and its creation
/// time, and only the kind-specific columns are cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// Namespace, then name.
    Namespace,
    /// Name. The order rows arrive in, and the one to return to.
    #[default]
    Name,
    /// Age, youngest or oldest first.
    Age,
    /// One of the kind's own columns.
    Cell(usize),
}

/// How a table is sorted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    /// What it is sorted by.
    pub key: SortKey,
    /// Whether the order is reversed.
    pub descending: bool,
}

impl Sort {
    /// The order a click on a column produces.
    ///
    /// A new column starts ascending; the same column reverses; a third click
    /// puts it back to name order, so there is always a way out of a sort
    /// rather than only ever a different one.
    pub fn clicked(self, key: SortKey) -> Self {
        match (self.key == key, self.descending) {
            (false, _) => Self {
                key,
                descending: false,
            },
            (true, false) => Self {
                key,
                descending: true,
            },
            (true, true) => Self::default(),
        }
    }

    /// The arrow a column header shows, if it is the one being sorted by.
    pub fn marker(self, key: SortKey) -> Option<&'static str> {
        (self.key == key).then_some(if self.descending { "▾" } else { "▴" })
    }
}

/// Compares two cells, numerically when both are numbers.
///
/// Sorting `RESTARTS` as text puts 10 before 2, which is the kind of wrong that
/// makes people stop trusting the column.
fn compare_cells(left: &str, right: &str) -> std::cmp::Ordering {
    match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.total_cmp(&right),
        _ => left.to_lowercase().cmp(&right.to_lowercase()),
    }
}

/// Sorts rows in place.
///
/// Rows arrive sorted by namespace and name — that is how the table holds them
/// — so the name order is the natural one and needs no work. Every other order
/// falls back to the name when the keys are equal, which keeps rows from
/// jumping around each other as a watch stream updates them.
fn sort_rows(rows: &mut [Arc<ResourceRow>], sort: Sort) {
    match sort.key {
        SortKey::Name if !sort.descending => return,
        SortKey::Name => rows.reverse(),
        SortKey::Namespace => rows.sort_by(|left, right| {
            left.key
                .namespace
                .cmp(&right.key.namespace)
                .then_with(|| left.key.name.cmp(&right.key.name))
        }),
        SortKey::Age => rows.sort_by(|left, right| {
            // Missing creation times sort last whichever way the column goes:
            // an unknown age is not older or younger, it is unknown.
            match (left.created, right.created) {
                (Some(left_at), Some(right_at)) => left_at
                    .cmp(&right_at)
                    .then_with(|| left.key.name.cmp(&right.key.name)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => left.key.name.cmp(&right.key.name),
            }
        }),
        SortKey::Cell(column) => rows.sort_by(|left, right| {
            compare_cells(left.cell(column), right.cell(column))
                .then_with(|| left.key.name.cmp(&right.key.name))
        }),
    }

    if sort.descending && !matches!(sort.key, SortKey::Name) {
        rows.reverse();
    }
}

/// One view onto one cluster.
///
/// Two of these side by side is what "view two clusters at once" means; the
/// tables themselves are shared, so a pane costs a selection and a materialised
/// row list, not a second copy of the data.
#[derive(Debug, Default)]
pub struct Pane {
    cluster: Option<ClusterId>,
    kind: Option<KindId>,
    filters: Filters,
    /// Materialised rows for this pane, for indexed access by the virtualised
    /// list. Rebuilt only when something it shows changes, and shared as an
    /// `Arc` so a render that captures them costs one refcount bump rather
    /// than a copy of ten thousand pointers.
    rows: Arc<[Arc<ResourceRow>]>,
    columns: Arc<[ColumnSpec]>,
    /// Which row the keyboard is on.
    ///
    /// An index rather than a key, because the thing being pointed at is a
    /// position in a list somebody is moving through — and it is clamped
    /// whenever the rows change, so a row vanishing under the cursor moves it
    /// rather than leaving it pointing past the end.
    cursor: usize,
    /// What this pane's table is sorted by.
    sort: Sort,
    /// Why this pane's kind cannot be listed, if it cannot.
    unavailable: Option<Arc<str>>,
}

impl Pane {
    /// The cluster this pane shows.
    pub fn cluster(&self) -> Option<&ClusterId> {
        self.cluster.as_ref()
    }

    /// The kind this pane shows.
    pub fn kind(&self) -> Option<&KindId> {
        self.kind.as_ref()
    }

    /// The filters applied to it.
    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    /// Which row the keyboard is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// What its table is sorted by.
    pub fn sort(&self) -> Sort {
        self.sort
    }

    /// The row the keyboard is on, if there is one.
    pub fn current_row(&self) -> Option<&Arc<ResourceRow>> {
        self.rows.get(self.cursor)
    }

    /// The rows it renders.
    pub fn rows(&self) -> &[Arc<ResourceRow>] {
        &self.rows
    }

    /// The same rows, shared, for a virtualised list.
    pub fn rows_shared(&self) -> Arc<[Arc<ResourceRow>]> {
        Arc::clone(&self.rows)
    }

    /// The columns those rows carry cells for.
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// Why this kind cannot be listed here, if it cannot.
    ///
    /// Set when the apiserver refuses the kind rather than the credential —
    /// RBAC, almost always. The table must say so: no rows and "you may not
    /// see these" are different facts and only one of them is reassuring.
    pub fn unavailable(&self) -> Option<&Arc<str>> {
        self.unavailable.as_ref()
    }

    /// Whether this pane shows a given cluster and kind.
    fn shows(&self, cluster: &ClusterId, kind: &KindId) -> bool {
        self.cluster.as_ref() == Some(cluster) && self.kind.as_ref() == Some(kind)
    }
}

/// How many rows one cluster may hold across every kind before the oldest
/// tables are released.
///
/// Twenty times the largest cluster this has been tested against. The point is
/// not to be tight: it is that one enormous cluster cannot consume every byte
/// the process has and starve the others.
pub const DEFAULT_ROW_BUDGET: usize = 200_000;

/// How long a cluster nobody is looking at keeps streaming before it is let go.
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The application's state.
#[derive(Debug)]
pub struct AppState {
    connections: ConnectionRegistry,
    contexts: Vec<ContextInfo>,
    current: Option<ClusterId>,
    config_error: Option<String>,
    kinds: BTreeMap<ClusterId, Arc<[KindInfo]>>,
    tables: BTreeMap<(ClusterId, KindId), ResourceTable>,
    detail: Option<Detail>,
    logs: Option<LogSession>,
    /// The command that is running, or the last one that ran.
    exec: Option<ExecSession>,
    /// One pane, or two side by side.
    panes: Vec<Pane>,
    /// Which pane commands apply to.
    focus: usize,
    /// When each cluster was last in a pane, for the idle sweep.
    last_viewed: BTreeMap<ClusterId, Instant>,
    /// Rows one cluster may hold before its unviewed tables are released.
    budget: usize,
    /// Which clusters may be changed.
    permissions: Permissions,
    /// What recent mutations did, newest first.
    activity: Vec<Activity>,
    /// Port forwards, by cluster and id.
    forwards: BTreeMap<(ClusterId, ForwardId), Arc<ForwardInfo>>,
    /// The id the next forward gets.
    next_forward: u64,
}

/// One thing Periscope did to a cluster, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Which cluster.
    pub cluster: ClusterId,
    /// What was asked for.
    pub mutation: Arc<Mutation>,
    /// What happened.
    pub outcome: MutationOutcome,
    /// When it was recorded.
    pub at: Instant,
}

/// How many recent actions the UI keeps.
const ACTIVITY_LIMIT: usize = 50;

impl Default for AppState {
    fn default() -> Self {
        Self {
            connections: ConnectionRegistry::default(),
            contexts: Vec::new(),
            current: None,
            config_error: None,
            kinds: BTreeMap::new(),
            tables: BTreeMap::new(),
            detail: None,
            logs: None,
            exec: None,
            panes: vec![Pane::default()],
            focus: 0,
            last_viewed: BTreeMap::new(),
            budget: DEFAULT_ROW_BUDGET,
            permissions: Permissions::permissive(),
            activity: Vec::new(),
            forwards: BTreeMap::new(),
            next_forward: 1,
        }
    }
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
                if self.panes[0].cluster.is_none()
                    && let Some(current) = current.clone()
                {
                    self.last_viewed.insert(current.clone(), now);
                    self.panes[0].cluster = Some(current);
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
                self.enforce_budget(cluster);
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
                cluster,
                kind,
                detail: object,
            } => {
                // Ignore an answer to a question the user has moved on from —
                // including one from a cluster they have moved away from.
                if self.awaiting(cluster, kind, &object.key) {
                    self.detail = Some(Detail::Ready {
                        cluster: cluster.clone(),
                        kind: kind.clone(),
                        object: Arc::clone(object),
                    });
                    changed = true;
                }
            }

            ClusterEvent::KindFailed {
                cluster,
                kind,
                reason,
            } => {
                let touched = self.table_mut(cluster, kind).refuse(reason);
                changed |= self.touch(cluster, kind, touched);
            }

            ClusterEvent::ObjectFailed {
                cluster,
                kind,
                key,
                reason,
            } => {
                if self.awaiting(cluster, kind, key) {
                    self.detail = Some(Detail::Failed {
                        cluster: cluster.clone(),
                        kind: kind.clone(),
                        key: key.clone(),
                        reason: reason.clone(),
                    });
                    changed = true;
                }
            }

            ClusterEvent::LogBatch { cluster, lines } => {
                if let Some(session) = self.logs.as_mut().filter(|s| &s.cluster == cluster) {
                    session.buffer.extend(lines);
                    changed = true;
                }
            }

            ClusterEvent::LogSourceChanged {
                cluster,
                source,
                state,
            } => {
                if let Some(session) = self.logs.as_mut().filter(|s| &s.cluster == cluster) {
                    session.buffer.source_changed(source.clone(), state.clone());
                    changed = true;
                }
            }

            ClusterEvent::LogsFailed { cluster, reason } => {
                if let Some(session) = self.logs.as_mut().filter(|s| &s.cluster == cluster) {
                    session.buffer.fail(reason.clone());
                    changed = true;
                }
            }

            ClusterEvent::MutationDone {
                cluster,
                mutation,
                outcome,
            } => {
                self.record_activity(Activity {
                    cluster: cluster.clone(),
                    mutation: Arc::clone(mutation),
                    outcome: outcome.clone(),
                    at: now,
                });
                changed = true;
            }

            ClusterEvent::ForwardChanged { cluster, forward } => {
                let key = (cluster.clone(), forward.id);
                match forward.state {
                    // A forward that has stopped is gone from the list; one
                    // that failed stays, because the reason is the point.
                    periscope_bridge::ForwardState::Stopped => {
                        self.forwards.remove(&key);
                    }
                    _ => {
                        self.forwards.insert(key, Arc::clone(forward));
                    }
                }
                changed = true;
            }

            ClusterEvent::ExecOutput { cluster, lines } => {
                if let Some(session) = self.exec.as_mut().filter(|s| &s.cluster == cluster) {
                    session.buffer.extend(lines);
                    changed = true;
                }
            }

            ClusterEvent::ExecFinished { cluster, status } => {
                if let Some(session) = self.exec.as_mut().filter(|s| &s.cluster == cluster) {
                    session.status = Some(status.clone());
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
        let mut stale: Vec<usize> = Vec::new();

        for event in events {
            changed |= self.apply(event, now);
            for pane in self.affected_panes(event) {
                if !stale.contains(&pane) {
                    stale.push(pane);
                }
            }
        }

        for pane in stale {
            self.refresh_pane(pane);
        }
        changed
    }

    /// Switches which cluster the focused pane shows.
    pub fn select_cluster(&mut self, cluster: ClusterId) {
        if self.panes[self.focus].cluster.as_ref() == Some(&cluster) {
            return;
        }
        self.last_viewed.insert(cluster.clone(), Instant::now());
        self.panes[self.focus].cluster = Some(cluster);
        // A different cluster is a different table; keeping the cursor at row
        // 500 of the last one points it at something unrelated.
        self.panes[self.focus].cursor = 0;
        self.detail = None;
        // A tail — or a command's output — belongs to the cluster it was opened
        // on; carrying it across would leave lines on screen that no longer
        // relate to anything.
        self.logs = None;
        self.exec = None;
        self.refresh_pane(self.focus);
    }

    /// Switches which kind the focused pane shows.
    pub fn select_kind(&mut self, kind: KindId) {
        if self.panes[self.focus].kind.as_ref() == Some(&kind) {
            return;
        }
        self.panes[self.focus].kind = Some(kind);
        self.panes[self.focus].cursor = 0;
        self.detail = None;
        self.refresh_pane(self.focus);
    }

    /// Applies a namespace filter to the focused pane, or clears it.
    pub fn set_namespace(&mut self, namespace: Option<Arc<str>>) {
        self.panes[self.focus].filters.namespace =
            namespace.filter(|namespace| !namespace.is_empty());
        self.refresh_pane(self.focus);
    }

    /// Applies a label selector to the focused pane, or clears it.
    pub fn set_selector(&mut self, selector: Option<Arc<str>>) {
        self.panes[self.focus].filters.selector = selector.filter(|selector| !selector.is_empty());
        self.refresh_pane(self.focus);
    }

    /// Applies the client-side text filter to the focused pane.
    pub fn set_search(&mut self, search: Option<Arc<str>>) {
        self.panes[self.focus].filters.search = search.filter(|search| !search.is_empty());
        self.refresh_pane(self.focus);
    }

    /// The filters on the focused pane.
    pub fn filters(&self) -> &Filters {
        &self.panes[self.focus].filters
    }

    // --- panes --------------------------------------------------------------

    /// Every pane, left to right.
    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    /// Which pane commands apply to.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Focuses a pane. Out-of-range indexes are ignored rather than panicking
    /// mid-render.
    pub fn focus_pane(&mut self, index: usize) {
        if index < self.panes.len() && index != self.focus {
            self.focus = index;
            self.detail = None;
            if let Some(cluster) = self.panes[index].cluster.clone() {
                self.last_viewed.insert(cluster, Instant::now());
            }
        }
    }

    /// Adds a second pane, showing the same cluster until it is pointed
    /// somewhere else. Returns whether the layout changed.
    pub fn split(&mut self) -> bool {
        if self.panes.len() > 1 {
            return false;
        }

        let mut pane = Pane {
            cluster: self.panes[0].cluster.clone(),
            kind: self.panes[0].kind.clone(),
            ..Pane::default()
        };
        // Start the new pane on a different cluster when there is one: opening
        // two identical panes is never what "split" means.
        if let Some(other) = self
            .contexts
            .iter()
            .map(|context| ClusterId::new(&*context.name))
            .find(|candidate| Some(candidate) != self.panes[0].cluster.as_ref())
        {
            pane.cluster = Some(other);
        }

        self.panes.push(pane);
        self.focus = 1;
        if let Some(cluster) = self.panes[1].cluster.clone() {
            self.last_viewed.insert(cluster, Instant::now());
        }
        self.refresh_pane(1);
        true
    }

    /// Drops the second pane. Returns whether the layout changed.
    pub fn unsplit(&mut self) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        self.panes.truncate(1);
        self.focus = 0;
        self.detail = None;
        true
    }

    /// Whether two panes are open.
    pub fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    /// The clusters currently in a pane.
    pub fn clusters_in_view(&self) -> Vec<ClusterId> {
        let mut clusters: Vec<ClusterId> = self
            .panes
            .iter()
            .filter_map(|pane| pane.cluster.clone())
            .collect();
        clusters.dedup();
        clusters
    }

    /// Everything a pane needs watched: its cluster, kind and server-side
    /// filters.
    pub fn watch_targets(&self) -> Vec<(ClusterId, KindId, Filters)> {
        self.panes
            .iter()
            .filter_map(|pane| {
                Some((
                    pane.cluster.clone()?,
                    pane.kind.clone()?,
                    pane.filters.clone(),
                ))
            })
            .collect()
    }

    // --- mutations ----------------------------------------------------------

    /// When each visible row of the focused pane last changed.
    ///
    /// The view uses it to mark what just moved: a watch stream rewrites rows
    /// under the reader with nothing else to say that anything happened.
    pub fn changed_recently(
        &mut self,
        pane: usize,
        now: Instant,
        window: std::time::Duration,
    ) -> Arc<BTreeMap<ResourceKey, Instant>> {
        let Some(pane) = self.panes.get(pane) else {
            return Arc::default();
        };
        let (Some(cluster), Some(kind)) = (pane.cluster.clone(), pane.kind.clone()) else {
            return Arc::default();
        };

        self.tables
            .get_mut(&(cluster, kind))
            .map(|table| table.changed_since(now, window))
            .unwrap_or_default()
    }

    // --- keyboard cursor ----------------------------------------------------

    /// Where the keyboard is in the focused pane's table.
    pub fn cursor(&self) -> usize {
        self.panes[self.focus].cursor()
    }

    /// The row the keyboard is on, if the table has any.
    pub fn current_row(&self) -> Option<&Arc<ResourceRow>> {
        self.panes[self.focus].current_row()
    }

    /// Moves the cursor, reporting whether it went anywhere.
    ///
    /// It stops at both ends rather than wrapping: in a list of ten thousand
    /// pods, wrapping from the top to the bottom is never what was meant, and
    /// it loses your place.
    pub fn move_cursor(&mut self, by: isize) -> bool {
        let pane = &mut self.panes[self.focus];
        if pane.rows.is_empty() {
            return false;
        }

        let last = pane.rows.len() - 1;
        let moved = pane.cursor.saturating_add_signed(by).min(last);
        if moved == pane.cursor {
            return false;
        }

        pane.cursor = moved;
        true
    }

    /// Puts the cursor on a specific row, clamped to what exists.
    pub fn set_cursor(&mut self, index: usize) -> bool {
        let pane = &mut self.panes[self.focus];
        let clamped = index.min(pane.rows.len().saturating_sub(1));
        if clamped == pane.cursor {
            return false;
        }

        pane.cursor = clamped;
        true
    }

    /// Moves the cursor half a screen, the way `ctrl-d` and `ctrl-u` do.
    ///
    /// Half rather than a whole screen because the point of the key is to keep
    /// your place: a full page replaces everything you were reading, and there
    /// is nothing left on screen to tell you where you landed.
    ///
    /// `visible` is how many rows fit, which only the view knows and only after
    /// it has been laid out once. Zero means "not measured yet", and a key that
    /// does nothing at all until the first frame has been painted would be a
    /// worse answer than moving one row.
    pub fn move_cursor_half_page(&mut self, down: bool, visible: usize) -> bool {
        let half = (visible / 2).max(1) as isize;
        self.move_cursor(if down { half } else { -half })
    }

    /// Puts the cursor on the last row.
    pub fn cursor_to_end(&mut self) -> bool {
        let last = self.panes[self.focus].rows.len().saturating_sub(1);
        self.set_cursor(last)
    }

    /// Sets the policy, from settings.
    pub fn set_permissions(&mut self, permissions: Permissions) {
        self.permissions = permissions;
    }

    /// The policy in force.
    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    /// Whether a cluster may be changed at all, for the UI to grey buttons out.
    ///
    /// Greying out is a courtesy, not the enforcement: [`AppState::authorize`]
    /// is what actually refuses.
    pub fn may_mutate(&self, cluster: &ClusterId) -> bool {
        self.permissions.may_mutate(cluster)
    }

    /// Authorises a mutation, or says why not.
    ///
    /// Nothing else in the app can construct an [`Authorized`], so a mutation
    /// that was never checked cannot be sent.
    pub fn authorize(
        &self,
        cluster: &ClusterId,
        mutation: Arc<Mutation>,
    ) -> Result<Authorized, Refusal> {
        if !self.permissions.may_mutate(cluster) {
            return Err(Refusal::ReadOnly {
                cluster: cluster.clone(),
            });
        }

        // Acting on a cluster the app never connected to would fail at the API
        // with something unhelpful; refuse it here, where the reason is known.
        let connected = self.connections.get(cluster).is_some_and(|connection| {
            matches!(
                connection.state,
                ConnectionState::Connected | ConnectionState::Degraded { .. }
            )
        });
        if !connected {
            return Err(Refusal::NotConnected {
                cluster: cluster.clone(),
            });
        }

        Ok(Authorized::new(cluster.clone(), mutation))
    }

    /// Authorises a command, or says why not.
    ///
    /// Held to the same rule as a mutation: `kubectl exec` needs `create` on
    /// `pods/exec`, and a command can change anything a container can reach, so
    /// a read-only cluster refuses one.
    pub fn authorize_exec(
        &self,
        cluster: &ClusterId,
        target: Arc<ExecTarget>,
    ) -> Result<AuthorizedExec, Refusal> {
        if !self.permissions.may_mutate(cluster) {
            return Err(Refusal::ReadOnly {
                cluster: cluster.clone(),
            });
        }

        let connected = self.connections.get(cluster).is_some_and(|connection| {
            matches!(
                connection.state,
                ConnectionState::Connected | ConnectionState::Degraded { .. }
            )
        });
        if !connected {
            return Err(Refusal::NotConnected {
                cluster: cluster.clone(),
            });
        }

        Ok(AuthorizedExec::new(cluster.clone(), target))
    }

    /// Records a refusal so it shows up in the activity list like anything
    /// else. A refusal the user cannot see is indistinguishable from a bug.
    pub fn record_refusal(
        &mut self,
        cluster: ClusterId,
        mutation: Arc<Mutation>,
        refusal: &Refusal,
        now: Instant,
    ) {
        self.record_activity(Activity {
            cluster,
            mutation,
            outcome: MutationOutcome::Refused {
                reason: refusal.reason(),
            },
            at: now,
        });
    }

    fn record_activity(&mut self, activity: Activity) {
        self.activity.insert(0, activity);
        self.activity.truncate(ACTIVITY_LIMIT);
    }

    /// What Periscope has done recently, newest first.
    pub fn activity(&self) -> &[Activity] {
        &self.activity
    }

    /// The most recent action, for the status line.
    pub fn last_activity(&self) -> Option<&Activity> {
        self.activity.first()
    }

    // --- forwards -----------------------------------------------------------

    /// Takes the next forward id.
    pub fn next_forward_id(&mut self) -> ForwardId {
        let id = ForwardId(self.next_forward);
        self.next_forward += 1;
        id
    }

    /// Every forward, in the order they were started.
    pub fn forwards(&self) -> impl ExactSizeIterator<Item = (&ClusterId, &Arc<ForwardInfo>)> {
        self.forwards
            .iter()
            .map(|((cluster, _), forward)| (cluster, forward))
    }

    /// How many forwards are open.
    pub fn forward_count(&self) -> usize {
        self.forwards.len()
    }

    /// Forwards that need attention.
    pub fn broken_forwards(&self) -> usize {
        self.forwards
            .values()
            .filter(|forward| forward.state.is_problem())
            .count()
    }

    /// Forgets a forward the UI has torn down, without waiting for the event.
    pub fn forget_forward(&mut self, cluster: &ClusterId, id: ForwardId) {
        self.forwards.remove(&(cluster.clone(), id));
    }

    // --- warm clusters ------------------------------------------------------

    /// Records that a cluster is being looked at.
    pub fn touch_cluster(&mut self, cluster: &ClusterId, now: Instant) {
        self.last_viewed.insert(cluster.clone(), now);
    }

    /// Clusters that have been out of view longer than `timeout`.
    ///
    /// Warm means "still streaming so that going back is instant"; past the
    /// timeout the streaming is just cost, and the rows are dropped with it.
    pub fn idle_clusters(&self, now: Instant, timeout: std::time::Duration) -> Vec<ClusterId> {
        let in_view = self.clusters_in_view();
        self.last_viewed
            .iter()
            .filter(|(cluster, _)| !in_view.contains(cluster))
            .filter(|(_, seen)| now.saturating_duration_since(**seen) >= timeout)
            .map(|(cluster, _)| cluster.clone())
            .collect()
    }

    /// Drops everything held for a cluster, keeping its connection state.
    pub fn release(&mut self, cluster: &ClusterId) {
        self.tables.retain(|(held, _), _| held != cluster);
        self.last_viewed.remove(cluster);
        for index in 0..self.panes.len() {
            if self.panes[index].cluster.as_ref() == Some(cluster) {
                self.refresh_pane(index);
            }
        }
    }

    /// Rows held for one cluster, across every kind.
    pub fn cluster_rows(&self, cluster: &ClusterId) -> usize {
        self.tables
            .iter()
            .filter(|((held, _), _)| held == cluster)
            .map(|(_, table)| table.len())
            .sum()
    }

    /// Rows held for every cluster.
    pub fn total_rows(&self) -> usize {
        self.tables.values().map(ResourceTable::len).sum()
    }

    /// The per-cluster row budget.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Sets the per-cluster row budget.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget.max(1);
    }

    /// Every row held, with the cluster and kind it belongs to.
    ///
    /// This is what cross-cluster search reads: it deliberately includes
    /// clusters that are not in a pane, because "find this pod" is most useful
    /// when you cannot remember which cluster it is on.
    pub fn all_rows(&self) -> impl Iterator<Item = (&ClusterId, &KindId, &Arc<ResourceRow>)> {
        self.tables
            .iter()
            .flat_map(|((cluster, kind), table)| table.iter().map(move |row| (cluster, kind, row)))
    }

    /// Opens a log session, replacing any that was running.
    pub fn open_logs(&mut self, cluster: ClusterId, target: Arc<LogTarget>, capacity: usize) {
        self.logs = Some(LogSession {
            cluster,
            target,
            buffer: LogBuffer::new(capacity),
            following: true,
        });
    }

    /// Closes the log session.
    pub fn close_logs(&mut self) {
        self.logs = None;
    }

    /// The log session, if one is open.
    pub fn logs(&self) -> Option<&LogSession> {
        self.logs.as_ref()
    }

    /// Applies a filter to the open session, reporting whether it changed.
    pub fn set_log_filter(&mut self, spec: FilterSpec) -> bool {
        self.logs
            .as_mut()
            .is_some_and(|session| session.buffer.set_filter(spec))
    }

    /// Orders the open session's lines, reporting whether it changed.
    pub fn set_log_order(&mut self, order: LogOrder) -> bool {
        self.logs
            .as_mut()
            .is_some_and(|session| session.buffer.set_order(order))
    }

    /// Sticks the view to the bottom, or lets it stay put.
    pub fn set_following(&mut self, following: bool) {
        if let Some(session) = self.logs.as_mut() {
            session.following = following;
        }
    }

    /// Opens a command session, replacing any that was running.
    pub fn open_exec(&mut self, cluster: ClusterId, target: Arc<ExecTarget>, capacity: usize) {
        self.exec = Some(ExecSession {
            cluster,
            target,
            buffer: LogBuffer::new(capacity),
            status: None,
        });
    }

    /// Closes the command session.
    pub fn close_exec(&mut self) {
        self.exec = None;
    }

    /// The command session, if one is open.
    pub fn exec(&self) -> Option<&ExecSession> {
        self.exec.as_ref()
    }

    /// Marks the running command as cancelled, if one is running.
    ///
    /// The cluster layer reports this too, but only when it still had the task;
    /// saying it here means the pane never sits on "running…" after the user
    /// pressed stop.
    pub fn cancel_exec(&mut self) -> bool {
        match self.exec.as_mut().filter(|session| session.is_running()) {
            Some(session) => {
                session.status = Some(ExecStatus::Cancelled);
                true
            }
            None => false,
        }
    }

    /// Applies a filter to the command's output, reporting whether it changed.
    pub fn set_exec_filter(&mut self, spec: FilterSpec) -> bool {
        self.exec
            .as_mut()
            .is_some_and(|session| session.buffer.set_filter(spec))
    }

    /// Records that a detail fetch is in flight.
    pub fn open_detail(&mut self, cluster: ClusterId, kind: KindId, key: ResourceKey) {
        self.detail = Some(Detail::Loading { cluster, kind, key });
    }

    /// Closes the detail pane.
    pub fn close_detail(&mut self) {
        self.detail = None;
    }

    /// What the detail pane is showing, if anything.
    pub fn detail(&self) -> Option<&Detail> {
        self.detail.as_ref()
    }

    /// The cluster the focused pane shows.
    pub fn active(&self) -> Option<&ClusterId> {
        self.panes[self.focus].cluster.as_ref()
    }

    /// The kind the focused pane shows.
    pub fn kind(&self) -> Option<&KindId> {
        self.panes[self.focus].kind.as_ref()
    }

    /// The kinds the focused pane's cluster serves.
    pub fn kinds(&self) -> &[KindInfo] {
        self.kinds_of(self.active())
    }

    /// The kinds a cluster serves.
    pub fn kinds_of(&self, cluster: Option<&ClusterId>) -> &[KindInfo] {
        cluster
            .and_then(|cluster| self.kinds.get(cluster))
            .map(|kinds| &**kinds)
            .unwrap_or_default()
    }

    /// Looks up what discovery said about a kind on the active cluster.
    pub fn kind_info(&self, kind: &KindId) -> Option<&KindInfo> {
        self.kinds().iter().find(|info| &info.id == kind)
    }

    /// The rows the focused pane renders, filtered and in namespace-then-name
    /// order.
    pub fn rows(&self) -> &[Arc<ResourceRow>] {
        self.panes[self.focus].rows()
    }

    /// The same rows, shared, for a virtualised list that must own what it
    /// renders from.
    pub fn rows_shared(&self) -> Arc<[Arc<ResourceRow>]> {
        self.panes[self.focus].rows_shared()
    }

    /// The columns those rows carry cells for.
    pub fn columns(&self) -> &[ColumnSpec] {
        self.panes[self.focus].columns()
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

    /// Connection health for the cluster the focused pane shows.
    pub fn active_connection(&self) -> Option<&Connection> {
        self.active().and_then(|id| self.connections.get(id))
    }

    /// How many rows the active view holds before and after filtering.
    pub fn counts(&self) -> (usize, usize) {
        let total = self
            .active_table()
            .map_or(0, |(_, table): (_, &ResourceTable)| table.len());
        (total, self.rows().len())
    }

    /// How many rows a kind holds on the focused pane's cluster, for the picker.
    pub fn row_count(&self, kind: &KindId) -> usize {
        self.active()
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
        self.pane_table(self.focus)
    }

    fn pane_table(&self, index: usize) -> Option<(&KindId, &ResourceTable)> {
        let pane = self.panes.get(index)?;
        let cluster = pane.cluster.as_ref()?;
        let kind = pane.kind.as_ref()?;
        let table = self.tables.get(&(cluster.clone(), kind.clone()))?;
        Some((kind, table))
    }

    /// Releases a cluster's least useful tables when it is over budget.
    ///
    /// The kinds a pane is showing are never released — dropping what is on
    /// screen to save memory would be absurd — so a single enormous table can
    /// exceed the budget. What the budget prevents is a cluster accumulating
    /// every kind the user has ever visited while another cluster needs the
    /// memory.
    fn enforce_budget(&mut self, cluster: &ClusterId) {
        if self.cluster_rows(cluster) <= self.budget {
            return;
        }

        let shown: Vec<KindId> = self
            .panes
            .iter()
            .filter(|pane| pane.cluster.as_ref() == Some(cluster))
            .filter_map(|pane| pane.kind.clone())
            .collect();

        let mut releasable: Vec<(KindId, usize)> = self
            .tables
            .iter()
            .filter(|((held, kind), _)| held == cluster && !shown.contains(kind))
            .map(|((_, kind), table)| (kind.clone(), table.len()))
            .collect();
        // Biggest first: the point is to get back under budget in as few
        // releases as possible.
        releasable.sort_by_key(|(_, rows)| std::cmp::Reverse(*rows));

        for (kind, _) in releasable {
            if self.cluster_rows(cluster) <= self.budget {
                break;
            }
            tracing::debug!(%cluster, %kind, "releasing a table to stay inside the row budget");
            self.tables.remove(&(cluster.clone(), kind));
        }
    }

    fn table_mut(&mut self, cluster: &ClusterId, kind: &KindId) -> &mut ResourceTable {
        self.tables
            .entry((cluster.clone(), kind.clone()))
            .or_default()
    }

    /// Whether a detail event answers the fetch that is actually in flight.
    fn awaiting(&self, cluster: &ClusterId, kind: &KindId, key: &ResourceKey) -> bool {
        self.detail.as_ref().is_some_and(|detail| {
            detail.cluster() == cluster && detail.kind() == kind && detail.key() == key
        })
    }

    /// Reports a table change, and whether any pane can see it.
    fn touch(&mut self, cluster: &ClusterId, kind: &KindId, changed: bool) -> bool {
        changed && self.panes.iter().any(|pane| pane.shows(cluster, kind))
    }

    /// Which panes an event changes the contents of.
    fn affected_panes(&self, event: &ClusterEvent) -> Vec<usize> {
        let kind = match event {
            ClusterEvent::ResourceReset { kind, .. }
            | ClusterEvent::ResourceApplied { kind, .. }
            | ClusterEvent::ResourceDeleted { kind, .. }
            // A refusal empties the table, which is a change to what the pane
            // renders exactly like a delete is.
            | ClusterEvent::KindFailed { kind, .. } => kind,
            _ => return Vec::new(),
        };
        let Some(cluster) = event.cluster() else {
            return Vec::new();
        };

        self.panes
            .iter()
            .enumerate()
            .filter(|(_, pane)| pane.shows(cluster, kind))
            .map(|(index, _)| index)
            .collect()
    }

    /// What the focused pane is sorted by.
    pub fn sort(&self) -> Sort {
        self.panes[self.focus].sort
    }

    /// Sorts the focused pane by a column, cycling through the orders.
    pub fn sort_by(&mut self, key: SortKey) {
        self.panes[self.focus].sort = self.panes[self.focus].sort.clicked(key);
        // A sort is a different order, not a different row: the cursor goes
        // back to the top rather than staying on an index that now means
        // something else.
        self.panes[self.focus].cursor = 0;
        self.refresh_pane(self.focus);
    }

    fn refresh_pane(&mut self, index: usize) {
        let Some((_, table)) = self.pane_table(index) else {
            if let Some(pane) = self.panes.get_mut(index) {
                pane.rows = Arc::from([] as [Arc<ResourceRow>; 0]);
                pane.columns = Arc::from([] as [ColumnSpec; 0]);
                pane.unavailable = None;
            }
            return;
        };

        let columns = Arc::clone(table.columns());
        let unavailable = table.unavailable().map(Arc::clone);
        let filters = self.panes[index].filters.clone();
        let rows: Vec<_> = table
            .iter()
            // The namespace filter is applied by the apiserver too, but the
            // rows already held must follow it immediately rather than waiting
            // for the re-listing to arrive.
            .filter(|row| {
                filters
                    .namespace
                    .as_deref()
                    .is_none_or(|namespace| &*row.key.namespace == namespace)
            })
            .filter(|row| filters.matches(row))
            .cloned()
            .collect();

        let mut rows = rows;
        let sort = self.panes[index].sort;
        sort_rows(&mut rows, sort);

        let pane = &mut self.panes[index];
        pane.columns = columns;
        pane.unavailable = unavailable;
        pane.rows = Arc::from(rows);
        // A row can disappear under the cursor at any moment — that is what a
        // watch stream does — so the cursor follows the list rather than the
        // list being assumed to hold still.
        pane.cursor = pane.cursor.min(pane.rows.len().saturating_sub(1));
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
        state.open_detail(ClusterId::new("prod"), pods(), key.clone());

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
    fn a_forbidden_kind_says_so_instead_of_looking_empty() {
        // "No pods here" and "you may not list pods" are different facts, and
        // only one of them means the namespace is safe to walk away from.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "api-0")])],
            Instant::now(),
        );
        assert_eq!(row_names(&state).len(), 1);

        state.apply_batch(
            &[ClusterEvent::KindFailed {
                cluster: "prod".into(),
                kind: pods(),
                reason: "pods is forbidden: User \"dev\" cannot list resource".to_owned(),
            }],
            Instant::now(),
        );

        // The rows are gone — nobody may refresh them, so keeping them would be
        // showing a listing that quietly stops being true.
        assert!(row_names(&state).is_empty());
        let reason = state.panes()[0]
            .unavailable()
            .expect("the pane says why it is empty");
        assert!(reason.contains("forbidden"), "{reason}");

        // And a kind that becomes readable again stops explaining itself.
        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "api-0")])],
            Instant::now(),
        );
        assert!(state.panes()[0].unavailable().is_none());
        assert_eq!(row_names(&state).len(), 1);
    }

    #[test]
    fn a_forbidden_kind_does_not_report_the_cluster_as_broken() {
        // The point of the whole split: one denied kind must leave the
        // connection exactly as it was.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
            Instant::now(),
        );

        state.apply_batch(
            &[ClusterEvent::KindFailed {
                cluster: "prod".into(),
                kind: pods(),
                reason: "secrets is forbidden".to_owned(),
            }],
            Instant::now(),
        );

        let connection = state
            .connection(&ClusterId::new("prod"))
            .expect("the cluster is still known");
        assert_eq!(connection.state, ConnectionState::Connected);
    }

    #[test]
    fn a_failed_fetch_replaces_the_spinner_with_the_reason() {
        let mut state = state_with_contexts();
        let key = ResourceKey::new("default", "api-0");
        state.open_detail(ClusterId::new("prod"), pods(), key.clone());

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
        state.open_detail(
            ClusterId::new("prod"),
            pods(),
            ResourceKey::new("default", "api-0"),
        );

        state.select_kind(deployments());
        assert!(state.detail().is_none());

        state.open_detail(
            ClusterId::new("prod"),
            deployments(),
            ResourceKey::new("default", "api"),
        );
        state.select_cluster(ClusterId::new("staging"));
        assert!(state.detail().is_none());
    }

    #[test]
    fn log_lines_land_in_the_open_session() {
        use periscope_bridge::{LogLine, LogSource, LogSourceState};

        let mut state = state_with_contexts();
        state.open_logs(
            ClusterId::new("prod"),
            Arc::new(LogTarget::pod("default", "api-0")),
            crate::logs::DEFAULT_CAPACITY,
        );

        let batch = ClusterEvent::LogBatch {
            cluster: "prod".into(),
            lines: Arc::from([LogLine {
                source: LogSource::new("api-0", "api"),
                timestamp: None,
                text: Arc::from("hello"),
            }]),
        };
        assert!(state.apply_batch(std::slice::from_ref(&batch), Instant::now()));
        assert_eq!(state.logs().unwrap().buffer.visible_len(), 1);

        state.apply_batch(
            &[ClusterEvent::LogSourceChanged {
                cluster: "prod".into(),
                source: LogSource::new("api-0", "api"),
                state: LogSourceState::Streaming,
            }],
            Instant::now(),
        );
        assert_eq!(state.logs().unwrap().buffer.streaming(), 1);
    }

    #[test]
    fn lines_for_another_cluster_are_ignored() {
        let mut state = state_with_contexts();
        state.open_logs(
            ClusterId::new("prod"),
            Arc::new(LogTarget::pod("default", "api-0")),
            crate::logs::DEFAULT_CAPACITY,
        );

        let changed = state.apply_batch(
            &[ClusterEvent::LogBatch {
                cluster: "staging".into(),
                lines: Arc::from([periscope_bridge::LogLine {
                    source: periscope_bridge::LogSource::new("api-0", "api"),
                    timestamp: None,
                    text: Arc::from("not mine"),
                }]),
            }],
            Instant::now(),
        );

        assert!(!changed);
        assert_eq!(state.logs().unwrap().buffer.visible_len(), 0);
    }

    #[test]
    fn switching_clusters_closes_the_tail() {
        let mut state = state_with_contexts();
        state.open_logs(
            ClusterId::new("prod"),
            Arc::new(LogTarget::pod("default", "api-0")),
            crate::logs::DEFAULT_CAPACITY,
        );

        state.select_cluster(ClusterId::new("staging"));
        assert!(state.logs().is_none());
    }

    #[test]
    fn a_failed_session_says_why() {
        let mut state = state_with_contexts();
        state.open_logs(
            ClusterId::new("prod"),
            Arc::new(LogTarget::pod("default", "api-0")),
            crate::logs::DEFAULT_CAPACITY,
        );

        state.apply_batch(
            &[ClusterEvent::LogsFailed {
                cluster: "prod".into(),
                reason: "container \"api\" in pod \"api-0\" is waiting to start".into(),
            }],
            Instant::now(),
        );

        assert!(
            state
                .logs()
                .unwrap()
                .buffer
                .error()
                .is_some_and(|reason| reason.contains("waiting to start"))
        );
    }

    #[test]
    fn splitting_opens_a_second_pane_on_another_cluster() {
        let mut state = state_with_contexts();

        assert!(!state.is_split());
        assert!(state.split());
        assert!(state.is_split());

        // Two panes showing the same cluster is never what split means.
        assert_eq!(state.panes()[0].cluster(), Some(&ClusterId::new("prod")));
        assert_eq!(state.panes()[1].cluster(), Some(&ClusterId::new("staging")));
        // Commands go to the new pane.
        assert_eq!(state.focus(), 1);
        assert_eq!(state.active(), Some(&ClusterId::new("staging")));

        assert!(!state.split(), "splitting twice does nothing");
        assert!(state.unsplit());
        assert_eq!(state.focus(), 0);
        assert_eq!(state.active(), Some(&ClusterId::new("prod")));
    }

    #[test]
    fn each_pane_keeps_its_own_kind_and_filters() {
        let mut state = state_with_contexts();
        state.split();

        state.focus_pane(0);
        state.select_kind(pods());
        state.set_namespace(Some(Arc::from("payments")));

        state.focus_pane(1);
        state.select_kind(deployments());

        assert_eq!(state.panes()[0].kind(), Some(&pods()));
        assert_eq!(
            state.panes()[0].filters().namespace.as_deref(),
            Some("payments")
        );
        assert_eq!(state.panes()[1].kind(), Some(&deployments()));
        assert_eq!(state.panes()[1].filters().namespace, None);
    }

    #[test]
    fn both_panes_update_from_one_batch() {
        let mut state = state_with_contexts();
        state.split();
        state.focus_pane(0);
        state.select_kind(pods());
        state.focus_pane(1);
        state.select_kind(pods());

        let changed = state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "prod-a")]),
                reset(
                    "staging",
                    pods(),
                    &[row("default", "staging-a"), row("default", "staging-b")],
                ),
            ],
            Instant::now(),
        );

        assert!(changed);
        assert_eq!(state.panes()[0].rows().len(), 1);
        assert_eq!(state.panes()[1].rows().len(), 2);
    }

    #[test]
    fn a_cluster_no_pane_shows_still_holds_its_rows() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "prod-a")]),
                reset("staging", pods(), &[row("default", "staging-a")]),
            ],
            Instant::now(),
        );

        // Switching away and back must not re-fetch: that is what "warm" means.
        state.select_cluster(ClusterId::new("staging"));
        assert_eq!(state.rows().len(), 1);
        state.select_cluster(ClusterId::new("prod"));
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.total_rows(), 2);
    }

    #[test]
    fn a_cluster_out_of_view_goes_idle_and_is_released() {
        let mut state = state_with_contexts();
        let now = Instant::now();
        state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "prod-a")]),
                reset("staging", pods(), &[row("default", "staging-a")]),
            ],
            now,
        );
        state.touch_cluster(&ClusterId::new("staging"), now);

        let timeout = std::time::Duration::from_secs(300);
        // Nothing is idle while it is still being looked at.
        assert!(state.idle_clusters(now, timeout).is_empty());

        let later = now + std::time::Duration::from_secs(301);
        let idle = state.idle_clusters(later, timeout);
        assert_eq!(idle, vec![ClusterId::new("staging")]);
        // The cluster in a pane is never idle, however long it has been open.
        assert!(!idle.contains(&ClusterId::new("prod")));

        state.release(&ClusterId::new("staging"));
        assert_eq!(state.cluster_rows(&ClusterId::new("staging")), 0);
        assert_eq!(state.cluster_rows(&ClusterId::new("prod")), 1);
    }

    #[test]
    fn releasing_a_cluster_a_pane_shows_empties_that_pane() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "api")])],
            Instant::now(),
        );
        assert_eq!(state.rows().len(), 1);

        state.release(&ClusterId::new("prod"));
        assert!(state.rows().is_empty());
    }

    #[test]
    fn a_cluster_over_budget_releases_the_tables_no_pane_is_showing() {
        let mut state = state_with_contexts();
        state.set_budget(10);

        let many: Vec<_> = (0..8)
            .map(|index| row("default", &format!("dep-{index}")))
            .collect();
        state.apply_batch(&[reset("prod", deployments(), &many)], Instant::now());
        assert_eq!(state.cluster_rows(&ClusterId::new("prod")), 8);

        // Pods are what the pane shows, so pods survive and the deployments
        // table is what gets released.
        let pods_rows: Vec<_> = (0..6)
            .map(|index| row("default", &format!("pod-{index}")))
            .collect();
        state.apply_batch(&[reset("prod", pods(), &pods_rows)], Instant::now());

        assert_eq!(state.row_count(&pods()), 6);
        assert_eq!(state.row_count(&deployments()), 0);
        assert!(state.cluster_rows(&ClusterId::new("prod")) <= state.budget());
    }

    #[test]
    fn one_cluster_over_budget_does_not_touch_another() {
        let mut state = state_with_contexts();
        state.set_budget(5);

        state.apply_batch(
            &[reset("staging", pods(), &[row("default", "keep-me")])],
            Instant::now(),
        );

        let many: Vec<_> = (0..20)
            .map(|index| row("default", &format!("dep-{index}")))
            .collect();
        state.apply_batch(&[reset("prod", deployments(), &many)], Instant::now());

        // The budget is per cluster: staging's row is untouched.
        assert_eq!(state.cluster_rows(&ClusterId::new("staging")), 1);
    }

    #[test]
    fn cross_cluster_search_sees_every_cluster_held() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                reset("prod", pods(), &[row("default", "api-0")]),
                reset("staging", deployments(), &[row("default", "api")]),
            ],
            Instant::now(),
        );

        let found: Vec<_> = state
            .all_rows()
            .map(|(cluster, kind, row)| format!("{cluster}/{}/{}", kind.label(), row.key.name))
            .collect();

        assert!(found.contains(&"prod/pods/api-0".to_owned()), "{found:?}");
        assert!(
            found.contains(&"staging/deployments.apps/api".to_owned()),
            "{found:?}"
        );
    }

    #[test]
    fn watch_targets_cover_every_pane() {
        let mut state = state_with_contexts();
        state.select_kind(pods());
        state.split();
        state.select_kind(deployments());

        let targets = state.watch_targets();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].0, ClusterId::new("prod"));
        assert_eq!(targets[0].1, pods());
        assert_eq!(targets[1].0, ClusterId::new("staging"));
        assert_eq!(targets[1].1, deployments());
    }

    fn connected(state: &mut AppState, cluster: &str) {
        state.apply_batch(
            &[ClusterEvent::Status {
                cluster: cluster.into(),
                state: ConnectionState::Connected,
            }],
            Instant::now(),
        );
    }

    fn delete_api() -> Arc<Mutation> {
        Arc::new(Mutation::Delete {
            kind: pods(),
            key: ResourceKey::new("default", "api-0"),
            grace_period: None,
        })
    }

    #[test]
    fn the_built_in_limits_match_what_settings_default_to() {
        // Two copies of these numbers exist — these constants, which apply when
        // nobody configured anything, and `periscope_config::Limits`, which the
        // settings file fills in. They must agree, or the documented default
        // and the actual default are different numbers.
        let configured = periscope_config::Limits::default();

        assert_eq!(configured.row_budget, DEFAULT_ROW_BUDGET);
        assert_eq!(configured.idle_timeout.get(), DEFAULT_IDLE_TIMEOUT);
        assert_eq!(configured.log_buffer, crate::logs::DEFAULT_CAPACITY);
    }

    /// A row carrying a cell value and an age.
    fn row_with(name: &str, cell: &str, created: Option<std::time::SystemTime>) -> ResourceRow {
        ResourceRow {
            key: ResourceKey::new("default", name),
            uid: None,
            cells: Arc::from([Arc::from(cell)]),
            state: RowState::Healthy,
            created,
        }
    }

    fn names(state: &AppState) -> Vec<String> {
        state
            .rows()
            .iter()
            .map(|row| row.key.name.to_string())
            .collect()
    }

    #[test]
    fn a_column_sorts_numerically_when_it_holds_numbers() {
        // Sorting RESTARTS as text puts 10 before 2, which is the kind of wrong
        // that makes people stop trusting a column.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row_with("a", "2", None),
                    row_with("b", "10", None),
                    row_with("c", "1", None),
                ],
            )],
            Instant::now(),
        );

        state.sort_by(SortKey::Cell(0));
        assert_eq!(names(&state), ["c", "a", "b"]);
    }

    #[test]
    fn clicking_a_column_cycles_ascending_descending_and_back() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row_with("a", "2", None),
                    row_with("b", "10", None),
                    row_with("c", "1", None),
                ],
            )],
            Instant::now(),
        );

        state.sort_by(SortKey::Cell(0));
        assert_eq!(names(&state), ["c", "a", "b"]);

        state.sort_by(SortKey::Cell(0));
        assert!(state.sort().descending);
        assert_eq!(names(&state), ["b", "a", "c"]);

        // A third click is the way out, back to the order rows arrive in.
        state.sort_by(SortKey::Cell(0));
        assert_eq!(state.sort(), Sort::default());
        assert_eq!(names(&state), ["a", "b", "c"]);
    }

    #[test]
    fn sorting_by_age_puts_unknown_ages_last_either_way() {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let new = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9_000);
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row_with("young", "0", Some(new)),
                    row_with("unknown", "0", None),
                    row_with("old", "0", Some(old)),
                ],
            )],
            Instant::now(),
        );

        state.sort_by(SortKey::Age);
        assert_eq!(names(&state), ["old", "young", "unknown"]);

        state.sort_by(SortKey::Age);
        // Reversed among the known ones; the unknown is still not claiming to
        // be the oldest or the youngest.
        assert_eq!(names(&state), ["unknown", "young", "old"]);
    }

    #[test]
    fn a_sort_survives_new_rows_arriving() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[row_with("a", "2", None), row_with("b", "1", None)],
            )],
            Instant::now(),
        );
        state.sort_by(SortKey::Cell(0));
        assert_eq!(names(&state), ["b", "a"]);

        state.apply_batch(
            &[ClusterEvent::ResourceApplied {
                cluster: "prod".into(),
                kind: pods(),
                row: Arc::new(row_with("c", "0", None)),
            }],
            Instant::now(),
        );

        assert_eq!(names(&state), ["c", "b", "a"]);
    }

    #[test]
    fn sorting_by_a_column_the_next_kind_does_not_have_falls_back_to_the_name() {
        // The sort belongs to the pane, not to the kind, so switching kinds
        // carries `Cell(3)` onto a table whose rows have one cell. Every cell
        // reads empty, every comparison ties, and the tie-break is the name —
        // the only order left that means anything.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                reset(
                    "prod",
                    pods(),
                    &[row_with("a", "1", None), row_with("b", "2", None)],
                ),
                reset(
                    "prod",
                    deployments(),
                    &[
                        row_with("zebra", "x", None),
                        row_with("aardvark", "y", None),
                    ],
                ),
            ],
            Instant::now(),
        );

        state.sort_by(SortKey::Cell(3));
        state.select_kind(deployments());

        assert_eq!(state.sort().key, SortKey::Cell(3));
        assert_eq!(names(&state), ["aardvark", "zebra"]);
    }

    #[test]
    fn a_filter_that_matches_nothing_empties_the_table_and_takes_the_cursor_with_it() {
        // A cursor left pointing at row 2 of a list with no rows is how
        // `current_row` starts handing out something that is not on screen.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row("default", "api-0"),
                    row("default", "api-1"),
                    row("default", "api-2"),
                ],
            )],
            Instant::now(),
        );
        state.cursor_to_end();
        assert_eq!(state.cursor(), 2);

        state.set_search(Some(Arc::from("nothing-is-called-this")));
        assert!(state.rows().is_empty());
        assert_eq!(state.cursor(), 0);
        assert!(state.current_row().is_none());

        // Clearing it brings the rows back; the cursor stays where the empty
        // table left it rather than pretending to remember.
        state.set_search(None);
        assert_eq!(row_names(&state).len(), 3);
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn sorting_puts_the_cursor_back_at_the_top() {
        // The row under it means something different once the order changes.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[row_with("a", "2", None), row_with("b", "1", None)],
            )],
            Instant::now(),
        );
        state.cursor_to_end();

        state.sort_by(SortKey::Cell(0));
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn the_cursor_moves_through_the_rows_and_stops_at_both_ends() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row("default", "a"),
                    row("default", "b"),
                    row("default", "c"),
                ],
            )],
            Instant::now(),
        );

        assert_eq!(state.cursor(), 0);
        assert!(state.move_cursor(1));
        assert_eq!(
            state
                .current_row()
                .map(|row| row.key.name.to_string())
                .as_deref(),
            Some("b")
        );

        assert!(state.cursor_to_end());
        assert_eq!(state.cursor(), 2);
        // Stopping rather than wrapping: in ten thousand pods, wrapping loses
        // your place and is never what was meant.
        assert!(!state.move_cursor(1));
        assert_eq!(state.cursor(), 2);

        assert!(state.move_cursor(-10));
        assert_eq!(state.cursor(), 0);
        assert!(!state.move_cursor(-1));
    }

    #[test]
    fn a_row_disappearing_under_the_cursor_moves_it_rather_than_stranding_it() {
        // Exactly what a watch stream does while somebody is reading.
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row("default", "a"),
                    row("default", "b"),
                    row("default", "c"),
                ],
            )],
            Instant::now(),
        );
        state.cursor_to_end();
        assert_eq!(state.cursor(), 2);

        state.apply_batch(
            &[reset("prod", pods(), &[row("default", "a")])],
            Instant::now(),
        );

        assert_eq!(state.cursor(), 0);
        assert_eq!(
            state
                .current_row()
                .map(|row| row.key.name.to_string())
                .as_deref(),
            Some("a")
        );
    }

    #[test]
    fn changing_what_the_table_shows_puts_the_cursor_back_at_the_top() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row("default", "a"),
                    row("default", "b"),
                    row("default", "c"),
                ],
            )],
            Instant::now(),
        );
        state.cursor_to_end();
        assert_eq!(state.cursor(), 2);

        // A different kind is a different table: row 3 of pods says nothing
        // about row 3 of deployments.
        state.select_kind(deployments());
        assert_eq!(state.cursor(), 0);
    }

    #[test]
    fn half_a_page_is_half_of_what_fits_on_screen() {
        let mut state = state_with_contexts();
        let rows: Vec<ResourceRow> = (0..100)
            .map(|index| row("default", &index.to_string()))
            .collect();
        state.apply_batch(&[reset("prod", pods(), &rows)], Instant::now());

        assert!(state.move_cursor_half_page(true, 20));
        assert_eq!(state.cursor(), 10);
        assert!(state.move_cursor_half_page(false, 20));
        assert_eq!(state.cursor(), 0);

        // Before the first layout nothing has been measured, and a key that
        // did nothing at all would read as broken.
        assert!(state.move_cursor_half_page(true, 0));
        assert_eq!(state.cursor(), 1);

        // It stops at the ends like every other cursor move.
        assert!(state.move_cursor_half_page(true, 1_000));
        assert_eq!(state.cursor(), 99);
        assert!(!state.move_cursor_half_page(true, 20));
    }

    #[test]
    fn an_empty_table_has_nowhere_to_move_and_says_so() {
        let mut state = state_with_contexts();

        assert!(!state.move_cursor(1));
        assert!(state.current_row().is_none());
    }

    #[test]
    fn each_pane_keeps_its_own_place() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[reset(
                "prod",
                pods(),
                &[
                    row("default", "a"),
                    row("default", "b"),
                    row("default", "c"),
                ],
            )],
            Instant::now(),
        );
        state.cursor_to_end();

        state.split();
        state.focus_pane(1);
        // A fresh pane starts at the top, not wherever the other one was.
        assert_eq!(state.cursor(), 0);

        state.focus_pane(0);
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn an_object_from_another_cluster_never_lands_in_the_detail_pane() {
        // Two clusters routinely hold the same namespace and name — that is
        // what `default/api-0` and `kube-root-ca.crt` are. Matching an answer on
        // kind and key alone showed one cluster's object, and one cluster's
        // revealed Secret, under the other's heading.
        let mut state = state_with_contexts();
        let key = ResourceKey::new("default", "api-0");
        state.open_detail(ClusterId::new("prod"), pods(), key.clone());

        let intruder = ClusterEvent::Object {
            cluster: "staging".into(),
            kind: pods(),
            detail: Arc::new(ObjectDetail {
                key: key.clone(),
                yaml: Arc::from("kind: Pod\n"),
                maskable: false,
                revealed: false,
                events: Arc::from([]),
                owners: Arc::from([]),
            }),
        };
        state.apply(&intruder, Instant::now());

        assert!(
            matches!(state.detail(), Some(Detail::Loading { .. })),
            "an answer from another cluster replaced the pane"
        );
    }

    #[test]
    fn a_failure_from_another_cluster_does_not_replace_the_pane_either() {
        let mut state = state_with_contexts();
        let key = ResourceKey::new("default", "api-0");
        state.open_detail(ClusterId::new("prod"), pods(), key.clone());

        state.apply(
            &ClusterEvent::ObjectFailed {
                cluster: "staging".into(),
                kind: pods(),
                key,
                reason: "not found".to_owned(),
            },
            Instant::now(),
        );

        assert!(matches!(state.detail(), Some(Detail::Loading { .. })));
    }

    #[test]
    fn a_read_only_cluster_rejects_mutations_at_the_store_layer() {
        // The Phase 5 acceptance criterion, in one test: the refusal happens
        // here, not in the UI, and not only in the cluster layer.
        let mut state = state_with_contexts();
        connected(&mut state, "prod");

        let mut permissions = crate::Permissions::permissive();
        permissions.deny(ClusterId::new("prod"));
        state.set_permissions(permissions);

        let refusal = state
            .authorize(&ClusterId::new("prod"), delete_api())
            .expect_err("a read-only cluster must refuse");

        assert_eq!(
            refusal,
            crate::Refusal::ReadOnly {
                cluster: ClusterId::new("prod")
            }
        );
        assert!(!state.may_mutate(&ClusterId::new("prod")));
        assert!(
            refusal.reason().contains("read-only"),
            "{}",
            refusal.reason()
        );
    }

    fn command(command: &str) -> Arc<ExecTarget> {
        Arc::new(ExecTarget::parse("default", "api-0", None, command).expect("a command to run"))
    }

    fn exec_line(text: &str) -> periscope_bridge::LogLine {
        periscope_bridge::LogLine {
            source: periscope_bridge::LogSource::new("api-0", "stdout"),
            timestamp: None,
            text: Arc::from(text),
        }
    }

    #[test]
    fn a_read_only_cluster_refuses_to_run_a_command() {
        // Running a command is a write: `kubectl exec` needs `create` on
        // `pods/exec`, and `rm -rf` is a perfectly ordinary command.
        let mut state = state_with_contexts();
        connected(&mut state, "prod");

        let mut permissions = crate::Permissions::permissive();
        permissions.deny(ClusterId::new("prod"));
        state.set_permissions(permissions);

        let refusal = state
            .authorize_exec(&ClusterId::new("prod"), command("rm -rf /data"))
            .expect_err("a read-only cluster must refuse");

        assert_eq!(
            refusal,
            crate::Refusal::ReadOnly {
                cluster: ClusterId::new("prod")
            }
        );
    }

    #[test]
    fn a_command_on_a_cluster_that_is_not_connected_is_refused_before_the_api_can() {
        let state = state_with_contexts();

        assert_eq!(
            state
                .authorize_exec(&ClusterId::new("prod"), command("ls"))
                .expect_err("nothing to act on"),
            crate::Refusal::NotConnected {
                cluster: ClusterId::new("prod")
            }
        );
    }

    #[test]
    fn a_writable_cluster_authorizes_a_command() {
        let mut state = state_with_contexts();
        connected(&mut state, "prod");

        let authorized = state
            .authorize_exec(&ClusterId::new("prod"), command("ls -la"))
            .expect("a writable, connected cluster authorizes");

        assert_eq!(authorized.cluster(), &ClusterId::new("prod"));
        assert_eq!(authorized.target().command_line(), "ls -la");
    }

    #[test]
    fn command_output_lands_in_the_session_and_the_exit_status_ends_it() {
        let mut state = state_with_contexts();
        state.open_exec(ClusterId::new("prod"), command("ls"), 100);
        assert!(state.exec().expect("open").is_running());

        state.apply(
            &ClusterEvent::ExecOutput {
                cluster: "prod".into(),
                lines: Arc::from([exec_line("bin"), exec_line("etc")]),
            },
            Instant::now(),
        );
        state.apply(
            &ClusterEvent::ExecFinished {
                cluster: "prod".into(),
                status: periscope_bridge::ExecStatus::Exited {
                    code: Some(0),
                    message: String::new(),
                },
            },
            Instant::now(),
        );

        let session = state.exec().expect("still open after the command ended");
        assert_eq!(session.buffer.len(), 2);
        assert!(!session.is_running());
        assert_eq!(session.summary(), "exited 0");
    }

    #[test]
    fn output_from_another_cluster_never_reaches_the_session() {
        // Two clusters can be on screen; output belongs to exactly one of them.
        let mut state = state_with_contexts();
        state.open_exec(ClusterId::new("prod"), command("ls"), 100);

        state.apply(
            &ClusterEvent::ExecOutput {
                cluster: "staging".into(),
                lines: Arc::from([exec_line("not yours")]),
            },
            Instant::now(),
        );

        assert!(state.exec().expect("open").buffer.is_empty());
    }

    #[test]
    fn cancelling_says_so_rather_than_leaving_it_running_forever() {
        let mut state = state_with_contexts();
        state.open_exec(ClusterId::new("prod"), command("sleep 600"), 100);

        assert!(state.cancel_exec());
        assert_eq!(state.exec().expect("open").summary(), "cancelled");
        // Nothing is running any more, so a second stop is a no-op.
        assert!(!state.cancel_exec());
    }

    #[test]
    fn switching_cluster_drops_a_command_that_belonged_to_the_old_one() {
        let mut state = state_with_contexts();
        state.open_exec(ClusterId::new("prod"), command("ls"), 100);

        state.select_cluster(ClusterId::new("staging"));

        assert!(state.exec().is_none());
    }

    #[test]
    fn a_writable_cluster_authorizes() {
        let mut state = state_with_contexts();
        connected(&mut state, "prod");

        let authorized = state
            .authorize(&ClusterId::new("prod"), delete_api())
            .expect("a writable, connected cluster authorizes");

        assert_eq!(authorized.cluster(), &ClusterId::new("prod"));
        assert_eq!(authorized.mutation().verb(), "delete");
    }

    #[test]
    fn a_cluster_that_is_not_connected_refuses_before_the_api_can() {
        let state = state_with_contexts();

        let refusal = state
            .authorize(&ClusterId::new("prod"), delete_api())
            .expect_err("nothing to act on");
        assert_eq!(
            refusal,
            crate::Refusal::NotConnected {
                cluster: ClusterId::new("prod")
            }
        );
    }

    #[test]
    fn read_only_by_default_refuses_everything_not_named() {
        let mut state = state_with_contexts();
        connected(&mut state, "prod");
        connected(&mut state, "staging");
        state.set_permissions(crate::Permissions::read_only_except([ClusterId::new(
            "staging",
        )]));

        assert!(
            state
                .authorize(&ClusterId::new("prod"), delete_api())
                .is_err()
        );
        assert!(
            state
                .authorize(&ClusterId::new("staging"), delete_api())
                .is_ok()
        );
    }

    #[test]
    fn a_refusal_is_recorded_where_the_user_can_see_it() {
        let mut state = state_with_contexts();
        connected(&mut state, "prod");
        let mut permissions = crate::Permissions::permissive();
        permissions.deny(ClusterId::new("prod"));
        state.set_permissions(permissions);

        let mutation = delete_api();
        let refusal = state
            .authorize(&ClusterId::new("prod"), Arc::clone(&mutation))
            .expect_err("refused");
        state.record_refusal(ClusterId::new("prod"), mutation, &refusal, Instant::now());

        let last = state.last_activity().expect("recorded");
        assert!(last.outcome.is_problem());
        assert!(last.outcome.message().contains("read-only"));
    }

    #[test]
    fn outcomes_arriving_from_the_cluster_join_the_activity_list() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[ClusterEvent::MutationDone {
                cluster: "prod".into(),
                mutation: delete_api(),
                outcome: MutationOutcome::Applied {
                    detail: "pod \"api-0\" deleted".into(),
                },
            }],
            Instant::now(),
        );

        let last = state.last_activity().expect("recorded");
        assert!(!last.outcome.is_problem());
        assert_eq!(last.outcome.message(), "pod \"api-0\" deleted");
        assert_eq!(state.activity().len(), 1);
    }

    #[test]
    fn the_activity_list_keeps_the_newest_actions_first_and_is_bounded() {
        let mut state = state_with_contexts();
        for index in 0..60 {
            state.apply_batch(
                &[ClusterEvent::MutationDone {
                    cluster: "prod".into(),
                    mutation: Arc::new(Mutation::Scale {
                        kind: deployments(),
                        key: ResourceKey::new("default", "api"),
                        replicas: index,
                        current: None,
                    }),
                    outcome: MutationOutcome::Applied {
                        detail: format!("scaled to {index}"),
                    },
                }],
                Instant::now(),
            );
        }

        assert_eq!(state.activity().len(), ACTIVITY_LIMIT);
        assert_eq!(state.activity()[0].outcome.message(), "scaled to 59");
    }

    fn forward_event(
        cluster: &str,
        id: u64,
        state: periscope_bridge::ForwardState,
    ) -> ClusterEvent {
        ClusterEvent::ForwardChanged {
            cluster: cluster.into(),
            forward: Arc::new(ForwardInfo {
                state,
                ..ForwardInfo::starting(
                    ForwardId(id),
                    Arc::new(periscope_bridge::ForwardTarget::new(
                        "default", "api-0", 8080,
                    )),
                )
            }),
        }
    }

    #[test]
    fn forward_ids_are_unique() {
        let mut state = AppState::new();
        assert_ne!(state.next_forward_id(), state.next_forward_id());
    }

    #[test]
    fn a_forward_appears_and_shows_its_address() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Listening { local_port: 51234 },
            )],
            Instant::now(),
        );

        assert_eq!(state.forward_count(), 1);
        let (cluster, forward) = state.forwards().next().unwrap();
        assert_eq!(cluster, &ClusterId::new("prod"));
        assert_eq!(forward.address().as_deref(), Some("127.0.0.1:51234"));
        assert_eq!(state.broken_forwards(), 0);
    }

    #[test]
    fn a_failed_forward_stays_in_the_list_with_its_reason() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Failed {
                    reason: "address already in use".into(),
                },
            )],
            Instant::now(),
        );

        // Removing it would hide the only explanation of what went wrong.
        assert_eq!(state.forward_count(), 1);
        assert_eq!(state.broken_forwards(), 1);
    }

    #[test]
    fn a_stopped_forward_leaves_the_list() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Listening { local_port: 51234 },
            )],
            Instant::now(),
        );

        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Stopped,
            )],
            Instant::now(),
        );
        assert_eq!(state.forward_count(), 0);
    }

    #[test]
    fn a_forward_that_recovers_stops_being_a_problem() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Degraded {
                    local_port: 51234,
                    reason: "connection refused".into(),
                },
            )],
            Instant::now(),
        );
        assert_eq!(state.broken_forwards(), 1);

        state.apply_batch(
            &[forward_event(
                "prod",
                1,
                periscope_bridge::ForwardState::Listening { local_port: 51234 },
            )],
            Instant::now(),
        );
        assert_eq!(state.broken_forwards(), 0);
        assert_eq!(state.forward_count(), 1);
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
