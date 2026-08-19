//! The row cache for one kind on one cluster.
//!
//! Keyed by namespace and name in a `BTreeMap`, so iteration is already in the
//! order the table renders and no sort is needed after an update. Rows are
//! `Arc`s: the UI materialises a slice of them for virtualised rendering, and
//! that clone is a refcount bump rather than a copy of every row.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{ColumnSpec, ResourceKey, ResourceRow};

/// Every object known for one kind.
#[derive(Clone, Debug, Default)]
pub struct ResourceTable {
    rows: BTreeMap<ResourceKey, Arc<ResourceRow>>,
    columns: Arc<[ColumnSpec]>,
    /// When each row last changed. Pruned as it is read, so a resync cannot
    /// leave a timestamp per object behind it.
    changed: BTreeMap<ResourceKey, Instant>,
    /// The same map, shared with the view.
    ///
    /// Rebuilt only when the set actually moves rather than once per frame: a
    /// busy cluster can have thousands of rows inside the highlight window, and
    /// copying that map sixty times a second to render thirty visible rows is
    /// the kind of cost that does not show up until somebody has a real
    /// workload.
    snapshot: Arc<BTreeMap<ResourceKey, Instant>>,
    /// Whether `snapshot` is behind `changed`.
    restale: bool,
    /// Why this kind cannot be listed, if it cannot.
    ///
    /// An RBAC refusal and an empty namespace look identical in a table of no
    /// rows, and the difference matters: one means "there are none", the other
    /// means "you are not allowed to know". Held here so the view can say which.
    unavailable: Option<Arc<str>>,
}

