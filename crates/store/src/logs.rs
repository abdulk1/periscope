//! The log buffer: bounded, filtered, and fast enough to type at.
//!
//! Three rules shape this:
//!
//! * **Bounded.** A tail that runs all afternoon must not grow without limit,
//!   so the buffer is a ring: old lines are evicted and counted, and the count
//!   is shown rather than hidden.
//! * **Filtering never restarts the stream.** The filter is applied here, over
//!   what is already held, so changing it costs a scan and not a reconnection.
//! * **Incremental.** New lines are tested against the current filter as they
//!   arrive; a full re-scan happens only when the filter itself changes.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use periscope_bridge::{LogLine, LogSource, LogSourceState};
use regex::{Regex, RegexBuilder};

/// Lines kept per session before the oldest are dropped.
pub const DEFAULT_CAPACITY: usize = 100_000;

/// Seconds in a day, for turning a wall-clock time into a point in time.
const DAY: u64 = 86_400;

/// The order the visible lines are shown in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogOrder {
    /// The order the lines were read, which is what `kubectl logs` shows: each
    /// pod's backlog arrives as a block before the live streams interleave.
    #[default]
    Arrival,
    /// By the timestamp the container wrote the line at.
    ///
    /// Lines the apiserver gave no timestamp for — a container writing a
    /// partial line, mostly — cannot be placed among lines that have one, so
    /// they are listed last, in arrival order. [`LogBuffer::untimestamped`]
    /// counts them so the view can say so rather than leaving them to be
    /// discovered.
    Timestamp,
}

/// What the user typed into the filter box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterSpec {
    /// The pattern. Empty means everything matches.
    pub pattern: String,
    /// Whether to treat the pattern as a regular expression.
    pub regex: bool,
    /// Whether case matters.
    pub case_sensitive: bool,
}

impl FilterSpec {
    /// Whether this filter lets everything through.
    pub fn is_empty(&self) -> bool {
        self.pattern.is_empty()
    }
}

/// A compiled filter.
#[derive(Debug, Default)]
enum Matcher {
    /// Everything matches.
    #[default]
    All,
    /// Case-sensitive substring.
    Substring(String),
    /// Case-insensitive substring, pattern already lowercased.
    SubstringInsensitive(String),
    /// A regular expression.
    Pattern(Box<Regex>),
    /// The pattern is not a valid regular expression. Nothing matches, and the
    /// UI shows why rather than silently emptying the view.
    Invalid(String),
}

impl Matcher {
    fn compile(spec: &FilterSpec) -> Self {
        if spec.is_empty() {
            return Self::All;
        }

        if spec.regex {
            return match RegexBuilder::new(&spec.pattern)
                .case_insensitive(!spec.case_sensitive)
                .size_limit(1 << 20)
                .build()
            {
                Ok(regex) => Self::Pattern(Box::new(regex)),
                Err(error) => Self::Invalid(error.to_string()),
            };
        }

        if spec.case_sensitive {
            Self::Substring(spec.pattern.clone())
        } else {
            Self::SubstringInsensitive(spec.pattern.to_lowercase())
        }
    }

    fn matches(&self, line: &LogLine) -> bool {
        match self {
            Self::All => true,
            Self::Substring(needle) => line.text.contains(needle.as_str()),
            // `to_lowercase` per line would allocate on every line of a 500k
            // buffer; the ASCII fast path covers what logs actually contain.
            Self::SubstringInsensitive(needle) => contains_ignoring_case(&line.text, needle),
            Self::Pattern(regex) => regex.is_match(&line.text),
            Self::Invalid(_) => false,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Invalid(message) => Some(message),
            _ => None,
        }
    }
}

/// Case-insensitive substring search that does not allocate.
///
/// `needle` is already lowercase.
fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }

    if haystack.is_ascii() && needle.is_ascii() {
        let haystack = haystack.as_bytes();
        let needle = needle.as_bytes();
        let first = needle[0];
        return haystack.windows(needle.len()).any(|window| {
            window[0].eq_ignore_ascii_case(&first)
                && window
                    .iter()
                    .zip(needle)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        });
    }

    haystack.to_lowercase().contains(needle)
}

