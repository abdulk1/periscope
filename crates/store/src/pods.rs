//! The pod cache for one cluster.
//!
//! Keyed by namespace and name in a `BTreeMap`, so iteration is already in the
//! order the table renders and no sort is needed after an update. Rows are
//! `Arc`s: the UI materialises a `Vec` of them for virtualised rendering, and
//! that clone is a refcount bump rather than a copy of every pod.

use std::collections::BTreeMap;
use std::sync::Arc;

use periscope_bridge::{PodSnapshot, ResourceKey};

/// Every pod known for one cluster.
#[derive(Clone, Debug, Default)]
pub struct PodTable {
    pods: BTreeMap<ResourceKey, Arc<PodSnapshot>>,
}

impl PodTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the whole table with a fresh listing.
    ///
    /// Returns whether anything the UI renders changed. A resync that finds the
    /// world unchanged — the common case after a brief watch drop — must not
    /// repaint 10,000 rows.
    pub fn reset(&mut self, pods: &[PodSnapshot]) -> bool {
        let replacement: BTreeMap<_, _> = pods
            .iter()
            .map(|pod| (pod.key.clone(), Arc::new(pod.clone())))
            .collect();

        if replacement.len() == self.pods.len()
            && replacement
                .iter()
                .zip(self.pods.iter())
                .all(|((_, new), (_, old))| new == old)
        {
            return false;
        }

        self.pods = replacement;
        true
    }

    /// Adds or updates one pod, reporting whether it differed from what was held.
    pub fn apply(&mut self, pod: Arc<PodSnapshot>) -> bool {
        match self.pods.get(&pod.key) {
            Some(existing) if **existing == *pod => false,
            _ => {
                self.pods.insert(pod.key.clone(), pod);
                true
            }
        }
    }

    /// Removes a pod, reporting whether it was there.
    pub fn remove(&mut self, key: &ResourceKey) -> bool {
        self.pods.remove(key).is_some()
    }

    /// Pods in table order: namespace, then name.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<PodSnapshot>> {
        self.pods.values()
    }

    /// A snapshot of the rows, for indexed access by a virtualised list.
    pub fn rows(&self) -> Vec<Arc<PodSnapshot>> {
        self.pods.values().cloned().collect()
    }

    /// How many pods are held.
    pub fn len(&self) -> usize {
        self.pods.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.pods.is_empty()
    }

    /// How many pods have every container ready.
    pub fn ready(&self) -> usize {
        self.pods.values().filter(|pod| pod.is_ready()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(namespace: &str, name: &str, status: &str) -> PodSnapshot {
        PodSnapshot {
            key: ResourceKey::new(namespace, name),
            uid: None,
            status: Arc::from(status),
            ready: 1,
            containers: 1,
            restarts: 0,
            node: None,
            created: None,
        }
    }

    fn names(table: &PodTable) -> Vec<String> {
        table.iter().map(|pod| pod.key.to_string()).collect()
    }

    #[test]
    fn rows_come_back_sorted_by_namespace_then_name() {
        let mut table = PodTable::new();
        for (namespace, name) in [
            ("kube-system", "coredns"),
            ("default", "web"),
            ("default", "api"),
        ] {
            table.apply(Arc::new(pod(namespace, name, "Running")));
        }

        assert_eq!(
            names(&table),
            ["default/api", "default/web", "kube-system/coredns"]
        );
    }

    #[test]
    fn applying_an_identical_pod_is_not_a_change() {
        let mut table = PodTable::new();
        assert!(table.apply(Arc::new(pod("default", "api", "Running"))));
        // The apiserver resends objects on resync; repainting for those would
        // burn frames for nothing.
        assert!(!table.apply(Arc::new(pod("default", "api", "Running"))));
        assert!(table.apply(Arc::new(pod("default", "api", "CrashLoopBackOff"))));
    }

    #[test]
    fn a_reset_replaces_the_whole_table() {
        let mut table = PodTable::new();
        table.apply(Arc::new(pod("default", "gone", "Running")));

        assert!(table.reset(&[pod("default", "api", "Running")]));
        assert_eq!(names(&table), ["default/api"]);
    }

    #[test]
    fn a_reset_that_changes_nothing_reports_no_change() {
        let mut table = PodTable::new();
        table.reset(&[pod("default", "api", "Running")]);

        assert!(!table.reset(&[pod("default", "api", "Running")]));
        assert!(table.reset(&[pod("default", "api", "Terminating")]));
    }

    #[test]
    fn a_reset_to_nothing_empties_the_table() {
        let mut table = PodTable::new();
        table.apply(Arc::new(pod("default", "api", "Running")));

        assert!(table.reset(&[]));
        assert!(table.is_empty());
        assert!(!table.reset(&[]));
    }

    #[test]
    fn removing_reports_whether_the_pod_was_known() {
        let mut table = PodTable::new();
        table.apply(Arc::new(pod("default", "api", "Running")));

        assert!(table.remove(&ResourceKey::new("default", "api")));
        assert!(!table.remove(&ResourceKey::new("default", "api")));
        assert!(table.is_empty());
    }

    #[test]
    fn readiness_is_counted_across_the_table() {
        let mut table = PodTable::new();
        table.apply(Arc::new(pod("default", "up", "Running")));

        let mut down = pod("default", "down", "CrashLoopBackOff");
        down.ready = 0;
        table.apply(Arc::new(down));

        assert_eq!((table.len(), table.ready()), (2, 1));
    }

    #[test]
    fn a_ten_thousand_pod_listing_stays_ordered_and_addressable() {
        let mut table = PodTable::new();
        let pods: Vec<_> = (0..10_000)
            .map(|i| pod("default", &format!("worker-{i:05}"), "Running"))
            .collect();

        assert!(table.reset(&pods));
        assert_eq!(table.len(), 10_000);

        let rows = table.rows();
        assert_eq!(&*rows[0].key.name, "worker-00000");
        assert_eq!(&*rows[9_999].key.name, "worker-09999");
    }
}
