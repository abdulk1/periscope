//! The GPUI side of the bridge.
//!
//! A single GPUI background task drains the event channel, coalesces what it
//! finds, and applies one batch per flush interval on the foreground. This is
//! the only place cluster events are allowed to touch the UI.

use std::time::{Duration, Instant};

use gpui::{App, AsyncApp, Task};

use crate::channel::EventStream;
use crate::coalesce::Coalescer;
use crate::protocol::{ClusterEvent, EventKey};

/// Default flush cadence. One batch per ~60Hz frame; the spec's starting point.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// Default cap on a single batch, so one flush cannot monopolise a frame.
pub const DEFAULT_MAX_BATCH: usize = 4_096;

/// Tuning for [`spawn_event_pump`].
#[derive(Clone, Copy, Debug)]
pub struct PumpConfig {
    /// How long to gather events before applying them to the UI.
    pub flush_interval: Duration,
    /// Flush early once this many distinct events are pending.
    pub max_batch: usize,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            max_batch: DEFAULT_MAX_BATCH,
        }
    }
}

/// Counters for one flush, for the `--perf` log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushStats {
    /// Events handed to the UI in this batch.
    pub applied: usize,
    /// Events absorbed by a newer event for the same key.
    pub collapsed: u64,
    /// Events the channel discarded because it was full.
    pub dropped: usize,
}

/// Starts the drain loop. The returned [`Task`] must be kept alive; dropping it
/// stops the pump.
///
/// `apply` runs on the main thread with a `&mut App`, once per flush, and is
/// handed every event in the batch plus the batch's counters.
pub fn spawn_event_pump<F>(
    stream: EventStream,
    config: PumpConfig,
    cx: &mut App,
    mut apply: F,
) -> Task<()>
where
    F: FnMut(Vec<ClusterEvent>, FlushStats, &mut App) + 'static,
{
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut coalescer: Coalescer<EventKey, ClusterEvent> =
            Coalescer::new(config.flush_interval, config.max_batch);

        loop {
            // Block until there is something to do at all. An idle app costs
            // nothing: no timer wakeups, no polling.
            let Some(first) = stream.recv().await else {
                break;
            };
            coalescer.push(first.coalesce_key(), first, Instant::now());
            drain_ready(&stream, &mut coalescer);

            // Gather everything that lands inside the flush window.
            while !coalescer.is_ready(Instant::now()) {
                cx.background_executor().timer(config.flush_interval).await;
                drain_ready(&stream, &mut coalescer);
            }

            let collapsed = coalescer.collapsed();
            let batch = coalescer.drain();
            let stats = FlushStats {
                applied: batch.len(),
                collapsed,
                dropped: stream.take_dropped(),
            };

            if stats.dropped > 0 {
                tracing::warn!(
                    dropped = stats.dropped,
                    "event channel overflowed; UI data may be stale"
                );
            }

            if cx.update(|cx| apply(batch, stats, cx)).is_err() {
                // The app is shutting down.
                break;
            }
        }
    })
}