/// Everything one log session holds.
#[derive(Debug)]
pub struct LogBuffer {
    lines: VecDeque<Arc<LogLine>>,
    /// Sequence number of `lines.front()`; sequence numbers never repeat, so
    /// the visible index survives eviction.
    first: u64,
    /// Sequence numbers of the lines the current filter admits, in the order
    /// they arrived. This stays the source of truth whatever the display order
    /// is, so eviction and filtering never have to know about sorting.
    visible: VecDeque<u64>,
    /// The same sequence numbers in timestamp order. Empty unless
    /// [`LogOrder::Timestamp`] is in force.
    sorted: Vec<u64>,
    order: LogOrder,
    /// How many of the sorted lines carry no timestamp.
    untimestamped: usize,
    capacity: usize,
    dropped: u64,
    spec: FilterSpec,
    matcher: Matcher,
    sources: Vec<(LogSource, LogSourceState)>,
    /// Why the session failed, if it did.
    error: Option<String>,
    /// The visible lines in display order, shared with the view.
    ///
    /// The list is virtualised — thirty lines are on screen — but the view
    /// needs indexed access to the whole of it, and building that list per
    /// frame meant a hundred thousand refcount bumps and an 800KB allocation
    /// sixty times a second for as long as a log pane was open. Rebuilt only
    /// when the lines or the order actually move.
    snapshot: Arc<[Arc<LogLine>]>,
    /// Whether `snapshot` is behind the buffer.
    restale: bool,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl LogBuffer {
    /// A buffer holding at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            first: 0,
            visible: VecDeque::new(),
            sorted: Vec::new(),
            order: LogOrder::default(),
            untimestamped: 0,
            capacity: capacity.max(1),
            dropped: 0,
            spec: FilterSpec::default(),
            matcher: Matcher::All,
            sources: Vec::new(),
            error: None,
            snapshot: Arc::from([] as [Arc<LogLine>; 0]),
            restale: false,
        }
    }

    /// Appends a batch, evicting from the front once the cap is reached.
    pub fn extend(&mut self, batch: &[LogLine]) {
        for line in batch {
            let seq = self.first + self.lines.len() as u64;
            if self.matcher.matches(line) {
                self.visible.push_back(seq);
            }
            self.lines.push_back(Arc::new(line.clone()));
            self.evict();
        }
        self.reorder();
    }

    fn evict(&mut self) {
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
            self.first += 1;
            self.dropped += 1;

            // Anything visible that has fallen out of the ring goes with it.
            while self.visible.front().is_some_and(|&seq| seq < self.first) {
                self.visible.pop_front();
            }
        }
    }

    /// Replaces the filter, rescanning what is held.
    ///
    /// Returns whether the visible set changed.
    pub fn set_filter(&mut self, spec: FilterSpec) -> bool {
        if spec == self.spec {
            return false;
        }

        self.spec = spec;
        self.matcher = Matcher::compile(&self.spec);
        self.rescan();
        true
    }

    fn rescan(&mut self) {
        self.visible.clear();
        if matches!(self.matcher, Matcher::All) {
            self.visible
                .extend(self.first..self.first + self.lines.len() as u64);
        } else {
            for (offset, line) in self.lines.iter().enumerate() {
                if self.matcher.matches(line) {
                    self.visible.push_back(self.first + offset as u64);
                }
            }
        }
        self.reorder();
    }

    /// Shows the lines in a different order, reporting whether it changed.
    pub fn set_order(&mut self, order: LogOrder) -> bool {
        if order == self.order {
            return false;
        }
        self.order = order;
        self.reorder();
        true
    }

    /// The order the visible lines are shown in.
    pub fn order(&self) -> LogOrder {
        self.order
    }

    /// How many visible lines have no timestamp to be sorted by.
    ///
    /// Zero while the lines are in arrival order, where the question does not
    /// arise.
    pub fn untimestamped(&self) -> usize {
        self.untimestamped
    }

    /// Rebuilds the timestamp order over the current visible set.
    ///
    /// Sorting is a second view over the arrival order rather than a
    /// reordering of it, so nothing else in this file has to change. It is
    /// rebuilt once per batch — not once per line — and log lines arrive very
    /// nearly in timestamp order already, which is the case Rust's stable sort
    /// is fastest on.
    fn reorder(&mut self) {
        // Every path that adds, evicts, refilters or reorders lines ends here,
        // which makes this the one place the snapshot has to be given up.
        self.restale = true;

        self.untimestamped = 0;
        if self.order == LogOrder::Arrival {
            // Not merely stale: holding a second copy of a 100,000-line index
            // that nothing reads is 800KB nobody asked for.
            self.sorted = Vec::new();
            return;
        }

        let (lines, first) = (&self.lines, self.first);
        let stamp_of = |seq: u64| {
            lines
                .get((seq - first) as usize)
                .and_then(|line: &Arc<LogLine>| line.timestamp)
        };

        let mut sorted = std::mem::take(&mut self.sorted);
        sorted.clear();
        sorted.extend(self.visible.iter().copied());
        // `false` sorts before `true`, so timestamped lines come first in time
        // order and the rest keep their arrival order at the end.
        sorted.sort_by_key(|&seq| {
            let stamp = stamp_of(seq);
            (stamp.is_none(), stamp)
        });

        self.untimestamped = sorted
            .iter()
            .rev()
            .take_while(|&&seq| stamp_of(seq).is_none())
            .count();
        self.sorted = sorted;
    }

    /// The filter currently applied.
    pub fn filter(&self) -> &FilterSpec {
        &self.spec
    }

    /// Why the filter pattern is not usable, if it is not.
    pub fn filter_error(&self) -> Option<&str> {
        self.matcher.error()
    }

    /// The sequence number shown at a display index.
    pub fn sequence_at(&self, index: usize) -> Option<u64> {
        match self.order {
            LogOrder::Arrival => self.visible.get(index).copied(),
            LogOrder::Timestamp => self.sorted.get(index).copied(),
        }
    }

    /// Where a sequence number sits in the display order now, if it is still
    /// held.
    ///
    /// This is what lets a paused view stay on the line it was reading. The
    /// display index of a line moves every time the ring evicts from the front;
    /// the sequence number never does, which is the whole reason it exists.
    /// `None` means the line has been evicted — the reader was looking at
    /// something that is now gone, and is owed that news rather than a view
    /// that quietly slid somewhere else.
    pub fn index_of(&self, sequence: u64) -> Option<usize> {
        match self.order {
            // `visible` is ascending, so this is a binary search rather than a
            // walk: a 100,000-line buffer is looked up on every frame a paused
            // view is on screen.
            LogOrder::Arrival => self
                .visible
                .binary_search(&sequence)
                .ok()
                .filter(|_| self.lines_hold(sequence)),
            LogOrder::Timestamp => self.sorted.iter().position(|&seq| seq == sequence),
        }
    }

    /// Whether the ring still holds a sequence number.
    fn lines_hold(&self, sequence: u64) -> bool {
        sequence >= self.first && sequence < self.first + self.lines.len() as u64
    }

    /// The line at a visible index, in display order.
    pub fn visible_line(&self, index: usize) -> Option<&Arc<LogLine>> {
        let seq = self.sequence_at(index)?;
        self.lines.get((seq - self.first) as usize)
    }

    /// Every visible line, in display order.
    pub fn visible(&self) -> impl Iterator<Item = &Arc<LogLine>> {
        (0..self.visible_len()).filter_map(|index| self.visible_line(index))
    }

    /// A snapshot of the visible lines, for a virtualised list.
    ///
    /// Takes `&mut self` because it memoises; the caller is a render pass and
    /// the answer only changes when the buffer does.
    pub fn visible_lines(&mut self) -> Arc<[Arc<LogLine>]> {
        if self.restale {
            self.snapshot = self.visible().cloned().collect();
            self.restale = false;
        }
        Arc::clone(&self.snapshot)
    }

    /// How many lines are visible under the current filter.
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// How many lines are held.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether anything is held.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// How many lines the ring has discarded.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The first visible line at or after a point in time, for jumping.
    ///
    /// The index is into the display order, so a jump lands on the same line
    /// the view is showing at that position whichever [`LogOrder`] is in force.
    pub fn seek(&self, at: SystemTime) -> Option<usize> {
        self.visible()
            .position(|line| line.timestamp.is_some_and(|stamp| stamp >= at))
    }

    /// Records what a source is doing.
    pub fn source_changed(&mut self, source: LogSource, state: LogSourceState) {
        match self.sources.iter_mut().find(|(known, _)| known == &source) {
            Some(entry) => entry.1 = state,
            None => {
                self.sources.push((source, state));
                self.sources.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
    }

    /// The sources this session knows about.
    pub fn sources(&self) -> &[(LogSource, LogSourceState)] {
        &self.sources
    }

    /// How many sources are streaming right now.
    pub fn streaming(&self) -> usize {
        self.sources
            .iter()
            .filter(|(_, state)| state == &LogSourceState::Streaming)
            .count()
    }

    /// Records that the session failed.
    pub fn fail(&mut self, reason: String) {
        self.error = Some(reason);
    }

    /// Why the session failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The visible lines as text, for export and for the clipboard.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in self.visible() {
            out.push_str(&line.source.label());
            out.push(' ');
            out.push_str(&line.text);
            out.push('\n');
        }
        out
    }
}

