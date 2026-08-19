//! Per-cluster connection health.
//!
//! The store, not the UI, is the authority on what state a cluster is in. The UI
//! reads this and renders it; it never infers connection state from whether a
//! table happens to be empty. An empty table and an expired token must never
//! look the same.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::time::{Duration, Instant};

use periscope_bridge::{ClusterEvent, ClusterId, ConnectionState};

/// What is known about one cluster's connection.
#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    /// Current state, including any underlying error text.
    pub state: ConnectionState,
    /// When the state last changed.
    pub since: Instant,
    /// Round trip of the most recent successful health probe.
    pub last_round_trip: Option<Duration>,
    /// Events discarded for this cluster because the UI fell behind.
    pub dropped_events: usize,
}

impl Connection {
    fn new(state: ConnectionState, now: Instant) -> Self {
        Self {
            state,
            since: now,
            last_round_trip: None,
            dropped_events: 0,
        }
    }

    /// Whether data for this cluster may be missing updates.
    pub fn is_stale(&self) -> bool {
        self.dropped_events > 0
    }
}

/// Connection state for every cluster the app knows about.
///
/// Ordered by cluster id so the UI renders a stable list without re-sorting.
#[derive(Clone, Debug, Default)]
pub struct ConnectionRegistry {
    clusters: BTreeMap<ClusterId, Connection>,
}

