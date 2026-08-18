//! The root view.
//!
//! Phase 0 renders the bridge rather than a cluster: connection state, probe
//! round trips and flush counters. That is deliberate — it makes the plumbing
//! visible while there is nothing else to show, and the same panels become the
//! status chrome once real resources arrive.

use std::time::{Duration, Instant};

use gpui::{
    App, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, CommandError, CommandSender, ConnectionState,
    FlushStats,
};
use periscope_config::ThemeChoice;
use periscope_store::ConnectionRegistry;

use crate::theme;

/// Running totals from the event pump, shown in the footer and logged by `--perf`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// Number of flushes applied to the UI.
    pub flushes: u64,
    /// Events applied across all flushes.
    pub applied: u64,
    /// Events superseded before they ever reached the UI.
    pub collapsed: u64,
    /// Events discarded because the channel was full.
    pub dropped: u64,
}

impl BridgeStats {
    fn record(&mut self, stats: FlushStats) {
        self.flushes += 1;
        self.applied += stats.applied as u64;
        self.collapsed += stats.collapsed;
        self.dropped += stats.dropped as u64;
    }
}

/// The application's root view.
#[derive(Debug)]
pub struct Workspace {
    commands: CommandSender,
    connections: ConnectionRegistry,
    stats: BridgeStats,
    theme: ThemeChoice,
    /// Process start, used to report cold-start time on first paint.
    started: Instant,
    cold_start: Option<Duration>,
    next_nonce: u64,
    /// The most recent thing that went wrong, shown verbatim.
    last_error: Option<SharedString>,
}

/// The cluster Phase 0 probes. Phase 1 replaces this with kubeconfig contexts.
const LOCAL: &str = "local";

impl Workspace {
    /// Builds the root view.
    pub fn new(commands: CommandSender, started: Instant, cx: &mut Context<Self>) -> Self {
        let mut connections = ConnectionRegistry::new();
        connections.track(ClusterId::new(LOCAL), Instant::now());
        cx.notify();

        Self {
            commands,
            connections,
            stats: BridgeStats::default(),
            theme: ThemeChoice::default(),
            started,
            cold_start: None,
            next_nonce: 0,
            last_error: None,
        }
    }

    /// Applies one coalesced batch from the bridge. This is the only entry point
    /// for cluster data into the UI.
    pub fn apply_events(
        &mut self,
        events: Vec<ClusterEvent>,
        stats: FlushStats,
        cx: &mut Context<Self>,
    ) {
        self.stats.record(stats);
        let now = Instant::now();
        self.connections.apply_batch(events.iter(), now);
        cx.notify();
    }

    /// Running bridge counters.
    pub fn stats(&self) -> BridgeStats {
        self.stats
    }

    fn send_probe(&mut self, cx: &mut Context<Self>) {
        self.next_nonce += 1;
        let command = ClusterCommand::Ping {
            cluster: ClusterId::new(LOCAL),
            nonce: self.next_nonce,
        };

        match self.commands.send(command) {
            Ok(()) => self.last_error = None,
            Err(error) => {
                // Never swallow this: a button that silently did nothing is the
                // exact failure mode the error-handling rules forbid.
                tracing::error!(%error, "probe command was not queued");
                self.last_error = Some(SharedString::from(error_text(error)));
            }
        }
        cx.notify();
    }

    fn cycle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = self.theme.next();
        theme::apply(self.theme, Some(window), cx);
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .px_5()
            .py_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_2xl()
                            .text_color(cx.theme().foreground)
                            .child("Periscope"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("A native Kubernetes console"),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("probe")
                            .primary()
                            .small()
                            .label("Send probe")
                            .on_click(cx.listener(|this, _, _, cx| this.send_probe(cx))),
                    )
                    .child(
                        Button::new("theme")
                            .outline()
                            .small()
                            .label(format!("Theme: {}", self.theme.label()))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cycle_theme(window, cx)),
                            ),
                    ),
            )
    }

    fn connections_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .connections
            .iter()
            .map(|(id, connection)| {
                let accent = state_color(&connection.state, cx);
                let detail = connection
                    .state
                    .detail()
                    .map(str::to_owned)
                    .or_else(|| {
                        connection
                            .last_round_trip
                            .map(|rtt| format!("last probe {:.1}ms", rtt.as_secs_f64() * 1_000.0))
                    })
                    .unwrap_or_else(|| "no probe yet".to_owned());

                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(div().size(px(8.)).rounded_full().bg(accent))
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().foreground)
                                            .child(id.to_string()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(detail),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(accent)
                            .child(connection.state.label()),
                    )
            })
            .collect();

        panel("Clusters", cx).children(rows)
    }

    fn bridge_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stats = self.stats;
        panel("Bridge", cx).child(
            h_flex()
                .w_full()
                .px_4()
                .py_3()
                .gap_6()
                .child(metric("flushes", stats.flushes.to_string(), cx))
                .child(metric("events applied", stats.applied.to_string(), cx))
                .child(metric("coalesced", stats.collapsed.to_string(), cx))
                .child(metric("dropped", stats.dropped.to_string(), cx)),
        )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cold_start = self
            .cold_start
            .map(|d| format!("cold start {}ms", d.as_millis()))
            .unwrap_or_else(|| "measuring cold start".to_owned());

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!(
                "Phase 0 · read-only · v{}",
                env!("CARGO_PKG_VERSION")
            ))
            .child(cold_start)
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.cold_start.is_none() {
            let elapsed = self.started.elapsed();
            self.cold_start = Some(elapsed);
            tracing::info!(cold_start_ms = elapsed.as_millis() as u64, "first paint");
        }

        let error_banner = self.last_error.clone().map(|message| {
            div()
                .w_full()
                .px_5()
                .py_3()
                .bg(cx.theme().danger)
                .text_sm()
                .text_color(cx.theme().danger_foreground)
                .child(message)
        });

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.header(cx))
            .children(error_banner)
            .child(
                v_flex()
                    .flex_1()
                    .w_full()
                    .gap_4()
                    .p_5()
                    .child(self.connections_panel(cx))
                    .child(self.bridge_panel(cx)),
            )
            .child(self.footer(cx))
    }
}

