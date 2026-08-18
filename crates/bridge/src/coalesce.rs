//! Batching so a watch storm becomes one UI mutation.
//!
//! The rule from the architecture spec: never apply one watch event per frame.
//! Events accumulate here until a flush deadline passes or the batch grows past
//! a cap, and events that supersede each other collapse in place.
//!
//! Time is passed in rather than read from a clock so the behaviour is testable
//! without sleeping.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// How a key relates to the other keys already pending.
///
/// Equal keys always collapse; that is the cheap, indexed case. Some events are
/// wider than that: a full resync of a cluster's pods makes *every* pending
/// update for that cluster redundant. Without this, in-place replacement could
/// reorder a newer update ahead of the resync that was meant to precede it, and
/// the resync would silently undo it.
pub trait CoalesceKey: Eq + Hash + Clone {
    /// Whether an item keyed by `self` makes one keyed by `other` redundant.
    ///
    /// Only consulted for keys that report [`CoalesceKey::is_barrier`].
    fn supersedes(&self, other: &Self) -> bool {
        self == other
    }

    /// Whether this key supersedes anything beyond an equal key. Keys that do
    /// not — the overwhelming majority — skip the linear scan entirely.
    fn is_barrier(&self) -> bool {
        false
    }
}

/// One pending item and the key it collapses on.
#[derive(Debug)]
struct Pending<K, T> {
    key: Option<K>,
    item: T,
}

/// Accumulates items, collapsing those that share a key.
#[derive(Debug)]
pub struct Coalescer<K, T> {
    interval: Duration,
    max_batch: usize,
    items: Vec<Pending<K, T>>,
    index: HashMap<K, usize>,
    deadline: Option<Instant>,
    collapsed: u64,
}