impl ConnectionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a cluster in the [`ConnectionState::Idle`] state if it is new.
    pub fn track(&mut self, cluster: ClusterId, now: Instant) -> &Connection {
        self.clusters
            .entry(cluster)
            .or_insert_with(|| Connection::new(ConnectionState::Idle, now))
    }

    /// Folds one event into the registry.
    ///
    /// Returns `true` when something the UI renders actually changed, so callers
    /// can skip a repaint on no-op events.
    pub fn apply(&mut self, event: &ClusterEvent, now: Instant) -> bool {
        match event {
            ClusterEvent::Status { cluster, state } => match self.clusters.entry(cluster.clone()) {
                // A cluster appearing for the first time is itself a change: a
                // new row shows up in the UI.
                Entry::Vacant(slot) => {
                    slot.insert(Connection::new(state.clone(), now));
                    true
                }
                Entry::Occupied(mut slot) => {
                    if slot.get().state == *state {
                        return false;
                    }
                    let connection = slot.get_mut();
                    connection.state = state.clone();
                    connection.since = now;
                    true
                }
            },
            ClusterEvent::Pong {
                cluster, elapsed, ..
            } => {
                let entry = self
                    .clusters
                    .entry(cluster.clone())
                    .or_insert_with(|| Connection::new(ConnectionState::Idle, now));
                entry.last_round_trip = Some(*elapsed);
                true
            }
            ClusterEvent::Stale { cluster, dropped } => {
                if *dropped == 0 {
                    return false;
                }
                match cluster {
                    Some(cluster) => {
                        let entry = self
                            .clusters
                            .entry(cluster.clone())
                            .or_insert_with(|| Connection::new(ConnectionState::Idle, now));
                        entry.dropped_events += dropped;
                    }
                    None => {
                        // Unattributed drops taint everything currently connected.
                        for entry in self.clusters.values_mut() {
                            entry.dropped_events += dropped;
                        }
                    }
                }
                true
            }
            // Resource and kubeconfig events say nothing about connection
            // health; the store's other tables own those.
            ClusterEvent::Contexts { .. }
            | ClusterEvent::ConfigFailed { .. }
            | ClusterEvent::Kinds { .. }
            | ClusterEvent::ResourceReset { .. }
            | ClusterEvent::ResourceApplied { .. }
            | ClusterEvent::ResourceDeleted { .. }
            | ClusterEvent::Object { .. }
            | ClusterEvent::ObjectFailed { .. }
            | ClusterEvent::KindFailed { .. }
            | ClusterEvent::LogBatch { .. }
            | ClusterEvent::LogSourceChanged { .. }
            | ClusterEvent::LogsFailed { .. }
            | ClusterEvent::MutationDone { .. }
            | ClusterEvent::ForwardChanged { .. }
            | ClusterEvent::ExecOutput { .. }
            | ClusterEvent::ExecFinished { .. } => false,
        }
    }

    /// Clears the stale marker for a cluster, once its data has been resynced.
    pub fn mark_fresh(&mut self, cluster: &ClusterId) {
        if let Some(connection) = self.clusters.get_mut(cluster) {
            connection.dropped_events = 0;
        }
    }

    /// Folds a whole flush batch in, reporting whether anything changed.
    pub fn apply_batch<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a ClusterEvent>,
        now: Instant,
    ) -> bool {
        let mut changed = false;
        for event in events {
            changed |= self.apply(event, now);
        }
        changed
    }

    /// Looks a cluster up.
    pub fn get(&self, cluster: &ClusterId) -> Option<&Connection> {
        self.clusters.get(cluster)
    }

    /// Every cluster, ordered by id.
    pub fn iter(&self) -> impl Iterator<Item = (&ClusterId, &Connection)> {
        self.clusters.iter()
    }

    /// How many clusters are tracked.
    pub fn len(&self) -> usize {
        self.clusters.len()
    }

    /// Whether any cluster is tracked.
    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    /// Clusters currently needing the user's attention.
    pub fn problems(&self) -> impl Iterator<Item = (&ClusterId, &Connection)> {
        self.clusters
            .iter()
            .filter(|(_, connection)| connection.state.is_problem())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(cluster: &str, state: ConnectionState) -> ClusterEvent {
        ClusterEvent::Status {
            cluster: cluster.into(),
            state,
        }
    }

    #[test]
    fn a_cluster_appearing_for_the_first_time_counts_as_a_change() {
        let mut registry = ConnectionRegistry::new();
        // Otherwise the first status for a cluster would seed the map and then
        // compare equal to itself, and the new row would never be painted.
        assert!(registry.apply(&status("prod", ConnectionState::Connected), Instant::now()));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_new_cluster_starts_idle() {
        let mut registry = ConnectionRegistry::new();
        let connection = registry.track("prod".into(), Instant::now());
        assert_eq!(connection.state, ConnectionState::Idle);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn status_events_move_the_state_machine() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();

        assert!(registry.apply(&status("prod", ConnectionState::Connecting), now));
        let later = now + Duration::from_millis(50);
        assert!(registry.apply(&status("prod", ConnectionState::Connected), later));

        let connection = registry.get(&"prod".into()).unwrap();
        assert_eq!(connection.state, ConnectionState::Connected);
        assert_eq!(connection.since, later);
    }

    #[test]
    fn a_repeated_status_is_not_a_change() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();

        assert!(registry.apply(&status("prod", ConnectionState::Connected), now));
        assert!(!registry.apply(
            &status("prod", ConnectionState::Connected),
            now + Duration::from_secs(1)
        ));
        // The timestamp must not move when nothing changed.
        assert_eq!(registry.get(&"prod".into()).unwrap().since, now);
    }

    #[test]
    fn auth_failure_keeps_its_error_text_for_the_ui() {
        let mut registry = ConnectionRegistry::new();
        registry.apply(
            &status(
                "prod",
                ConnectionState::AuthFailed {
                    reason: "exec plugin exited 255".into(),
                },
            ),
            Instant::now(),
        );

        let problems: Vec<_> = registry.problems().collect();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].1.state.detail(), Some("exec plugin exited 255"));
    }

    #[test]
    fn pongs_record_the_round_trip() {
        let mut registry = ConnectionRegistry::new();
        registry.apply(
            &ClusterEvent::Pong {
                cluster: "prod".into(),
                nonce: 1,
                elapsed: Duration::from_millis(12),
            },
            Instant::now(),
        );
        assert_eq!(
            registry.get(&"prod".into()).unwrap().last_round_trip,
            Some(Duration::from_millis(12))
        );
    }

    #[test]
    fn dropped_events_mark_a_cluster_stale() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();
        registry.apply(&status("prod", ConnectionState::Connected), now);

        assert!(registry.apply(
            &ClusterEvent::Stale {
                cluster: Some("prod".into()),
                dropped: 12,
            },
            now,
        ));
        assert!(registry.get(&"prod".into()).unwrap().is_stale());
        // Still connected: stale data is not a disconnection.
        assert_eq!(
            registry.get(&"prod".into()).unwrap().state,
            ConnectionState::Connected
        );
    }

    #[test]
    fn unattributed_drops_taint_every_cluster() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();
        registry.apply(&status("a", ConnectionState::Connected), now);
        registry.apply(&status("b", ConnectionState::Connected), now);

        registry.apply(
            &ClusterEvent::Stale {
                cluster: None,
                dropped: 3,
            },
            now,
        );

        assert!(registry.iter().all(|(_, c)| c.dropped_events == 3));
    }

    #[test]
    fn a_zero_drop_notice_changes_nothing() {
        let mut registry = ConnectionRegistry::new();
        assert!(!registry.apply(
            &ClusterEvent::Stale {
                cluster: None,
                dropped: 0,
            },
            Instant::now(),
        ));
    }

    #[test]
    fn batches_report_whether_anything_changed() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();
        let batch = vec![
            status("a", ConnectionState::Connecting),
            status("a", ConnectionState::Connected),
        ];
        assert!(registry.apply_batch(&batch, now));
        assert!(!registry.apply_batch(&batch[1..], now));
    }

    #[test]
    fn clusters_are_listed_in_a_stable_order() {
        let mut registry = ConnectionRegistry::new();
        let now = Instant::now();
        for name in ["zeta", "alpha", "mid"] {
            registry.apply(&status(name, ConnectionState::Connected), now);
        }

        let names: Vec<_> = registry.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(names, ["alpha", "mid", "zeta"]);
    }
}