/// Moves everything already queued into the coalescer without waiting.
fn drain_ready(stream: &EventStream, coalescer: &mut Coalescer<EventKey, ClusterEvent>) {
    let now = Instant::now();
    while let Some(event) = stream.try_recv() {
        coalescer.push(event.coalesce_key(), event, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::event_channel;
    use crate::protocol::ConnectionState;
    use crate::runtime::{ClusterHandlerFixture, ClusterRuntime, RuntimeConfig};
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, TestAppContext,
        Window, div,
    };

    /// A rendered entity, so the test proves the message reaches something the
    /// UI actually draws rather than a bare data holder.
    struct Readout {
        pongs: Vec<u64>,
        state: Option<ConnectionState>,
        flushes: usize,
        collapsed: u64,
    }

    impl Readout {
        fn new() -> Self {
            Self {
                pongs: Vec::new(),
                state: None,
                flushes: 0,
                collapsed: 0,
            }
        }

        fn apply(&mut self, batch: Vec<ClusterEvent>, stats: FlushStats) {
            self.flushes += 1;
            self.collapsed += stats.collapsed;
            for event in batch {
                match event {
                    ClusterEvent::Pong { nonce, .. } => self.pongs.push(nonce),
                    ClusterEvent::Status { state, .. } => self.state = Some(state),
                    _ => {}
                }
            }
        }
    }

    impl Render for Readout {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(format!(
                "{} pongs / {}",
                self.pongs.len(),
                self.state
                    .as_ref()
                    .map(ConnectionState::label)
                    .unwrap_or("unknown")
            ))
        }
    }

    fn start_pump(cx: &mut TestAppContext, stream: EventStream) -> (Entity<Readout>, Task<()>) {
        let readout = cx.new(|_| Readout::new());
        let handle = readout.clone();
        let task = cx.update(|cx| {
            spawn_event_pump(
                stream,
                PumpConfig {
                    flush_interval: Duration::from_millis(1),
                    max_batch: DEFAULT_MAX_BATCH,
                },
                cx,
                move |batch, stats, cx| {
                    handle.update(cx, |readout, cx| {
                        readout.apply(batch, stats);
                        cx.notify();
                    });
                },
            )
        });
        (readout, task)
    }

    /// Drives the pump until `done` holds, or gives up after `timeout`.
    ///
    /// Two clocks are in play. GPUI's test executor uses virtual time, so the
    /// pump's flush timer only fires when we advance it. The cluster runtime is
    /// a real OS thread, so it needs real wall-clock time to answer. Advancing
    /// the virtual clock alone spins through thousands of iterations in
    /// microseconds and the tokio thread never gets scheduled — hence the real
    /// sleep as well.
    fn settle_until(
        cx: &mut TestAppContext,
        timeout: Duration,
        mut done: impl FnMut(&mut TestAppContext) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            cx.executor().advance_clock(Duration::from_millis(2));
            cx.run_until_parked();
            if done(cx) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Drives the pump for a fixed spell, for tests asserting nothing happens.
    fn settle(cx: &mut TestAppContext) {
        settle_until(cx, Duration::from_millis(200), |_| false);
    }

    #[gpui::test]
    async fn a_tokio_message_crosses_into_gpui_and_mutates_a_rendered_entity(
        cx: &mut TestAppContext,
    ) {
        let (runtime, stream) =
            ClusterRuntime::start(ClusterHandlerFixture::default(), RuntimeConfig::default())
                .expect("runtime starts");
        let (readout, _pump) = start_pump(cx, stream);

        runtime
            .send(crate::protocol::ClusterCommand::Ping {
                cluster: "prod".into(),
                nonce: 7,
            })
            .expect("command queued");

        let probe = readout.clone();
        let arrived = settle_until(cx, Duration::from_secs(10), move |cx| {
            probe.read_with(cx, |readout, _| !readout.pongs.is_empty())
        });
        assert!(arrived, "the pong never reached the entity");

        readout.read_with(cx, |readout, _| assert_eq!(readout.pongs, vec![7]));
    }

    #[gpui::test]
    async fn a_resync_storm_collapses_into_few_flushes(cx: &mut TestAppContext) {
        let (sink, stream) = event_channel(16_384);
        let (readout, _pump) = start_pump(cx, stream);

        // 5,000 status updates for the same cluster: the UI must see the final
        // state, not 5,000 mutations.
        for i in 0..5_000 {
            sink.send(ClusterEvent::Status {
                cluster: "prod".into(),
                state: if i == 4_999 {
                    ConnectionState::Connected
                } else {
                    ConnectionState::Connecting
                },
            });
        }

        let probe = readout.clone();
        let arrived = settle_until(cx, Duration::from_secs(10), move |cx| {
            probe.read_with(cx, |readout, _| readout.state.is_some())
        });
        assert!(arrived, "the storm never reached the entity");

        readout.read_with(cx, |readout, _| {
            assert_eq!(readout.state, Some(ConnectionState::Connected));
            assert!(
                readout.collapsed > 4_000,
                "expected heavy collapsing, saw {}",
                readout.collapsed
            );
            assert!(
                readout.flushes < 100,
                "expected few flushes, saw {}",
                readout.flushes
            );
        });
    }

    #[gpui::test]
    async fn an_idle_pump_never_flushes(cx: &mut TestAppContext) {
        let (_sink, stream) = event_channel(8);
        let (readout, _pump) = start_pump(cx, stream);

        settle(cx);

        readout.read_with(cx, |readout, _| {
            assert_eq!(readout.flushes, 0, "an idle bridge must not wake the UI");
        });
    }
}