impl<K, T> Coalescer<K, T>
where
    K: CoalesceKey,
{
    /// Creates a coalescer that flushes after `interval`, or sooner if the
    /// batch reaches `max_batch` items.
    ///
    /// `max_batch` is clamped to at least 1 so a zero can never wedge the pump.
    pub fn new(interval: Duration, max_batch: usize) -> Self {
        Self {
            interval,
            max_batch: max_batch.max(1),
            items: Vec::new(),
            index: HashMap::new(),
            deadline: None,
            collapsed: 0,
        }
    }

    /// Adds an item. If `key` is `Some` and an item with that key is already
    /// pending, the pending item is replaced in place — position in the batch is
    /// preserved so ordering stays stable.
    ///
    /// A barrier key instead *removes* everything it supersedes and takes its
    /// place at the end of the batch, so nothing it invalidates can be applied
    /// after it.
    pub fn push(&mut self, key: Option<K>, item: T, now: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(now + self.interval);
        }

        let Some(key) = key else {
            self.items.push(Pending { key: None, item });
            return;
        };

        if key.is_barrier() {
            self.supersede(&key);
        } else if let Some(&slot) = self.index.get(&key) {
            self.items[slot].item = item;
            self.collapsed += 1;
            return;
        }

        self.index.insert(key.clone(), self.items.len());
        self.items.push(Pending {
            key: Some(key),
            item,
        });
    }

    /// Drops every pending item that `key` makes redundant, and reindexes.
    ///
    /// Unkeyed items always survive: they are the ones that must all be
    /// delivered, so no barrier may swallow them.
    fn supersede(&mut self, key: &K) {
        let before = self.items.len();
        self.items
            .retain(|pending| !pending.key.as_ref().is_some_and(|k| key.supersedes(k)));

        let removed = before - self.items.len();
        if removed == 0 {
            return;
        }
        self.collapsed += removed as u64;

        self.index.clear();
        for (slot, pending) in self.items.iter().enumerate() {
            if let Some(key) = &pending.key {
                self.index.insert(key.clone(), slot);
            }
        }
    }

    /// Number of items waiting to be flushed.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether anything is waiting.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many pushes were absorbed by an already-pending item.
    pub fn collapsed(&self) -> u64 {
        self.collapsed
    }

    /// When the pending batch wants to be flushed, if anything is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Whether the batch should be flushed now: the deadline passed, or the
    /// batch hit its size cap.
    pub fn is_ready(&self, now: Instant) -> bool {
        if self.items.is_empty() {
            return false;
        }
        self.items.len() >= self.max_batch || self.deadline.is_some_and(|at| now >= at)
    }

    /// Takes the pending batch, resetting the deadline.
    pub fn drain(&mut self) -> Vec<T> {
        self.index.clear();
        self.deadline = None;
        std::mem::take(&mut self.items)
            .into_iter()
            .map(|pending| pending.item)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLUSH: Duration = Duration::from_millis(16);

    impl CoalesceKey for &'static str {}
    impl CoalesceKey for u32 {}

    /// A key with two scopes: one object, or a whole cluster's worth of them.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    enum Scoped {
        Object(&'static str, u32),
        Resync(&'static str),
    }

    impl CoalesceKey for Scoped {
        fn is_barrier(&self) -> bool {
            matches!(self, Self::Resync(_))
        }

        fn supersedes(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Resync(cluster), Self::Object(other, _) | Self::Resync(other)) => {
                    cluster == other
                }
                _ => false,
            }
        }
    }

    fn coalescer() -> Coalescer<&'static str, i32> {
        Coalescer::new(FLUSH, 512)
    }

    #[test]
    fn empty_coalescer_is_never_ready() {
        let c = coalescer();
        assert!(c.is_empty());
        assert!(!c.is_ready(Instant::now()));
        assert_eq!(c.deadline(), None);
    }

    #[test]
    fn unkeyed_items_all_survive() {
        let mut c = coalescer();
        let now = Instant::now();
        for i in 0..5 {
            c.push(None, i, now);
        }
        assert_eq!(c.drain(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn keyed_items_collapse_in_place() {
        let mut c = coalescer();
        let now = Instant::now();
        c.push(Some("a"), 1, now);
        c.push(Some("b"), 2, now);
        c.push(Some("a"), 3, now);

        assert_eq!(c.len(), 2);
        assert_eq!(c.collapsed(), 1);
        // "a" keeps its original slot but carries the newer value.
        assert_eq!(c.drain(), vec![3, 2]);
    }

    #[test]
    fn a_resync_storm_collapses_to_one_item_per_object() {
        let mut c: Coalescer<u32, u32> = Coalescer::new(FLUSH, usize::MAX);
        let now = Instant::now();
        // 10k objects, each updated 5 times.
        for round in 0..5 {
            for object in 0..10_000 {
                c.push(Some(object), object * 10 + round, now);
            }
        }

        assert_eq!(c.len(), 10_000);
        assert_eq!(c.collapsed(), 40_000);
        let batch = c.drain();
        // Each object carries only its newest value.
        assert_eq!(batch[0], 4);
        assert_eq!(batch[9_999], 99_994);
    }

    #[test]
    fn deadline_is_set_on_first_push_and_not_extended() {
        let mut c = coalescer();
        let start = Instant::now();
        c.push(None, 1, start);
        let deadline = c.deadline().expect("deadline set on first push");
        assert_eq!(deadline, start + FLUSH);

        // A later push must not push the deadline out, or a steady event stream
        // would starve the UI forever.
        c.push(None, 2, start + Duration::from_millis(10));
        assert_eq!(c.deadline(), Some(deadline));
    }

    #[test]
    fn becomes_ready_at_the_deadline() {
        let mut c = coalescer();
        let start = Instant::now();
        c.push(None, 1, start);

        assert!(!c.is_ready(start));
        assert!(!c.is_ready(start + Duration::from_millis(15)));
        assert!(c.is_ready(start + FLUSH));
    }

    #[test]
    fn becomes_ready_early_when_the_batch_cap_is_hit() {
        let mut c: Coalescer<&str, i32> = Coalescer::new(FLUSH, 3);
        let start = Instant::now();
        c.push(None, 1, start);
        c.push(None, 2, start);
        assert!(!c.is_ready(start));
        c.push(None, 3, start);
        assert!(c.is_ready(start));
    }

    #[test]
    fn draining_resets_the_deadline_and_the_key_index() {
        let mut c = coalescer();
        let start = Instant::now();
        c.push(Some("a"), 1, start);
        assert_eq!(c.drain(), vec![1]);
        assert_eq!(c.deadline(), None);

        // The key from the drained batch must not collapse into the new one.
        let later = start + Duration::from_secs(1);
        c.push(Some("a"), 2, later);
        assert_eq!(c.len(), 1);
        assert_eq!(c.deadline(), Some(later + FLUSH));
        assert_eq!(c.drain(), vec![2]);
    }

    #[test]
    fn a_barrier_drops_what_it_supersedes_and_moves_to_the_end() {
        let mut c: Coalescer<Scoped, &str> = Coalescer::new(FLUSH, usize::MAX);
        let now = Instant::now();

        c.push(Some(Scoped::Object("prod", 1)), "pod-1 v1", now);
        c.push(Some(Scoped::Object("staging", 9)), "other cluster", now);
        c.push(Some(Scoped::Resync("prod")), "prod resync", now);
        // Arrives after the resync, so it must stay after it.
        c.push(Some(Scoped::Object("prod", 1)), "pod-1 v2", now);

        assert_eq!(c.drain(), vec!["other cluster", "prod resync", "pod-1 v2"]);
    }

    #[test]
    fn a_barrier_never_swallows_unkeyed_items() {
        let mut c: Coalescer<Scoped, &str> = Coalescer::new(FLUSH, usize::MAX);
        let now = Instant::now();

        c.push(None, "must be delivered", now);
        c.push(Some(Scoped::Object("prod", 1)), "pod-1", now);
        c.push(Some(Scoped::Resync("prod")), "prod resync", now);

        assert_eq!(c.drain(), vec!["must be delivered", "prod resync"]);
    }

    #[test]
    fn back_to_back_barriers_collapse_to_the_newest() {
        let mut c: Coalescer<Scoped, &str> = Coalescer::new(FLUSH, usize::MAX);
        let now = Instant::now();

        c.push(Some(Scoped::Resync("prod")), "first", now);
        c.push(Some(Scoped::Resync("prod")), "second", now);

        assert_eq!(c.len(), 1);
        assert_eq!(c.collapsed(), 1);
        assert_eq!(c.drain(), vec!["second"]);
    }

    #[test]
    fn zero_max_batch_is_clamped_rather_than_wedging() {
        let mut c: Coalescer<&str, i32> = Coalescer::new(FLUSH, 0);
        let start = Instant::now();
        c.push(None, 1, start);
        assert!(c.is_ready(start));
    }
}
