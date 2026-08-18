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
use std::sync::Arc;

use periscope_bridge::{LogLine, LogSource, LogSourceState};
use regex::{Regex, RegexBuilder};

/// Lines kept per session before the oldest are dropped.
pub const DEFAULT_CAPACITY: usize = 100_000;

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
    /// Sequence numbers of the lines the current filter admits.
    visible: VecDeque<u64>,
    capacity: usize,
    dropped: u64,
    spec: FilterSpec,
    matcher: Matcher,
    sources: Vec<(LogSource, LogSourceState)>,
    /// Why the session failed, if it did.
    error: Option<String>,
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
            capacity: capacity.max(1),
            dropped: 0,
            spec: FilterSpec::default(),
            matcher: Matcher::All,
            sources: Vec::new(),
            error: None,
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
            return;
        }

        for (offset, line) in self.lines.iter().enumerate() {
            if self.matcher.matches(line) {
                self.visible.push_back(self.first + offset as u64);
            }
        }
    }

    /// The filter currently applied.
    pub fn filter(&self) -> &FilterSpec {
        &self.spec
    }

    /// Why the filter pattern is not usable, if it is not.
    pub fn filter_error(&self) -> Option<&str> {
        self.matcher.error()
    }

    /// The line at a visible index.
    pub fn visible_line(&self, index: usize) -> Option<&Arc<LogLine>> {
        let seq = *self.visible.get(index)?;
        self.lines.get((seq - self.first) as usize)
    }

    /// Every visible line, in order.
    pub fn visible(&self) -> impl Iterator<Item = &Arc<LogLine>> {
        self.visible
            .iter()
            .filter_map(|&seq| self.lines.get((seq - self.first) as usize))
    }

    /// A snapshot of the visible lines, for a virtualised list.
    pub fn visible_lines(&self) -> Vec<Arc<LogLine>> {
        self.visible().cloned().collect()
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
    pub fn seek(&self, at: std::time::SystemTime) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

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
        let budget = if cfg!(debug_assertions) {
            std::time::Duration::from_millis(2_000)
        } else {
            std::time::Duration::from_millis(100)
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
