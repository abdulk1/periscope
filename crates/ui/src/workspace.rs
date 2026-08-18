//! The root view.
//!
//! It owns the [`AppState`] and renders it: the context picker on the left, the
//! pod table on the right, and connection state everywhere it matters. It sends
//! commands and reads state; it never talks to Kubernetes and never decides
//! what is true.

use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, CommandError, CommandSender, ConnectionState,
    FlushStats,
};
use periscope_config::ThemeChoice;
use periscope_store::AppState;

use crate::{format, table, theme};

/// How often the view repaints with no events, so the age column keeps moving.
const TICK: Duration = Duration::from_secs(1);

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
pub struct Workspace {
    commands: CommandSender,
    state: AppState,
    stats: BridgeStats,
    theme: ThemeChoice,
    /// Process start, used to report cold-start time on first paint.
    started: Instant,
    cold_start: Option<Duration>,
    /// Clusters this session has already tried to connect to, so a repainting
    /// UI cannot spam the runtime with connect commands.
    attempted: HashSet<ClusterId>,
    /// The most recent thing that went wrong locally, shown verbatim.
    last_error: Option<SharedString>,
    /// Repaints the age column while nothing else is happening.
    _ticker: Task<()>,
}

impl fmt::Debug for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("active", &self.state.active())
            .field("contexts", &self.state.contexts().len())
            .field("rows", &self.state.rows().len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Builds the root view and asks the cluster layer what contexts exist.
    pub fn new(commands: CommandSender, started: Instant, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });

        let mut workspace = Self {
            commands,
            state: AppState::new(),
            stats: BridgeStats::default(),
            theme: ThemeChoice::default(),
            started,
            cold_start: None,
            attempted: HashSet::new(),
            last_error: None,
            _ticker: ticker,
        };
        workspace.send(ClusterCommand::ListContexts);
        workspace
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
        self.state.apply_batch(events.iter(), Instant::now());

        // Reading kubeconfig picks a cluster; opening it is what the user
        // actually asked for by starting the app.
        if let Some(active) = self.state.active().cloned() {
            self.connect_once(active);
        }

        cx.notify();
    }

    /// Running bridge counters.
    pub fn stats(&self) -> BridgeStats {
        self.stats
    }

    /// The state being rendered.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Switches to a cluster, connecting to it if this session has not yet.
    fn select(&mut self, cluster: ClusterId, cx: &mut Context<Self>) {
        self.state.select(cluster.clone());
        self.connect_once(cluster);
        cx.notify();
    }

    /// Connects to a cluster once per session. Reconnecting is explicit.
    fn connect_once(&mut self, cluster: ClusterId) {
        if self.attempted.insert(cluster.clone()) {
            self.send(ClusterCommand::Connect { cluster });
        }
    }

    /// Retries the active cluster after a failure.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(cluster) = self.state.active().cloned() {
            self.attempted.insert(cluster.clone());
            self.send(ClusterCommand::Connect { cluster });
        }
        cx.notify();
    }

    /// Re-reads kubeconfig.
    fn reload_contexts(&mut self, cx: &mut Context<Self>) {
        self.send(ClusterCommand::ListContexts);
        cx.notify();
    }

    fn cycle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = self.theme.next();
        theme::apply(self.theme, Some(window), cx);
        cx.notify();
    }

    /// Queues a command, surfacing a failure rather than swallowing it.
    fn send(&mut self, command: ClusterCommand) {
        match self.commands.send(command) {
            Ok(()) => self.last_error = None,
            Err(error) => {
                // A button that silently did nothing is the exact failure mode
                // the error-handling rules forbid.
                tracing::error!(%error, "command was not queued");
                self.last_error = Some(SharedString::from(error_text(error)));
            }
        }
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let subtitle = match self.state.active() {
            Some(cluster) => format!("{cluster}"),
            None => "no cluster selected".to_owned(),
        };

        h_flex()
            .w_full()
            .flex_none()
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
                            .child(subtitle),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("reload")
                            .outline()
                            .small()
                            .label("Reload contexts")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_contexts(cx))),
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

    /// The context picker.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active().cloned();

        let rows: Vec<_> = self
            .state
            .contexts()
            .iter()
            .map(|context| {
                let id = ClusterId::new(&*context.name);
                let selected = active.as_ref() == Some(&id);
                let connection = self.state.connection(&id);
                let state = connection
                    .map(|c| &c.state)
                    .unwrap_or(&ConnectionState::Idle);
                let pods = self.state.pod_count(&id);

                let detail = match (state.detail(), pods) {
                    (Some(reason), _) => reason.to_owned(),
                    (None, 0) => state.label().to_owned(),
                    (None, count) => format!("{count} pods"),
                };

                let click_id = id.clone();
                div()
                    .id(SharedString::from(context.name.to_string()))
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .when(selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(click_id.clone(), cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.)).rounded_full().bg(state_color(state, cx)))
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().foreground)
                                            .child(context.name.to_string()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(detail),
                                    ),
                            ),
                    )
            })
            .collect();

        let empty = rows.is_empty().then(|| {
            div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("no contexts in kubeconfig")
        });

        v_flex()
            .w(px(240.))
            .flex_none()
            .h_full()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("CONTEXTS"),
            )
            .children(rows)
            .children(empty)
    }

    /// The banner that explains whatever is currently wrong.
    fn banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (message, retry) = if let Some(error) = self.last_error.clone() {
            (error.to_string(), false)
        } else if let Some(error) = self.state.config_error() {
            (format!("kubeconfig: {error}"), false)
        } else {
            let connection = self.state.active_connection()?;
            if !connection.state.is_problem() {
                return None;
            }
            let cluster = self.state.active()?;
            let detail = connection.state.detail().unwrap_or("no detail available");
            (
                format!("{cluster}: {} — {detail}", connection.state.label()),
                true,
            )
        };

        Some(
            h_flex()
                .w_full()
                .flex_none()
                .items_center()
                .justify_between()
                .gap_4()
                .px_5()
                .py_3()
                .bg(cx.theme().danger)
                .text_sm()
                .text_color(cx.theme().danger_foreground)
                .child(div().flex_1().child(message))
                .children(retry.then(|| {
                    Button::new("reconnect")
                        .small()
                        .label("Reconnect")
                        .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx)))
                })),
        )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (pods, ready) = self.state.active_counts();
        let connection = self.state.active_connection();
        let state = connection
            .map(|c| c.state.label())
            .unwrap_or("no cluster selected");

        let stale = connection
            .filter(|connection| connection.is_stale())
            .map(|connection| format!("· {} events dropped", connection.dropped_events));

        let cold_start = self
            .cold_start
            .map(|elapsed| format!("cold start {}ms", elapsed.as_millis()))
            .unwrap_or_else(|| "measuring cold start".to_owned());

        let round_trip = connection
            .and_then(|connection| connection.last_round_trip)
            .map(|rtt| format!("· probe {}", format::millis(rtt)));

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                h_flex()
                    .gap_2()
                    .child(format!("{pods} pods · {ready} ready · {state}"))
                    .children(stale)
                    .children(round_trip),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(format!(
                        "flushes {} · coalesced {} · dropped {}",
                        self.stats.flushes, self.stats.collapsed, self.stats.dropped
                    ))
                    .child(cold_start),
            )
    }

    /// The table, or an explanation of why there is no table.
    fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.state.rows_shared();

        let body = if rows.is_empty() {
            let message = match self.state.active_connection().map(|c| &c.state) {
                None => "Select a context to connect.".to_owned(),
                Some(ConnectionState::Connecting) => "Connecting…".to_owned(),
                Some(ConnectionState::Idle) => "Not connected.".to_owned(),
                // A failure is already in the banner; do not repeat the reason,
                // but never leave the table looking merely empty.
                Some(state) if state.is_problem() => {
                    format!("No pods to show — the cluster is {}.", state.label())
                }
                Some(_) => "No pods in this cluster.".to_owned(),
            };
            table::placeholder(message, cx).into_any_element()
        } else {
            table::body(rows, SystemTime::now()).into_any_element()
        };

        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(table::header(cx))
            .child(body)
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.cold_start.is_none() {
            let elapsed = self.started.elapsed();
            self.cold_start = Some(elapsed);
            tracing::info!(cold_start_ms = elapsed.as_millis() as u64, "first paint");
        }

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.header(cx))
            .children(self.banner(cx))
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(self.content(cx)),
            )
            .child(self.footer(cx))
    }
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
    use periscope_bridge::{
        CommandReceiver, ContextInfo, PodSnapshot, ResourceKey, command_channel,
    };
    use std::sync::Arc;

    fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, CommandReceiver) {
        let (tx, rx) = command_channel(16);
        let view = cx.new(|cx| Workspace::new(tx, Instant::now(), cx));
        (view, rx)
    }

    fn context(name: &str) -> ContextInfo {
        ContextInfo {
            name: Arc::from(name),
            cluster: Arc::from("cluster"),
            user: None,
            namespace: None,
        }
    }

    fn contexts(names: &[&str], current: &str) -> ClusterEvent {
        ClusterEvent::Contexts {
            contexts: names.iter().copied().map(context).collect(),
            current: Some(ClusterId::new(current)),
        }
    }

    fn pod(name: &str) -> PodSnapshot {
        PodSnapshot {
            key: ResourceKey::new("default", name),
            uid: None,
            status: Arc::from("Running"),
            ready: 1,
            containers: 1,
            restarts: 0,
            node: None,
            created: None,
        }
    }

    fn apply(view: &Entity<Workspace>, cx: &mut TestAppContext, events: Vec<ClusterEvent>) {
        view.update(cx, |workspace, cx| {
            workspace.apply_events(events, FlushStats::default(), cx);
        });
    }

    fn drain(rx: &CommandReceiver) -> Vec<ClusterCommand> {
        std::iter::from_fn(|| rx.try_recv()).collect()
    }

    #[gpui::test]
    fn the_view_asks_for_contexts_as_soon_as_it_opens(cx: &mut TestAppContext) {
        let (_view, rx) = workspace(cx);
        assert_eq!(drain(&rx), vec![ClusterCommand::ListContexts]);
    }

    #[gpui::test]
    fn the_current_context_is_connected_to_exactly_once(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);
        drain(&rx);

        apply(&view, cx, vec![contexts(&["prod", "staging"], "prod")]);
        // A second batch must not re-issue the connect: repainting is not a
        // reason to open another set of watches.
        apply(&view, cx, vec![contexts(&["prod", "staging"], "prod")]);

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Connect {
                cluster: ClusterId::new("prod")
            }]
        );
    }

    #[gpui::test]
    fn selecting_a_context_switches_the_table_and_connects(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);
        apply(&view, cx, vec![contexts(&["prod", "staging"], "prod")]);
        apply(
            &view,
            cx,
            vec![
                ClusterEvent::PodsReset {
                    cluster: "prod".into(),
                    pods: Arc::from([pod("prod-pod")]),
                },
                ClusterEvent::PodsReset {
                    cluster: "staging".into(),
                    pods: Arc::from([pod("staging-pod")]),
                },
            ],
        );
        drain(&rx);

        view.update(cx, |workspace, cx| {
            workspace.select(ClusterId::new("staging"), cx);
        });

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Connect {
                cluster: ClusterId::new("staging")
            }]
        );
        view.read_with(cx, |workspace, _| {
            let rows = workspace.state().rows();
            assert_eq!(rows.len(), 1);
            assert_eq!(&*rows[0].key.name, "staging-pod");
        });
    }

    #[gpui::test]
    fn pods_arriving_for_the_active_cluster_become_rows(cx: &mut TestAppContext) {
        let (view, _rx) = workspace(cx);
        apply(&view, cx, vec![contexts(&["prod"], "prod")]);
        apply(
            &view,
            cx,
            vec![ClusterEvent::PodApplied {
                cluster: "prod".into(),
                pod: Arc::new(pod("api-0")),
            }],
        );

        view.read_with(cx, |workspace, _| {
            assert_eq!(workspace.state().rows().len(), 1);
            assert_eq!(workspace.stats().flushes, 2);
        });
    }

    #[gpui::test]
    fn an_auth_failure_is_offered_with_a_retry(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);
        apply(&view, cx, vec![contexts(&["prod"], "prod")]);
        apply(
            &view,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::AuthFailed {
                    reason: "exec plugin `aws` exited 255".into(),
                },
            }],
        );
        drain(&rx);

        view.update(cx, |workspace, cx| workspace.reconnect(cx));

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Connect {
                cluster: ClusterId::new("prod")
            }]
        );
        view.read_with(cx, |workspace, _| {
            let connection = workspace.state().active_connection().unwrap();
            assert_eq!(
                connection.state.detail(),
                Some("exec plugin `aws` exited 255")
            );
        });
    }

    #[gpui::test]
    fn a_dead_runtime_surfaces_an_error_rather_than_doing_nothing(cx: &mut TestAppContext) {
        let (view, rx) = workspace(cx);
        drop(rx);

        view.update(cx, |workspace, cx| workspace.reload_contexts(cx));

        view.read_with(cx, |workspace, _| {
            let message = workspace.last_error.as_ref().expect("error surfaced");
            assert!(message.contains("Restart Periscope"), "{message}");
        });
    }

    #[gpui::test]
    fn a_kubeconfig_failure_is_reported_verbatim(cx: &mut TestAppContext) {
        let (view, _rx) = workspace(cx);
        apply(
            &view,
            cx,
            vec![ClusterEvent::ConfigFailed {
                reason: "open /home/x/.kube/config: permission denied".into(),
            }],
        );

        view.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.state().config_error(),
                Some("open /home/x/.kube/config: permission denied")
            );
        });
    }
}
