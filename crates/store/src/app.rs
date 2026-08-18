//! Everything the UI renders, and the only place that decides what is true.
//!
//! The view holds one of these and asks it questions; it never infers state
//! from what happens to be in a table. That separation is what makes "the
//! table is empty" and "the token expired" impossible to confuse, and it is why
//! every rule below is testable without a window or a cluster.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use periscope_bridge::{ClusterEvent, ClusterId, ContextInfo, PodSnapshot};

use crate::connections::{Connection, ConnectionRegistry};
use crate::pods::PodTable;

/// The application's state.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    connections: ConnectionRegistry,
    contexts: Vec<ContextInfo>,
    current: Option<ClusterId>,
    config_error: Option<String>,
    active: Option<ClusterId>,
    tables: BTreeMap<ClusterId, PodTable>,
    /// Materialised rows for the active cluster, for indexed access by the
    /// virtualised list. Rebuilt only when that cluster's table changes, and
    /// shared as an `Arc` so a render that captures them costs one refcount
    /// bump rather than a copy of ten thousand pointers.
    rows: Arc<[Arc<PodSnapshot>]>,
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

            ClusterEvent::PodsReset { cluster, pods } => {
                let touched = self.table_mut(cluster).reset(pods);
                // A completed resync is the moment dropped events stop
                // mattering: the table was just rebuilt from scratch.
                self.connections.mark_fresh(cluster);
                changed |= self.touch(cluster, touched);
            }

            ClusterEvent::PodApplied { cluster, pod } => {
                let touched = self.table_mut(cluster).apply(Arc::clone(pod));
                changed |= self.touch(cluster, touched);
            }

            ClusterEvent::PodDeleted { cluster, key } => {
                let touched = self.table_mut(cluster).remove(key);
                changed |= self.touch(cluster, touched);
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

    /// Switches which cluster the table shows.
    pub fn select(&mut self, cluster: ClusterId) {
        if self.active.as_ref() == Some(&cluster) {
            return;
        }
        self.active = Some(cluster);
        self.refresh_rows();
    }

    /// The cluster currently being viewed.
    pub fn active(&self) -> Option<&ClusterId> {
        self.active.as_ref()
    }

    /// The rows the table renders, in namespace-then-name order.
    pub fn rows(&self) -> &[Arc<PodSnapshot>] {
        &self.rows
    }

    /// The same rows, shared, for a virtualised list that must own what it
    /// renders from.
    pub fn rows_shared(&self) -> Arc<[Arc<PodSnapshot>]> {
        Arc::clone(&self.rows)
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

    /// Pods held for the cluster being viewed, and how many are fully ready.
    pub fn active_counts(&self) -> (usize, usize) {
        match self.active.as_ref().and_then(|id| self.tables.get(id)) {
            Some(table) => (table.len(), table.ready()),
            None => (0, 0),
        }
    }

    /// Pods held for a cluster, whether or not it is the one being viewed.
    pub fn pod_count(&self, cluster: &ClusterId) -> usize {
        self.tables.get(cluster).map_or(0, PodTable::len)
    }

    fn table_mut(&mut self, cluster: &ClusterId) -> &mut PodTable {
        self.tables.entry(cluster.clone()).or_default()
    }

    /// Reports a table change, and whether it is one the user can see.
    fn touch(&mut self, cluster: &ClusterId, changed: bool) -> bool {
        changed && self.active.as_ref() == Some(cluster)
    }

    fn affects_active(&self, event: &ClusterEvent) -> bool {
        matches!(
            event,
            ClusterEvent::PodsReset { .. }
                | ClusterEvent::PodApplied { .. }
                | ClusterEvent::PodDeleted { .. }
        ) && event.cluster() == self.active.as_ref()
    }

    fn refresh_rows(&mut self) {
        self.rows = match self.active.as_ref().and_then(|id| self.tables.get(id)) {
            Some(table) => Arc::from(table.rows()),
            None => Arc::from([] as [Arc<PodSnapshot>; 0]),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::{ConnectionState, ResourceKey};

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

    fn pod(namespace: &str, name: &str) -> PodSnapshot {
        PodSnapshot {
            key: ResourceKey::new(namespace, name),
            uid: None,
            status: Arc::from("Running"),
            ready: 1,
            containers: 1,
            restarts: 0,
            node: None,
            created: None,
        }
    }

    fn state_with_contexts() -> AppState {
        let mut state = AppState::new();
        state.apply_batch(
            &[contexts_event(&["prod", "staging"], Some("prod"))],
            Instant::now(),
        );
        state
    }

    fn row_names(state: &AppState) -> Vec<String> {
        state.rows().iter().map(|pod| pod.key.to_string()).collect()
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
        state.select(ClusterId::new("staging"));

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
    fn a_successful_re_read_clears_the_failure() {
        let mut state = AppState::new();
        state.apply_batch(
            &[ClusterEvent::ConfigFailed {
                reason: "permission denied".into(),
            }],
            Instant::now(),
        );

        state.apply_batch(&[contexts_event(&["prod"], Some("prod"))], Instant::now());
        assert_eq!(state.config_error(), None);
    }

    #[test]
    fn pods_for_the_active_cluster_become_rows() {
        let mut state = state_with_contexts();

        state.apply_batch(
            &[ClusterEvent::PodsReset {
                cluster: "prod".into(),
                pods: Arc::from([pod("default", "web"), pod("default", "api")]),
            }],
            Instant::now(),
        );

        assert_eq!(row_names(&state), ["default/api", "default/web"]);
        assert_eq!(state.active_counts(), (2, 2));
    }

    #[test]
    fn pods_for_another_cluster_are_kept_but_not_rendered() {
        let mut state = state_with_contexts();

        let changed = state.apply_batch(
            &[ClusterEvent::PodsReset {
                cluster: "staging".into(),
                pods: Arc::from([pod("default", "api")]),
            }],
            Instant::now(),
        );

        // Nothing visible changed, so no repaint; the data is still there for
        // when the user switches over.
        assert!(!changed);
        assert!(state.rows().is_empty());
        assert_eq!(state.pod_count(&ClusterId::new("staging")), 1);
    }

    #[test]
    fn switching_clusters_shows_the_other_table_without_refetching() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[
                ClusterEvent::PodsReset {
                    cluster: "prod".into(),
                    pods: Arc::from([pod("default", "prod-pod")]),
                },
                ClusterEvent::PodsReset {
                    cluster: "staging".into(),
                    pods: Arc::from([pod("default", "staging-pod")]),
                },
            ],
            Instant::now(),
        );

        state.select(ClusterId::new("staging"));
        assert_eq!(row_names(&state), ["default/staging-pod"]);

        state.select(ClusterId::new("prod"));
        assert_eq!(row_names(&state), ["default/prod-pod"]);
    }

    #[test]
    fn an_update_and_a_delete_move_the_rows() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[ClusterEvent::PodApplied {
                cluster: "prod".into(),
                pod: Arc::new(pod("default", "api")),
            }],
            Instant::now(),
        );
        assert_eq!(row_names(&state), ["default/api"]);

        state.apply_batch(
            &[ClusterEvent::PodDeleted {
                cluster: "prod".into(),
                key: ResourceKey::new("default", "api"),
            }],
            Instant::now(),
        );
        assert!(state.rows().is_empty());
    }

    #[test]
    fn a_resync_that_changes_nothing_does_not_repaint() {
        let mut state = state_with_contexts();
        let reset = ClusterEvent::PodsReset {
            cluster: "prod".into(),
            pods: Arc::from([pod("default", "api")]),
        };

        assert!(state.apply_batch(std::slice::from_ref(&reset), Instant::now()));
        assert!(!state.apply_batch(std::slice::from_ref(&reset), Instant::now()));
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

        state.apply_batch(
            &[ClusterEvent::PodsReset {
                cluster: "prod".into(),
                pods: Arc::from([pod("default", "api")]),
            }],
            now,
        );

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
    fn connection_state_is_reported_for_the_active_cluster() {
        let mut state = state_with_contexts();
        state.apply_batch(
            &[ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::AuthFailed {
                    reason: "exec plugin exited 255".into(),
                },
            }],
            Instant::now(),
        );

        let connection = state.active_connection().expect("active cluster tracked");
        assert_eq!(connection.state.detail(), Some("exec plugin exited 255"));
    }

    #[test]
    fn with_no_contexts_there_is_nothing_active_and_no_rows() {
        let state = AppState::new();
        assert_eq!(state.active(), None);
        assert!(state.rows().is_empty());
        assert_eq!(state.active_counts(), (0, 0));
    }
}