/// Why a time typed into the jump field could not be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeError {
    /// Nothing was typed.
    Empty,
    /// The text is not one of the forms the field accepts.
    Unrecognised,
    /// The text parses but names an instant outside the epoch, which no log
    /// line can carry.
    OutOfRange,
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("type a time to jump to"),
            Self::Unrecognised => {
                f.write_str("not a time: try 14:32, 14:32:10, -5m, or 2026-08-18T14:32:00Z (UTC)")
            }
            Self::OutOfRange => f.write_str("that is outside the range a log line can carry"),
        }
    }
}

impl std::error::Error for TimeError {}

/// Turns what someone typed into the jump field into a point in time.
///
/// Four forms, because those are the four things people actually type when
/// they know roughly when something happened:
///
/// * `14:32` and `14:32:10` — a wall-clock time on the same day as `now`.
/// * `-5m` — relative to `now`. `s`, `m`, `h` and `d` are the units.
/// * `2026-08-18T14:32:00Z` — an RFC3339 stamp, as pasted from a log or an
///   alert. A `+01:00`-style offset is honoured; a space may stand in for the
///   `T`, because that is how most log files write it.
///
/// **Everything here is UTC**, including the bare `14:32`, because that is what
/// the timestamp column shows and a jump that disagreed with the column it
/// lands on would be worse than one that is merely not local.
///
/// `now` is a parameter rather than read here so the relative and same-day
/// forms are testable without waiting for a clock.
pub fn parse_time(input: &str, now: SystemTime) -> Result<SystemTime, TimeError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TimeError::Empty);
    }

    if let Some(ago) = input.strip_prefix('-') {
        let ago = parse_duration(ago)?;
        // `SystemTime` happily goes before the epoch; a log line cannot, so
        // "-1d" on a machine whose clock is at 1970 is a mistake, not a jump.
        return offset_epoch(
            epoch_seconds(now)?
                .checked_sub(ago.as_secs())
                .ok_or(TimeError::OutOfRange)?,
        );
    }

    if let Some(clock) = parse_clock(input) {
        let seconds = epoch_seconds(now)?;
        // The same UTC day as `now`, deliberately: rolling a future-looking
        // time back to yesterday would be a guess, and "no line at or after
        // 23:50" is a clearer answer than silently jumping a day.
        return offset_epoch(seconds - seconds % DAY + clock);
    }

    parse_rfc3339(input)
}

