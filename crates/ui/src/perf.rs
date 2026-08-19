//! Frame timing for `--perf`.
//!
//! GPUI only draws when something invalidates the window, so an idle app has no
//! frame rate to measure and a quiet one measures as "perfect". To get a number
//! that means anything, `--perf` puts the window into continuous redraw — every
//! frame rebuilds the whole view, including the visible rows of the table — and
//! this meter records the interval between consecutive renders.
//!
//! That is a **stress measurement**, deliberately pessimistic: scrolling rebuilds
//! the visible rows too, but only while the user is actually scrolling, and it
//! never rebuilds the chrome around them. Treat the numbers as a floor.

use std::time::{Duration, Instant};

/// How many frames to gather before logging a report.
const REPORT_EVERY: usize = 120;

/// Rolling frame-time statistics.
#[derive(Debug, Default)]
pub struct FrameMeter {
    enabled: bool,
    last_render: Option<Instant>,
    /// Interval between consecutive renders, in the current report window.
    intervals: Vec<Duration>,
    /// Time spent building the element tree, in the current report window.
    builds: Vec<Duration>,
    /// Time spent folding batches from the cluster layer into the store, in the
    /// current report window.
    ///
    /// Separate from `builds` because it is not part of a frame: it runs on the
    /// same thread, between frames, and a long one shows up as a dropped frame
    /// with nothing in the frame's own numbers to explain it.
    applies: Vec<Duration>,
    /// Frames measured since the process started.
    total: u64,
}

/// One report window's worth of frame statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStats {
    /// Frames in this report.
    pub frames: usize,
    /// Median interval between renders.
    pub p50: Duration,
    /// 95th percentile interval.
    pub p95: Duration,
    /// Worst interval.
    pub max: Duration,
    /// Median time spent inside `render` — every frame's own work, from the
    /// first statement to the finished element tree. It excludes GPUI's layout
    /// and paint, which happen after `render` returns, so a healthy `build_p50`
    /// beside a bad `p50` means the cost is not in this crate.
    pub build_p50: Duration,
    /// Median time spent applying one batch from the cluster layer.
    pub apply_p50: Duration,
    /// The worst batch in this window. A single batch that takes longer than a
    /// frame is a dropped frame, however cheap the frame itself was.
    pub apply_max: Duration,
    /// How many batches were applied in this window.
    pub applies: usize,
    /// Intervals longer than the 60fps budget of 16.67ms.
    ///
    /// On a vsync-locked 60Hz display this sits near half the frames whatever
    /// the app does, because vsync jitter puts each interval microseconds
    /// either side of the budget. It is the raw count, not a verdict.
    pub over_budget: usize,
    /// Intervals long enough that a frame was definitely dropped — over twice
    /// the 60fps budget. This is the number that corresponds to visible stutter.
    pub hitches: usize,
}

impl FrameStats {
    /// Frames per second implied by the median interval.
    pub fn fps(&self) -> f64 {
        let seconds = self.p50.as_secs_f64();
        if seconds > 0.0 {
            1.0 / seconds
        } else {
            f64::NAN
        }
    }
}

/// The interval a frame must beat to hold 60fps.
pub const SIXTY_FPS: Duration = Duration::from_nanos(16_666_667);

impl FrameMeter {
    /// A meter that records only when `--perf` is on.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// Whether frame timing is being collected.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Total frames measured.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Records how long one batch from the cluster layer took to fold in.
    ///
    /// Costs nothing when `--perf` is off, so the caller can time
    /// unconditionally only if timing is free; it is not, so the caller checks
    /// [`FrameMeter::enabled`] first.
    pub fn applied(&mut self, took: Duration) {
        if self.enabled {
            self.applies.push(took);
        }
    }

    /// Records the start of a render, returning the instant to hand back to
    /// [`FrameMeter::finish`]. `None` when timing is off, so the whole thing
    /// costs nothing in a normal run.
    pub fn start(&mut self, now: Instant) -> Option<Instant> {
        if !self.enabled {
            return None;
        }

        if let Some(last) = self.last_render {
            self.intervals.push(now.saturating_duration_since(last));
        }
        self.last_render = Some(now);
        self.total += 1;
        Some(now)
    }

    /// Records how long the element tree took to build, and returns a report
    /// once enough frames have accumulated.
    pub fn finish(&mut self, started: Option<Instant>, now: Instant) -> Option<FrameStats> {
        let started = started?;
        self.builds.push(now.saturating_duration_since(started));

        if self.intervals.len() < REPORT_EVERY {
            return None;
        }

        let stats = self.stats();
        self.intervals.clear();
        self.builds.clear();
        self.applies.clear();
        Some(stats)
    }

    fn stats(&self) -> FrameStats {
        let mut intervals = self.intervals.clone();
        intervals.sort_unstable();
        let mut builds = self.builds.clone();
        builds.sort_unstable();
        let mut applies = self.applies.clone();
        applies.sort_unstable();

        FrameStats {
            frames: intervals.len(),
            p50: percentile(&intervals, 0.50),
            p95: percentile(&intervals, 0.95),
            max: intervals.last().copied().unwrap_or_default(),
            build_p50: percentile(&builds, 0.50),
            apply_p50: percentile(&applies, 0.50),
            apply_max: applies.last().copied().unwrap_or_default(),
            applies: applies.len(),
            over_budget: intervals
                .iter()
                .filter(|interval| **interval > SIXTY_FPS)
                .count(),
            hitches: intervals
                .iter()
                .filter(|interval| **interval > SIXTY_FPS * 2)
                .count(),
        }
    }
}

