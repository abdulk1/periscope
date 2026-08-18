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

/// Accumulates items, collapsing those that share a key.
#[derive(Debug)]
pub struct Coalescer<K, T> {
    interval: Duration,
    max_batch: usize,
    items: Vec<T>,
    index: HashMap<K, usize>,
    deadline: Option<Instant>,
    collapsed: u64,
}

impl<K, T> Coalescer<K, T>
where
    K: Eq + Hash + Clone,
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
    pub fn push(&mut self, key: Option<K>, item: T, now: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(now + self.interval);
        }

        if let Some(key) = key {
            if let Some(&slot) = self.index.get(&key) {
                self.items[slot] = item;
                self.collapsed += 1;
                return;
            }
            self.index.insert(key, self.items.len());
        }

        self.items.push(item);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLUSH: Duration = Duration::from_millis(16);

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
    fn zero_max_batch_is_clamped_rather_than_wedging() {
        let mut c: Coalescer<&str, i32> = Coalescer::new(FLUSH, 0);
        let start = Instant::now();
        c.push(None, 1, start);
        assert!(c.is_ready(start));
    }
}