impl ResourceTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The columns these rows carry cells for.
    pub fn columns(&self) -> &Arc<[ColumnSpec]> {
        &self.columns
    }

    /// Why this kind cannot be listed, if it cannot.
    pub fn unavailable(&self) -> Option<&Arc<str>> {
        self.unavailable.as_ref()
    }

    /// Records that the apiserver will not serve this kind to this identity.
    ///
    /// Returns whether anything changed. The rows already held are dropped: a
    /// listing nobody is allowed to refresh goes stale silently, which is
    /// exactly the shape of bug this project refuses to ship.
    pub fn refuse(&mut self, reason: &str) -> bool {
        let reason: Arc<str> = Arc::from(reason);
        if self.unavailable.as_deref() == Some(&*reason) && self.rows.is_empty() {
            return false;
        }

        self.rows.clear();
        self.changed.clear();
        self.restale = true;
        self.unavailable = Some(reason);
        true
    }

    /// Replaces the whole table with a fresh listing.
    ///
    /// Returns whether anything the UI renders changed. A resync that finds the
    /// world unchanged — the common case after a brief watch drop — must not
    /// repaint ten thousand rows.
    pub fn reset(&mut self, columns: Arc<[ColumnSpec]>, rows: &[ResourceRow]) -> bool {
        // Whatever we were refused before, we are evidently allowed now.
        let was_refused = self.unavailable.take().is_some();
        let replacement: BTreeMap<_, _> = rows
            .iter()
            .map(|row| (row.key.clone(), Arc::new(row.clone())))
            .collect();

        let unchanged = self.columns == columns
            && replacement.len() == self.rows.len()
            && replacement
                .iter()
                .zip(self.rows.iter())
                .all(|((_, new), (_, old))| new == old);
        if unchanged {
            // The rows may match, but a table that was showing a refusal and is
            // now showing rows has changed even so.
            return was_refused;
        }

        self.columns = columns;
        self.rows = replacement;
        // Deliberately not marked as changed: a resync replaces everything, and
        // flashing every row would say "all of this just happened" when what
        // happened is that the watch reconnected.
        self.changed.clear();
        self.restale = true;
        true
    }

    /// Adds or updates one row, reporting whether it differed from what was held.
    pub fn apply(&mut self, row: Arc<ResourceRow>) -> bool {
        self.apply_at(row, Instant::now())
    }

    /// Adds or replaces a row, recording when it changed.
    ///
    /// The clock is a parameter so the marking is testable without sleeping.
    pub fn apply_at(&mut self, row: Arc<ResourceRow>, now: Instant) -> bool {
        match self.rows.get(&row.key) {
            Some(existing) if **existing == *row => false,
            _ => {
                self.changed.insert(row.key.clone(), now);
                self.restale = true;
                self.rows.insert(row.key.clone(), row);
                true
            }
        }
    }

    /// When each row last changed, for the view to mark what just moved.
    ///
    /// Only recent changes are kept — anything older than `window` is dropped
    /// as it is asked for — so this cannot grow with the table. A resync
    /// rewrites every row at once, which would otherwise leave ten thousand
    /// timestamps behind for a highlight that lasts under a second.
    pub fn changed_since(
        &mut self,
        now: Instant,
        window: Duration,
    ) -> Arc<BTreeMap<ResourceKey, Instant>> {
        let before = self.changed.len();
        self.changed
            .retain(|_, at| now.duration_since(*at) < window);

        if self.restale || self.changed.len() != before {
            self.snapshot = Arc::new(self.changed.clone());
            self.restale = false;
        }
        Arc::clone(&self.snapshot)
    }

    /// Removes a row, reporting whether it was there.
    pub fn remove(&mut self, key: &ResourceKey) -> bool {
        self.rows.remove(key).is_some()
    }

    /// Rows in table order: namespace, then name.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<ResourceRow>> {
        self.rows.values()
    }

    /// A snapshot of the rows, for indexed access by a virtualised list.
    pub fn rows(&self) -> Vec<Arc<ResourceRow>> {
        self.rows.values().cloned().collect()
    }

    /// How many rows are held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The namespaces these rows live in, in order and without duplicates.
    pub fn namespaces(&self) -> Vec<Arc<str>> {
        let mut seen: Vec<Arc<str>> = Vec::new();
        for key in self.rows.keys() {
            if key.is_namespaced() && seen.last() != Some(&key.namespace) {
                seen.push(Arc::clone(&key.namespace));
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_named(name: &str, cell: &str) -> Arc<ResourceRow> {
        Arc::new(ResourceRow {
            key: ResourceKey::new("default", name),
            uid: None,
            cells: Arc::from([Arc::from(cell)]),
            state: periscope_bridge::RowState::Healthy,
            created: None,
        })
    }

    #[test]
    fn a_changed_row_is_marked_and_then_forgotten() {
        let mut table = ResourceTable::new();
        let start = Instant::now();
        let window = Duration::from_millis(900);

        table.apply_at(row_named("api-0", "1"), start);
        assert!(
            table
                .changed_since(start, window)
                .contains_key(&ResourceKey::new("default", "api-0"))
        );

        // The mark is what makes a live change visible; it has to expire, or
        // the table ends up permanently highlighted.
        let later = start + window + Duration::from_millis(1);
        assert!(table.changed_since(later, window).is_empty());
    }

    #[test]
    fn a_row_that_did_not_change_is_not_marked() {
        // The same object arriving again is what a watch does constantly; only
        // a difference is news.
        let mut table = ResourceTable::new();
        let start = Instant::now();

        table.apply_at(row_named("api-0", "1"), start);
        table.changed_since(start + Duration::from_secs(5), Duration::from_millis(900));

        assert!(!table.apply_at(row_named("api-0", "1"), start));
        assert!(
            table
                .changed_since(start, Duration::from_millis(900))
                .is_empty()
        );
    }

    #[test]
    fn a_resync_marks_nothing() {
        // Everything arriving at once means the watch reconnected, not that
        // every object in the cluster just moved.
        let mut table = ResourceTable::new();
        let now = Instant::now();

        table.reset(
            Arc::from([ColumnSpec::fixed("DATA", 60)]),
            &[
                (*row_named("api-0", "1")).clone(),
                (*row_named("api-1", "2")).clone(),
            ],
        );

        assert!(
            table
                .changed_since(now, Duration::from_millis(900))
                .is_empty()
        );
    }

    #[test]
    fn the_shared_map_is_not_rebuilt_when_nothing_moved() {
        // It is read once per frame; rebuilding it each time would copy a map
        // of every row a busy cluster touched in the last second, sixty times
        // a second.
        let mut table = ResourceTable::new();
        let now = Instant::now();
        table.apply_at(row_named("api-0", "1"), now);

        let first = table.changed_since(now, Duration::from_secs(5));
        let second = table.changed_since(now, Duration::from_secs(5));

        assert!(Arc::ptr_eq(&first, &second));
    }

    use periscope_bridge::RowState;

    fn columns() -> Arc<[ColumnSpec]> {
        Arc::from([ColumnSpec::fixed("STATUS", 100)])
    }

    fn row(namespace: &str, name: &str, status: &str) -> ResourceRow {
        ResourceRow {
            key: ResourceKey::new(namespace, name),
            uid: None,
            cells: Arc::from([Arc::from(status)]),
            state: RowState::Healthy,
            created: None,
        }
    }

    fn names(table: &ResourceTable) -> Vec<String> {
        table.iter().map(|row| row.key.to_string()).collect()
    }

    #[test]
    fn rows_come_back_sorted_by_namespace_then_name() {
        let mut table = ResourceTable::new();
        for (namespace, name) in [
            ("kube-system", "coredns"),
            ("default", "web"),
            ("default", "api"),
        ] {
            table.apply(Arc::new(row(namespace, name, "Running")));
        }

        assert_eq!(
            names(&table),
            ["default/api", "default/web", "kube-system/coredns"]
        );
    }

    #[test]
    fn applying_an_identical_row_is_not_a_change() {
        let mut table = ResourceTable::new();
        assert!(table.apply(Arc::new(row("default", "api", "Running"))));
        // The apiserver resends objects on resync; repainting for those would
        // burn frames for nothing.
        assert!(!table.apply(Arc::new(row("default", "api", "Running"))));
        assert!(table.apply(Arc::new(row("default", "api", "CrashLoopBackOff"))));
    }

    #[test]
    fn a_reset_replaces_the_whole_table_and_its_columns() {
        let mut table = ResourceTable::new();
        table.apply(Arc::new(row("default", "gone", "Running")));

        assert!(table.reset(columns(), &[row("default", "api", "Running")]));
        assert_eq!(names(&table), ["default/api"]);
        assert_eq!(&*table.columns()[0].name, "STATUS");
    }

    #[test]
    fn a_reset_that_changes_nothing_reports_no_change() {
        let mut table = ResourceTable::new();
        table.reset(columns(), &[row("default", "api", "Running")]);

        assert!(!table.reset(columns(), &[row("default", "api", "Running")]));
        assert!(table.reset(columns(), &[row("default", "api", "Terminating")]));
    }

    #[test]
    fn switching_to_a_kind_with_different_columns_is_a_change() {
        let mut table = ResourceTable::new();
        table.reset(columns(), &[row("default", "api", "Running")]);

        // Same rows, different headings: rendering the old headings over the
        // new cells would silently mislabel every column.
        let other: Arc<[ColumnSpec]> = Arc::from([ColumnSpec::fixed("PHASE", 100)]);
        assert!(table.reset(other, &[row("default", "api", "Running")]));
    }

    #[test]
    fn a_reset_to_nothing_empties_the_table() {
        let mut table = ResourceTable::new();
        table.apply(Arc::new(row("default", "api", "Running")));

        assert!(table.reset(columns(), &[]));
        assert!(table.is_empty());
        assert!(!table.reset(columns(), &[]));
    }

    #[test]
    fn removing_reports_whether_the_row_was_known() {
        let mut table = ResourceTable::new();
        table.apply(Arc::new(row("default", "api", "Running")));

        assert!(table.remove(&ResourceKey::new("default", "api")));
        assert!(!table.remove(&ResourceKey::new("default", "api")));
        assert!(table.is_empty());
    }

    #[test]
    fn namespaces_are_listed_once_each_in_order() {
        let mut table = ResourceTable::new();
        for (namespace, name) in [
            ("kube-system", "coredns"),
            ("default", "web"),
            ("default", "api"),
        ] {
            table.apply(Arc::new(row(namespace, name, "Running")));
        }

        let namespaces: Vec<_> = table
            .namespaces()
            .iter()
            .map(|namespace| namespace.to_string())
            .collect();
        assert_eq!(namespaces, ["default", "kube-system"]);
    }

    #[test]
    fn cluster_scoped_rows_contribute_no_namespaces() {
        let mut table = ResourceTable::new();
        table.apply(Arc::new(row("", "node-1", "Ready")));
        assert!(table.namespaces().is_empty());
    }

    #[test]
    fn a_ten_thousand_row_listing_stays_ordered_and_addressable() {
        let mut table = ResourceTable::new();
        let rows: Vec<_> = (0..10_000)
            .map(|i| row("default", &format!("worker-{i:05}"), "Running"))
            .collect();

        assert!(table.reset(columns(), &rows));
        assert_eq!(table.len(), 10_000);

        let rows = table.rows();
        assert_eq!(&*rows[0].key.name, "worker-00000");
        assert_eq!(&*rows[9_999].key.name, "worker-09999");
    }
}