/// A titled card.
fn panel(title: &'static str, cx: &App) -> gpui::Div {
    v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .child(
            div()
                .px_4()
                .py_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .border_b_1()
                .border_color(cx.theme().border)
                .child(title),
        )
}

/// A label-over-value statistic.
fn metric(label: &'static str, value: String, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(value),
        )
}

/// The colour that carries connection state at a glance.
fn state_color(state: &ConnectionState, cx: &App) -> gpui::Hsla {
    match state {
        ConnectionState::Connected => cx.theme().success,
        ConnectionState::Connecting => cx.theme().info,
        ConnectionState::Degraded { .. } => cx.theme().warning,
        ConnectionState::AuthFailed { .. } => cx.theme().danger,
        ConnectionState::Disconnected { reason: Some(_) } => cx.theme().danger,
        _ => cx.theme().muted_foreground,
    }
}

/// Turns a command failure into text that says what to do about it.
fn error_text(error: CommandError) -> String {
    match error {
        CommandError::Backpressure => {
            "The cluster runtime is saturated and did not accept the command. Retry in a moment."
                .to_owned()
        }
        CommandError::Closed => {
            "The cluster runtime has stopped. Restart Periscope to reconnect.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use periscope_bridge::command_channel;

    fn workspace(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, periscope_bridge::CommandReceiver) {
        let (tx, rx) = command_channel(8);
        let started = Instant::now();
        let view = cx.new(|cx| Workspace::new(tx, started, cx));
        (view, rx)
    }

    #[gpui::test]
    fn a_batch_updates_connection_state(cx: &mut TestAppContext) {
        let (view, _rx) = workspace(cx);

        view.update(cx, |workspace, cx| {
            workspace.apply_events(
                vec![ClusterEvent::Status {
                    cluster: ClusterId::new(LOCAL),
                    state: ConnectionState::Connected,
                }],
                FlushStats {
                    applied: 1,
                    collapsed: 3,
                    dropped: 0,
                },
                cx,
            );
        });

        view.read_with(cx, |workspace, _| {
            assert_eq!(workspace.stats().flushes, 1);
            assert_eq!(workspace.stats().collapsed, 3);
            assert_eq!(
                workspace
                    .connections
                    .get(&ClusterId::new(LOCAL))
                    .unwrap()
                    .state,
                ConnectionState::Connected
            );
        });
    }

    #[gpui::test]
    fn probes_are_queued_with_increasing_nonces(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);

        view.update(cx, |workspace, cx| {
            workspace.send_probe(cx);
            workspace.send_probe(cx);
        });

        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv()).collect();

        assert_eq!(
            sent,
            vec![
                ClusterCommand::Ping {
                    cluster: ClusterId::new(LOCAL),
                    nonce: 1
                },
                ClusterCommand::Ping {
                    cluster: ClusterId::new(LOCAL),
                    nonce: 2
                },
            ]
        );
        view.read_with(cx, |workspace, _| assert!(workspace.last_error.is_none()));
    }

    #[gpui::test]
    fn a_dead_runtime_surfaces_an_error_rather_than_doing_nothing(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);
        drop(rx);

        view.update(cx, |workspace, cx| workspace.send_probe(cx));

        view.read_with(cx, |workspace, _| {
            let message = workspace.last_error.as_ref().expect("error surfaced");
            assert!(message.contains("Restart Periscope"), "{message}");
        });
    }
}