/// `5m`, `30s`, `2h`, `1d` — a count and a unit, no spaces.
fn parse_duration(input: &str) -> Result<Duration, TimeError> {
    // Split on the last character, not the last byte: `-5€` must be rejected
    // rather than panic on a split that is not a character boundary.
    let at = input
        .char_indices()
        .next_back()
        .ok_or(TimeError::Unrecognised)?
        .0;
    let (count, unit) = input.split_at(at);
    let count: u64 = count.parse().map_err(|_| TimeError::Unrecognised)?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => DAY,
        _ => return Err(TimeError::Unrecognised),
    };
    count
        .checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or(TimeError::OutOfRange)
}

/// `HH:MM` or `HH:MM:SS` as seconds since midnight.
fn parse_clock(input: &str) -> Option<u64> {
    let mut parts = input.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = match parts.next() {
        Some(field) => field.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(hours * 3_600 + minutes * 60 + seconds)
}

/// An RFC3339 stamp, to whole seconds.
///
/// Sub-second precision is parsed and discarded: log timestamps are held to the
/// second, so keeping it would only promise an accuracy the buffer does not
/// have.
fn parse_rfc3339(input: &str) -> Result<SystemTime, TimeError> {
    let (date, rest) = input
        .split_once(['T', 't', ' '])
        .ok_or(TimeError::Unrecognised)?;

    let (time, offset) = split_offset(rest)?;
    let clock =
        parse_clock(time.split('.').next().unwrap_or(time)).ok_or(TimeError::Unrecognised)?;

    let mut fields = date.split('-');
    let year: i64 = fields
        .next()
        .ok_or(TimeError::Unrecognised)?
        .parse()
        .map_err(|_| TimeError::Unrecognised)?;
    let month: u32 = fields
        .next()
        .ok_or(TimeError::Unrecognised)?
        .parse()
        .map_err(|_| TimeError::Unrecognised)?;
    let day: u32 = fields
        .next()
        .ok_or(TimeError::Unrecognised)?
        .parse()
        .map_err(|_| TimeError::Unrecognised)?;
    if fields.next().is_some()
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
    {
        return Err(TimeError::Unrecognised);
    }

    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(DAY as i64)
        .and_then(|seconds| seconds.checked_add(clock as i64))
        .and_then(|seconds| seconds.checked_sub(offset))
        .ok_or(TimeError::OutOfRange)?;

    offset_epoch(u64::try_from(seconds).map_err(|_| TimeError::OutOfRange)?)
}

/// Splits `12:00:00Z` or `12:00:00+01:00` into the clock and its offset from
/// UTC in seconds.
fn split_offset(rest: &str) -> Result<(&str, i64), TimeError> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Ok((time, 0));
    }

    // The sign cannot be the first character: that would be the hour field.
    let sign = rest
        .char_indices()
        .skip(1)
        .find(|(_, character)| *character == '+' || *character == '-');
    let Some((at, sign)) = sign else {
        // No offset at all. RFC3339 requires one; logs and humans routinely
        // omit it, and UTC is the only sensible reading in this app.
        return Ok((rest, 0));
    };

    let (time, zone) = rest.split_at(at);
    let clock = parse_clock(&zone[1..]).ok_or(TimeError::Unrecognised)?;
    let clock = i64::try_from(clock).map_err(|_| TimeError::OutOfRange)?;
    Ok((time, if sign == '-' { -clock } else { clock }))
}