/// Nearest-rank percentile of a sorted slice.
fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meter() -> FrameMeter {
        FrameMeter::new(true)
    }

    /// Feeds `count` frames spaced `interval` apart, each taking `build` to draw.
    fn run(
        meter: &mut FrameMeter,
        count: usize,
        interval: Duration,
        build: Duration,
    ) -> Option<FrameStats> {
        let mut now = Instant::now();
        let mut report = None;
        for _ in 0..count {
            let started = meter.start(now);
            report = meter.finish(started, now + build).or(report);
            now += interval;
        }
        report
    }

    #[test]
    fn a_disabled_meter_records_nothing() {
        let mut meter = FrameMeter::new(false);
        assert!(run(&mut meter, 500, Duration::from_millis(8), Duration::ZERO).is_none());
        meter.applied(Duration::from_millis(50));
        assert_eq!(meter.total(), 0);
    }

    #[test]
    fn a_slow_batch_is_reported_separately_from_the_frame() {
        // A batch is folded in between frames, on the same thread, so a slow
        // one is a dropped frame that nothing in the frame's own numbers
        // explains. That was a real unexplained stutter before it was measured.
        let mut meter = meter();
        meter.applied(Duration::from_millis(4));
        meter.applied(Duration::from_millis(40));
        meter.applied(Duration::from_millis(6));

        let stats = run(
            &mut meter,
            REPORT_EVERY + 1,
            Duration::from_millis(16),
            Duration::from_micros(200),
        )
        .expect("a report");

        assert_eq!(stats.applies, 3);
        assert_eq!(stats.apply_p50, Duration::from_millis(6));
        assert_eq!(stats.apply_max, Duration::from_millis(40));
        // The frame's own build time is untouched by it, which is the point.
        assert_eq!(stats.build_p50, Duration::from_micros(200));
    }

    #[test]
    fn a_report_arrives_once_enough_frames_have_passed() {
        let mut meter = meter();
        // One fewer interval than frames: the first frame has nothing to
        // measure against.
        assert!(
            run(
                &mut meter,
                REPORT_EVERY,
                Duration::from_millis(8),
                Duration::ZERO
            )
            .is_none()
        );

        let report = run(&mut meter, 2, Duration::from_millis(8), Duration::ZERO)
            .expect("a report once the window fills");
        assert_eq!(report.frames, REPORT_EVERY);
    }

    #[test]
    fn a_steady_eight_millisecond_frame_reads_as_125fps() {
        let mut meter = meter();
        let report = run(
            &mut meter,
            REPORT_EVERY + 2,
            Duration::from_millis(8),
            Duration::from_micros(300),
        )
        .expect("a report");

        assert_eq!(report.p50, Duration::from_millis(8));
        assert!((report.fps() - 125.0).abs() < 0.01, "{}", report.fps());
        assert_eq!(report.over_budget, 0);
        assert_eq!(report.hitches, 0);
        assert_eq!(report.build_p50, Duration::from_micros(300));
    }

    #[test]
    fn frames_that_miss_sixty_fps_are_counted() {
        let mut meter = meter();
        let report = run(
            &mut meter,
            REPORT_EVERY + 2,
            Duration::from_millis(20),
            Duration::ZERO,
        )
        .expect("a report");

        // Every frame is over budget, and that has to be visible in the report
        // rather than averaged away. 20ms is late but not late enough to have
        // skipped a frame, so it is not a hitch.
        assert_eq!(report.over_budget, report.frames);
        assert_eq!(report.hitches, 0);
        assert!(report.fps() < 60.0);

        let mut stalling = FrameMeter::new(true);
        let stalled = run(
            &mut stalling,
            REPORT_EVERY + 2,
            Duration::from_millis(40),
            Duration::ZERO,
        )
        .expect("a report");
        assert_eq!(stalled.hitches, stalled.frames);
    }

    #[test]
    fn the_worst_frame_survives_the_percentiles() {
        let mut meter = meter();
        let mut now = Instant::now();
        let mut report = None;
        for index in 0..=REPORT_EVERY {
            let started = meter.start(now);
            report = meter.finish(started, now).or(report);
            // One catastrophic frame among fast ones.
            now += if index == 40 {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(4)
            };
        }
        let report = report.expect("a report");

        assert_eq!(report.p50, Duration::from_millis(4));
        assert_eq!(report.max, Duration::from_millis(250));
        assert_eq!(report.over_budget, 1);
        assert_eq!(report.hitches, 1);
    }

    #[test]
    fn a_vsync_locked_sixty_hertz_display_reads_as_sixty_fps_without_hitches() {
        // Real intervals from a 60Hz panel: they straddle the budget by
        // microseconds, so half of them count as "over budget" while nothing is
        // actually being dropped. `hitches` is what must stay at zero.
        let mut meter = meter();
        let mut now = Instant::now();
        let mut report = None;
        for index in 0..=REPORT_EVERY {
            let started = meter.start(now);
            report = meter.finish(started, now).or(report);
            now += if index % 2 == 0 {
                Duration::from_micros(16_600)
            } else {
                Duration::from_micros(16_720)
            };
        }
        let report = report.expect("a report");

        assert!((report.fps() - 60.0).abs() < 0.5, "{}", report.fps());
        assert!(report.over_budget > 0);
        assert_eq!(report.hitches, 0);
    }

    #[test]
    fn percentiles_of_an_empty_series_are_zero_rather_than_a_panic() {
        assert_eq!(percentile(&[], 0.5), Duration::ZERO);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let series: Vec<_> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&series, 0.50), Duration::from_millis(50));
        assert_eq!(percentile(&series, 0.95), Duration::from_millis(95));
    }
}
