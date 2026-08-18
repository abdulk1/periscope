//! The row cache for one kind on one cluster.
//!
//! Keyed by namespace and name in a `BTreeMap`, so iteration is already in the
//! order the table renders and no sort is needed after an update. Rows are
//! `Arc`s: the UI materialises a slice of them for virtualised rendering, and
//! that clone is a refcount bump rather than a copy of every row.

use std::collections::BTreeMap;
use std::sync::Arc;

use periscope_bridge::{ColumnSpec, ResourceKey, ResourceRow};

/// Every object known for one kind.
#[derive(Clone, Debug, Default)]
pub struct ResourceTable {
    rows: BTreeMap<ResourceKey, Arc<ResourceRow>>,
    columns: Arc<[ColumnSpec]>,
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

    /// Replaces the whole table with a fresh listing.
    ///
    /// Returns whether anything the UI renders changed. A resync that finds the
    /// world unchanged — the common case after a brief watch drop — must not
    /// repaint ten thousand rows.
    pub fn reset(&mut self, columns: Arc<[ColumnSpec]>, rows: &[ResourceRow]) -> bool {
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
            return false;
        }

        self.columns = columns;
        self.rows = replacement;
        true
    }

    /// Adds or updates one row, reporting whether it differed from what was held.
    pub fn apply(&mut self, row: Arc<ResourceRow>) -> bool {
        match self.rows.get(&row.key) {
            Some(existing) if **existing == *row => false,
            _ => {
                self.rows.insert(row.key.clone(), row);
                true
            }
        }
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