/// Days since the epoch for a civil date, by Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

fn epoch_seconds(time: SystemTime) -> Result<u64, TimeError> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .map_err(|_| TimeError::OutOfRange)
}

fn offset_epoch(seconds: u64) -> Result<SystemTime, TimeError> {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or(TimeError::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(source: &str, text: &str) -> LogLine {
        LogLine {
            source: LogSource::new(source, "app"),
            timestamp: None,
            text: Arc::from(text),
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn texts(buffer: &LogBuffer) -> Vec<String> {
        buffer.visible().map(|line| line.text.to_string()).collect()
    }

    fn filter(pattern: &str) -> FilterSpec {
        FilterSpec {
            pattern: pattern.to_owned(),
            ..FilterSpec::default()
        }
    }

    #[test]
    fn a_line_keeps_its_identity_while_the_ring_evicts_underneath_it() {
        // The display index of a line moves every time the ring drops one from
        // the front. A paused view that remembers an index is looking at
        // something different a moment later — which is exactly what a reader
        // paused on a busy stream saw: "Paused", and 6.6 million lines dropped
        // out from under them.
        let mut buffer = LogBuffer::new(4);
        buffer.extend(&[line("api", "one"), line("api", "two")]);

        let watching = buffer.sequence_at(1).expect("a second line");
        assert_eq!(buffer.index_of(watching), Some(1));

        // Two more: still held, but everything has shifted down.
        buffer.extend(&[line("api", "three"), line("api", "four")]);
        assert_eq!(buffer.index_of(watching), Some(1));

        // Two more evicts the front. The line is still there, at a new index.
        buffer.extend(&[line("api", "five"), line("api", "six")]);
        assert_eq!(
            buffer.index_of(watching),
            None,
            "the watched line was evicted and index_of must say so"
        );

        // A line that survived reports where it is now, not where it was.
        let survivor = buffer.sequence_at(0).expect("a first line");
        assert_eq!(buffer.index_of(survivor), Some(0));
        buffer.extend(&[line("api", "seven")]);
        assert_eq!(buffer.index_of(survivor), None, "that one has now gone too");
    }

    #[test]
    fn the_visible_snapshot_is_rebuilt_only_when_the_lines_move() {
        // The default buffer holds 100,000 lines. Rebuilding this list per
        // frame was an 800KB allocation and a hundred thousand refcount bumps,
        // sixty times a second, for as long as a log pane was open.
        let mut buffer = LogBuffer::new(10);
        buffer.extend(&[line("api", "one"), line("api", "two")]);

        let first = buffer.visible_lines();
        assert_eq!(first.len(), 2);
        // Asking again with nothing changed hands back the same allocation.
        assert!(Arc::ptr_eq(&first, &buffer.visible_lines()));

        // A new line moves it.
        buffer.extend(&[line("api", "three")]);
        let grown = buffer.visible_lines();
        assert!(!Arc::ptr_eq(&first, &grown));
        assert_eq!(grown.len(), 3);

        // So does a filter.
        buffer.set_filter(filter("two"));
        assert_eq!(buffer.visible_lines().len(), 1);

        // And so does the order, which changes the list without changing the set.
        buffer.set_filter(FilterSpec::default());
        let arrival = buffer.visible_lines();
        buffer.set_order(LogOrder::Timestamp);
        assert!(!Arc::ptr_eq(&arrival, &buffer.visible_lines()));
    }

    #[test]
    fn lines_are_visible_in_the_order_they_arrived() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("a", "first"), line("b", "second")]);

        assert_eq!(texts(&buffer), ["first", "second"]);
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.dropped(), 0);
    }

    #[test]
    fn the_ring_drops_the_oldest_lines_and_counts_them() {
        let mut buffer = LogBuffer::new(3);
        for index in 0..10 {
            buffer.extend(&[line("a", &format!("line-{index}"))]);
        }

        assert_eq!(buffer.len(), 3);
        assert_eq!(texts(&buffer), ["line-7", "line-8", "line-9"]);
        // Silently discarding text would be worse than saying so.
        assert_eq!(buffer.dropped(), 7);
    }

    #[test]
    fn a_substring_filter_narrows_without_touching_the_buffer() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[
            line("a", "GET /health 200"),
            line("a", "GET /users 500"),
            line("b", "connected"),
        ]);

        assert!(buffer.set_filter(filter("500")));
        assert_eq!(texts(&buffer), ["GET /users 500"]);
        // The lines are still held: clearing the filter brings them back
        // without re-reading anything from the cluster.
        assert_eq!(buffer.len(), 3);

        assert!(buffer.set_filter(FilterSpec::default()));
        assert_eq!(buffer.visible_len(), 3);
    }

    #[test]
    fn filtering_is_case_insensitive_until_told_otherwise() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("a", "Error: nope"), line("a", "all good")]);

        buffer.set_filter(filter("error"));
        assert_eq!(texts(&buffer), ["Error: nope"]);

        buffer.set_filter(FilterSpec {
            pattern: "error".to_owned(),
            case_sensitive: true,
            ..FilterSpec::default()
        });
        assert!(texts(&buffer).is_empty());
    }

    #[test]
    fn a_regex_filter_matches_patterns() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[
            line("a", "status=200"),
            line("a", "status=404"),
            line("a", "status=503"),
        ]);

        buffer.set_filter(FilterSpec {
            pattern: r"status=5\d\d".to_owned(),
            regex: true,
            ..FilterSpec::default()
        });
        assert_eq!(texts(&buffer), ["status=503"]);
    }

    #[test]
    fn an_invalid_regex_says_why_rather_than_emptying_the_view_silently() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("a", "anything")]);

        buffer.set_filter(FilterSpec {
            pattern: "[unclosed".to_owned(),
            regex: true,
            ..FilterSpec::default()
        });

        assert!(buffer.visible_len() == 0);
        assert!(buffer.filter_error().is_some());
    }

    #[test]
    fn new_lines_are_filtered_as_they_arrive() {
        let mut buffer = LogBuffer::default();
        buffer.set_filter(filter("error"));

        buffer.extend(&[line("a", "fine"), line("a", "an error occurred")]);
        assert_eq!(texts(&buffer), ["an error occurred"]);
    }

    #[test]
    fn eviction_removes_lines_from_the_filtered_view_too() {
        let mut buffer = LogBuffer::new(2);
        buffer.set_filter(filter("keep"));

        buffer.extend(&[
            line("a", "keep 1"),
            line("a", "keep 2"),
            line("a", "keep 3"),
        ]);

        // The ring holds two; the visible set cannot claim three.
        assert_eq!(buffer.len(), 2);
        assert_eq!(texts(&buffer), ["keep 2", "keep 3"]);
    }

    #[test]
    fn seeking_finds_the_first_line_at_or_after_a_time() {
        let mut buffer = LogBuffer::default();
        for (index, seconds) in [10, 20, 30].into_iter().enumerate() {
            buffer.extend(&[LogLine {
                source: LogSource::new("a", "app"),
                timestamp: Some(at(seconds)),
                text: Arc::from(format!("line-{index}").as_str()),
            }]);
        }

        assert_eq!(buffer.seek(at(0)), Some(0));
        assert_eq!(buffer.seek(at(20)), Some(1));
        assert_eq!(buffer.seek(at(25)), Some(2));
        assert_eq!(buffer.seek(at(99)), None);
    }

    fn stamped(source: &str, seconds: Option<u64>, text: &str) -> LogLine {
        LogLine {
            source: LogSource::new(source, "app"),
            timestamp: seconds.map(at),
            text: Arc::from(text),
        }
    }

    #[test]
    fn ordering_by_timestamp_interleaves_two_backlogs() {
        let mut buffer = LogBuffer::default();
        // What a merged tail with history actually looks like: one pod's
        // backlog, then the other's, then the live streams.
        buffer.extend(&[
            stamped("a", Some(10), "a-first"),
            stamped("a", Some(30), "a-second"),
            stamped("b", Some(20), "b-first"),
            stamped("b", Some(40), "b-second"),
        ]);
        assert_eq!(
            texts(&buffer),
            ["a-first", "a-second", "b-first", "b-second"]
        );

        assert!(buffer.set_order(LogOrder::Timestamp));
        assert_eq!(
            texts(&buffer),
            ["a-first", "b-first", "a-second", "b-second"]
        );
        assert_eq!(buffer.order(), LogOrder::Timestamp);

        // And back, without re-reading anything.
        assert!(buffer.set_order(LogOrder::Arrival));
        assert_eq!(
            texts(&buffer),
            ["a-first", "a-second", "b-first", "b-second"]
        );
    }

    #[test]
    fn lines_with_no_timestamp_are_listed_last_and_counted() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[
            stamped("a", None, "partial one"),
            stamped("a", Some(30), "later"),
            stamped("a", None, "partial two"),
            stamped("a", Some(10), "earlier"),
        ]);

        buffer.set_order(LogOrder::Timestamp);
        assert_eq!(
            texts(&buffer),
            ["earlier", "later", "partial one", "partial two"]
        );
        // The count is what lets the view say where they went.
        assert_eq!(buffer.untimestamped(), 2);
    }

    #[test]
    fn arriving_lines_are_placed_in_the_sorted_order_too() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[stamped("a", Some(30), "late")]);
        buffer.set_order(LogOrder::Timestamp);

        buffer.extend(&[stamped("b", Some(10), "early")]);

        assert_eq!(texts(&buffer), ["early", "late"]);
        assert_eq!(buffer.visible_len(), 2);
    }

    #[test]
    fn a_filter_and_an_export_follow_the_sorted_order() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[
            stamped("a", Some(30), "keep late"),
            stamped("a", Some(20), "drop me"),
            stamped("b", Some(10), "keep early"),
        ]);
        buffer.set_order(LogOrder::Timestamp);
        buffer.set_filter(filter("keep"));

        assert_eq!(texts(&buffer), ["keep early", "keep late"]);
        assert_eq!(buffer.to_text(), "b/app keep early\na/app keep late\n");
    }

    #[test]
    fn seeking_finds_a_line_by_its_place_in_the_sorted_order() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[
            stamped("a", Some(10), "a-first"),
            stamped("a", Some(30), "a-second"),
            stamped("b", Some(20), "b-first"),
        ]);
        buffer.set_order(LogOrder::Timestamp);

        // Index 1 in the sorted view is b-first; in arrival order it is
        // a-second, and jumping to the wrong one is the whole bug this guards.
        assert_eq!(buffer.seek(at(20)), Some(1));
        assert_eq!(
            buffer.visible_line(1).map(|line| line.text.to_string()),
            Some("b-first".to_owned())
        );
    }

    #[test]
    fn eviction_does_not_leave_stale_entries_in_the_sorted_order() {
        let mut buffer = LogBuffer::new(2);
        buffer.set_order(LogOrder::Timestamp);
        for seconds in [10, 20, 30, 40] {
            buffer.extend(&[stamped("a", Some(seconds), &format!("line-{seconds}"))]);
        }

        assert_eq!(buffer.visible_len(), 2);
        assert_eq!(texts(&buffer), ["line-30", "line-40"]);
    }

    #[test]
    fn a_relative_time_counts_back_from_now() {
        let now = at(10_000);
        assert_eq!(parse_time("-5m", now), Ok(at(9_700)));
        assert_eq!(parse_time("-30s", now), Ok(at(9_970)));
        assert_eq!(parse_time("-2h", now), Ok(at(2_800)));
        // Surrounding spaces are what a paste leaves behind, not an error.
        assert_eq!(parse_time(" -1d ", at(200_000)), Ok(at(113_600)));
    }

    #[test]
    fn a_wall_clock_time_lands_on_the_same_utc_day() {
        // 1970-01-02T03:04:05Z: a day and change past the epoch.
        let now = at(86_400 + 3 * 3_600 + 4 * 60 + 5);
        assert_eq!(
            parse_time("14:32", now),
            Ok(at(86_400 + 14 * 3_600 + 32 * 60))
        );
        assert_eq!(
            parse_time("14:32:10", now),
            Ok(at(86_400 + 14 * 3_600 + 32 * 60 + 10))
        );
        // Midnight is a time, not an absence of one.
        assert_eq!(parse_time("00:00", now), Ok(at(86_400)));
    }

    #[test]
    fn an_rfc3339_stamp_is_read_as_a_log_line_would_carry_it() {
        let now = at(0);
        // The same instant `crates/cluster/src/logs.rs` parses out of a line.
        assert_eq!(
            parse_time("2026-08-18T10:00:00.123456789Z", now),
            Ok(at(1_787_047_200))
        );
        // A space for the `T`, because that is how log files write it.
        assert_eq!(
            parse_time("2026-08-18 10:00:00", now),
            Ok(at(1_787_047_200))
        );
        // An offset moves it, in the direction that makes 11:00+01:00 the same
        // instant as 10:00Z.
        assert_eq!(
            parse_time("2026-08-18T11:00:00+01:00", now),
            Ok(at(1_787_047_200))
        );
        assert_eq!(
            parse_time("2026-08-18T09:00:00-01:00", now),
            Ok(at(1_787_047_200))
        );
    }

    #[test]
    fn nonsense_in_the_time_field_is_reported_rather_than_guessed_at() {
        let now = at(10_000);
        assert_eq!(parse_time("   ", now), Err(TimeError::Empty));
        assert_eq!(parse_time("soon", now), Err(TimeError::Unrecognised));
        assert_eq!(parse_time("25:00", now), Err(TimeError::Unrecognised));
        assert_eq!(parse_time("14:60", now), Err(TimeError::Unrecognised));
        assert_eq!(parse_time("-5", now), Err(TimeError::Unrecognised));
        assert_eq!(parse_time("-5y", now), Err(TimeError::Unrecognised));
        // A multi-byte last character must be rejected, not split through.
        assert_eq!(parse_time("-5€", now), Err(TimeError::Unrecognised));
        assert_eq!(
            parse_time("2026-02-30T00:00:00Z", now),
            Err(TimeError::Unrecognised)
        );
        assert_eq!(
            parse_time("2026-13-01T00:00:00Z", now),
            Err(TimeError::Unrecognised)
        );
        // Before the epoch there are no log lines, and saying so beats
        // wrapping round to some enormous time.
        assert_eq!(parse_time("-1d", at(60)), Err(TimeError::OutOfRange));
        assert_eq!(
            parse_time("1969-12-31T00:00:00Z", now),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn a_leap_day_is_a_day_and_the_next_year_does_not_have_one() {
        let now = at(0);
        assert!(parse_time("2028-02-29T00:00:00Z", now).is_ok());
        assert_eq!(
            parse_time("2029-02-29T00:00:00Z", now),
            Err(TimeError::Unrecognised)
        );
    }

    #[test]
    fn sources_are_tracked_and_counted() {
        let mut buffer = LogBuffer::default();
        buffer.source_changed(LogSource::new("api-1", "api"), LogSourceState::Streaming);
        buffer.source_changed(LogSource::new("api-0", "api"), LogSourceState::Streaming);
        assert_eq!(buffer.streaming(), 2);

        // Sorted, so the legend does not reshuffle as pods attach.
        assert_eq!(&*buffer.sources()[0].0.pod, "api-0");

        buffer.source_changed(LogSource::new("api-0", "api"), LogSourceState::Ended);
        assert_eq!(buffer.streaming(), 1);
        assert_eq!(buffer.sources().len(), 2);
    }

    #[test]
    fn exported_text_names_the_source_of_every_line() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("api-0", "hello"), line("api-1", "world")]);

        assert_eq!(buffer.to_text(), "api-0/app hello\napi-1/app world\n");
    }

    #[test]
    fn export_follows_the_filter() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("api-0", "hello"), line("api-1", "world")]);
        buffer.set_filter(filter("world"));

        // "Export the visible buffer" means what is on screen, not everything.
        assert_eq!(buffer.to_text(), "api-1/app world\n");
    }

    #[test]
    fn filtering_half_a_million_lines_stays_inside_the_budget() {
        // The Phase 3 budget: a 500k-line buffer filters in under 100ms.
        let mut buffer = LogBuffer::new(500_000);
        let batch: Vec<LogLine> = (0..500_000)
            .map(|index| {
                line(
                    "api-0",
                    &format!("2026-08-18 GET /users/{index} 200 in {index}ms"),
                )
            })
            .collect();
        buffer.extend(&batch);
        assert_eq!(buffer.len(), 500_000);

        let started = std::time::Instant::now();
        buffer.set_filter(filter("/users/499999"));
        let substring = started.elapsed();
        assert_eq!(buffer.visible_len(), 1);

        let started = std::time::Instant::now();
        buffer.set_filter(FilterSpec {
            pattern: r"in \d{6}ms".to_owned(),
            regex: true,
            ..FilterSpec::default()
        });
        let regex = started.elapsed();

        // The budget is about the shipped binary. An unoptimised build runs
        // this scan several times slower, and failing there would only teach
        // people to ignore the test.
        //
        // The debug figure was 2s and the regex scan measures 2.2–2.6s on the
        // machine this was written on, so `cargo test` failed here on a clean
        // checkout — a red test that says nothing about the budget it names is
        // worse than no test. Five seconds is still an order of magnitude
        // clear of the 100ms the release build is held to.
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(5_000)
        } else {
            Duration::from_millis(100)
        };

        assert!(substring < budget, "substring filter took {substring:?}");
        assert!(regex < budget, "regex filter took {regex:?}");
    }

    #[test]
    fn a_case_insensitive_search_handles_non_ascii_without_panicking() {
        let mut buffer = LogBuffer::default();
        buffer.extend(&[line("a", "STRASSE Ärger"), line("a", "nothing")]);

        buffer.set_filter(filter("ärger"));
        assert_eq!(texts(&buffer), ["STRASSE Ärger"]);
    }
}
