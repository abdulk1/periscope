//! The root view.
//!
//! It owns the [`AppState`] and renders it: contexts and kinds on the left, the
//! resource table in the middle, the detail pane on the right, and connection
//! state everywhere it matters. It sends commands and reads state; it never
//! talks to Kubernetes and never decides what is true.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, actions, div, px,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, CommandError, CommandSender, ConnectionState,
    FlushStats, KindId, ResourceKey,
};
use periscope_config::ThemeChoice;
use periscope_store::{AppState, Detail};

use crate::palette::{Palette, Target};
use crate::perf::FrameMeter;
use crate::{table, theme};

actions!(
    periscope,
    [
        /// Open or close the fuzzy jump palette.
        TogglePalette,
        /// Close whatever is on top: the palette, then the detail pane.
        Dismiss,
        /// Move down the palette's results.
        SelectNext,
        /// Move up the palette's results.
        SelectPrevious,
        /// Jump to the highlighted palette result.
        Confirm,
    ]
);

/// Registers Periscope's own key bindings.
///
/// Called once at startup, after `gpui_component::init`, which binds the keys
/// the text inputs need.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-k", TogglePalette, None),
        KeyBinding::new("ctrl-k", TogglePalette, None),
        KeyBinding::new("escape", Dismiss, None),
        KeyBinding::new("down", SelectNext, Some("Palette")),
        KeyBinding::new("up", SelectPrevious, Some("Palette")),
        KeyBinding::new("enter", Confirm, Some("Palette")),
    ]);
}

/// How often the view repaints with no events, so the age column keeps moving.
const TICK: Duration = Duration::from_secs(1);

/// Everything that decides which watch should be running.
///
/// Compared as a whole: any change to it means the current stream is watching
/// the wrong thing and has to be replaced.
type WatchTarget = (ClusterId, KindId, Option<Arc<str>>, Option<Arc<str>>);

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
    /// The watch currently running, so the same one is not started twice.
    watching: Option<WatchTarget>,
    /// The most recent thing that went wrong locally, shown verbatim.
    last_error: Option<SharedString>,

    namespace_input: Entity<InputState>,
    selector_input: Entity<InputState>,
    search_input: Entity<InputState>,
    /// The YAML pane, a read-only syntax-highlighted editor.
    yaml_view: Entity<InputState>,
    /// Which object's YAML the editor currently holds.
    yaml_showing: Option<ResourceKey>,

    palette: Palette,
    palette_open: bool,
    palette_input: Entity<InputState>,
    palette_matches: Vec<crate::palette::Match>,
    palette_index: usize,
    palette_focus: FocusHandle,

    /// Frame timing, collected only under `--perf`.
    frames: FrameMeter,
    _subscriptions: Vec<Subscription>,
    /// Repaints the age column while nothing else is happening.
    _ticker: Task<()>,
}

impl fmt::Debug for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("cluster", &self.state.active())
            .field("kind", &self.state.kind())
            .field("rows", &self.state.rows().len())
            .field("palette_open", &self.palette_open)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Builds the root view and asks the cluster layer what contexts exist.
    ///
    /// `perf` turns on continuous redraw and frame timing; see [`crate::perf`]
    /// for why measuring a frame rate needs the app to stop idling.
    pub fn new(
        commands: CommandSender,
        started: Instant,
        perf: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });

        let namespace_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("namespace (all)"));
        let selector_input = cx.new(|cx| InputState::new(window, cx).placeholder("label selector"));
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter rows"));
        let palette_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("jump to a kind or object"));
        let yaml_view = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .code_editor("yaml")
                .soft_wrap(false)
        });

        let subscriptions = vec![
            cx.subscribe(&search_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    this.state.set_search(Some(Arc::from(value.as_str())));
                    cx.notify();
                }
            }),
            // Namespace and selector re-list from the apiserver, so they apply
            // on Enter rather than on every keystroke.
            cx.subscribe(&namespace_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_server_filters(cx);
                }
            }),
            cx.subscribe(&selector_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_server_filters(cx);
                }
            }),
            cx.subscribe(&palette_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    this.palette_matches = this.palette.search(&query);
                    this.palette_index = 0;
                    cx.notify();
                }
            }),
        ];

        // The root element has to be focused for its key bindings to be in the
        // dispatch path; nothing else claims focus until the palette opens.
        let root_focus = cx.focus_handle();
        window.focus(&root_focus);

        let mut workspace = Self {
            commands,
            state: AppState::new(),
            stats: BridgeStats::default(),
            theme: ThemeChoice::default(),
            started,
            cold_start: None,
            attempted: HashSet::new(),
            watching: None,
            last_error: None,
            namespace_input,
            selector_input,
            search_input,
            yaml_view,
            yaml_showing: None,
            palette: Palette::new(),
            palette_open: false,
            palette_input,
            palette_matches: Vec::new(),
            palette_index: 0,
            palette_focus: root_focus,
            frames: FrameMeter::new(perf),
            _subscriptions: subscriptions,
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

        // Discovery decides which kind can be opened, so the first watch can
        // only start once the kinds have arrived.
        if self.state.kind().is_none()
            && let Some(default) = self.default_kind()
        {
            self.state.select_kind(default);
        }
        self.ensure_watch();

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

    /// Whether the palette is open.
    pub fn palette_open(&self) -> bool {
        self.palette_open
    }

    /// Pods if the cluster serves them, otherwise the first watchable kind.
    fn default_kind(&self) -> Option<KindId> {
        let kinds = self.state.kinds();
        kinds
            .iter()
            .find(|info| info.id.is_core() && &*info.id.kind == "Pod")
            .or_else(|| kinds.iter().find(|info| info.watchable))
            .map(|info| info.id.clone())
    }

    /// Starts the watch the current view needs, if it is not already running.
    fn ensure_watch(&mut self) {
        let (Some(cluster), Some(kind)) =
            (self.state.active().cloned(), self.state.kind().cloned())
        else {
            return;
        };

        let namespace = self.state.filters().namespace.clone();
        let selector = self.state.filters().selector.clone();
        let wanted = (
            cluster.clone(),
            kind.clone(),
            namespace.clone(),
            selector.clone(),
        );
        if self.watching.as_ref() == Some(&wanted) {
            return;
        }

        // One watch at a time: leaving the previous kind streaming would keep
        // paying for data nothing is rendering.
        if let Some((previous_cluster, previous_kind, ..)) = self.watching.take()
            && (previous_cluster != cluster || previous_kind != kind)
        {
            self.send(ClusterCommand::StopWatch {
                cluster: previous_cluster,
                kind: previous_kind,
            });
        }

        self.send(ClusterCommand::Watch {
            cluster,
            kind,
            namespace,
            selector,
        });
        self.watching = Some(wanted);
    }

    /// Reads the namespace and selector inputs and re-lists with them.
    fn apply_server_filters(&mut self, cx: &mut Context<Self>) {
        let namespace = self.namespace_input.read(cx).value().to_string();
        let selector = self.selector_input.read(cx).value().to_string();

        self.state
            .set_namespace(Some(Arc::from(namespace.as_str())));
        self.state.set_selector(Some(Arc::from(selector.as_str())));
        self.ensure_watch();
        cx.notify();
    }

    /// Switches to a cluster, connecting to it if this session has not yet.
    fn select_cluster(&mut self, cluster: ClusterId, cx: &mut Context<Self>) {
        self.state.select_cluster(cluster.clone());
        self.connect_once(cluster.clone());

        // The new cluster's kinds may not have arrived; the watch starts when
        // they do.
        if let Some(kind) = self.default_kind() {
            self.state.select_kind(kind);
        }
        self.ensure_watch();
        cx.notify();
    }

    /// Switches which kind the table shows.
    fn select_kind(&mut self, kind: KindId, cx: &mut Context<Self>) {
        self.state.select_kind(kind);
        self.ensure_watch();
        cx.notify();
    }

    /// Opens an object in the detail pane and asks for its YAML.
    pub fn open_object(&mut self, key: ResourceKey, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some(cluster), Some(kind)) =
            (self.state.active().cloned(), self.state.kind().cloned())
        else {
            return;
        };

        self.state.open_detail(kind.clone(), key.clone());
        self.send(ClusterCommand::FetchObject {
            cluster,
            kind,
            key,
            // Secrets open masked. Revealing is a separate, deliberate act.
            reveal: false,
        });
        cx.notify();
    }

    /// Re-fetches the object on screen with its secret values shown.
    fn reveal(&mut self, cx: &mut Context<Self>) {
        let (Some(cluster), Some(detail)) = (self.state.active().cloned(), self.state.detail())
        else {
            return;
        };
        let (kind, key) = (detail.kind().clone(), detail.key().clone());

        tracing::info!(%cluster, %kind, %key, "revealing secret values");
        self.state.open_detail(kind.clone(), key.clone());
        self.yaml_showing = None;
        self.send(ClusterCommand::FetchObject {
            cluster,
            kind,
            key,
            reveal: true,
        });
        cx.notify();
    }

    /// Jumps to an object of a kind other than the one on screen — what an
    /// owner reference or a palette hit does.
    fn open_elsewhere(&mut self, kind: KindId, key: ResourceKey, cx: &mut Context<Self>) {
        self.state.select_kind(kind.clone());
        self.ensure_watch();

        if let Some(cluster) = self.state.active().cloned() {
            self.state.open_detail(kind.clone(), key.clone());
            self.send(ClusterCommand::FetchObject {
                cluster,
                kind,
                key,
                reveal: false,
            });
        }
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
            self.watching = None;
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

    // --- palette -----------------------------------------------------------

    fn toggle_palette(&mut self, _: &TogglePalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(cx);
            return;
        }

        let clusters: Vec<Arc<str>> = self
            .state
            .contexts()
            .iter()
            .map(|context| Arc::clone(&context.name))
            .collect();
        self.palette.rebuild(
            &clusters,
            self.state.kinds(),
            self.state.kind(),
            self.state.rows(),
        );
        self.palette_matches = self.palette.search("");
        self.palette_index = 0;
        self.palette_open = true;

        self.palette_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        window.focus(&self.palette_input.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_matches.clear();
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(cx);
        } else if self.state.detail().is_some() {
            self.state.close_detail();
            self.yaml_showing = None;
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open && !self.palette_matches.is_empty() {
            self.palette_index = (self.palette_index + 1).min(self.palette_matches.len() - 1);
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_open {
            self.palette_index = self.palette_index.saturating_sub(1);
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        let Some(found) = self.palette_matches.get(self.palette_index).cloned() else {
            return;
        };

        self.close_palette(cx);
        match found.candidate.target {
            Target::Cluster(name) => self.select_cluster(ClusterId::new(&*name), cx),
            Target::Kind(kind) => self.select_kind(kind, cx),
            Target::Object { kind, key } => {
                if self.state.kind() == Some(&kind) {
                    self.open_object(key, window, cx);
                } else {
                    self.open_elsewhere(kind, key, cx);
                }
            }
        }
    }

    // --- rendering ---------------------------------------------------------

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let subtitle = match (self.state.active(), self.state.kind()) {
            (Some(cluster), Some(kind)) => format!("{cluster} · {kind}"),
            (Some(cluster), None) => cluster.to_string(),
            _ => "no cluster selected".to_owned(),
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
                        Button::new("palette")
                            .outline()
                            .small()
                            .label("Jump  ⌘K")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_palette(&TogglePalette, window, cx);
                            })),
                    )
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

    /// Contexts, then the kinds the active cluster serves.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active().cloned();

        let contexts: Vec<_> = self
            .state
            .contexts()
            .iter()
            .map(|context| {
                let id = ClusterId::new(&*context.name);
                let selected = active.as_ref() == Some(&id);
                let connection = self.state.connection(&id);
                let state = connection
                    .map(|connection| &connection.state)
                    .unwrap_or(&ConnectionState::Idle);
                let click_id = id.clone();

                div()
                    .id(SharedString::from(format!("context-{}", context.name)))
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .when(selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_cluster(click_id.clone(), cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.)).rounded_full().bg(state_color(state, cx)))
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(cx.theme().foreground)
                                    .child(context.name.to_string()),
                            ),
                    )
            })
            .collect();

        let current_kind = self.state.kind().cloned();
        let kinds: Vec<_> = self
            .state
            .kinds()
            .iter()
            .filter(|info| info.watchable)
            .map(|info| {
                let selected = current_kind.as_ref() == Some(&info.id);
                let count = self.state.row_count(&info.id);
                let click_kind = info.id.clone();

                div()
                    .id(SharedString::from(format!("kind-{}", info.id.label())))
                    .w_full()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .when(selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_kind(click_kind.clone(), cx);
                    }))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(if info.custom {
                                        cx.theme().info
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .child(info.id.label()),
                            )
                            .children((count > 0).then(|| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(count.to_string())
                            })),
                    )
            })
            .collect();

        let kinds_empty = kinds.is_empty().then(|| {
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("connect to discover kinds")
        });

        v_flex()
            .id("sidebar")
            .w(px(240.))
            .flex_none()
            .h_full()
            .gap_1()
            .p_2()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(section("CONTEXTS", cx))
            .children(contexts)
            .child(section("KINDS", cx))
            .children(kinds)
            .children(kinds_empty)
    }

    /// Namespace, selector and text filters.
    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (total, shown) = self.state.counts();
        let namespaces = self.state.namespaces().len();

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(px(200.))
                    .child(Input::new(&self.namespace_input).small()),
            )
            .child(
                div()
                    .w(px(220.))
                    .child(Input::new(&self.selector_input).small()),
            )
            .child(
                div()
                    .w(px(220.))
                    .child(Input::new(&self.search_input).small()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if total == shown {
                        format!("{total} shown · {namespaces} namespaces")
                    } else {
                        format!("{shown} of {total} shown · {namespaces} namespaces")
                    }),
            )
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
        let connection = self.state.active_connection();
        let state = connection
            .map(|connection| connection.state.label())
            .unwrap_or("no cluster selected");

        let stale = connection
            .filter(|connection| connection.is_stale())
            .map(|connection| format!("· {} events dropped", connection.dropped_events));

        let cold_start = self
            .cold_start
            .map(|elapsed| format!("cold start {}ms", elapsed.as_millis()))
            .unwrap_or_else(|| "measuring cold start".to_owned());

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
                    .child(format!("{} kinds · {state}", self.state.kinds().len()))
                    .children(stale),
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
        let columns: Arc<[periscope_bridge::ColumnSpec]> = Arc::from(self.state.columns().to_vec());
        let namespaced = self
            .state
            .kind()
            .and_then(|kind| self.state.kind_info(kind))
            .is_none_or(|info| info.namespaced);

        let body = if rows.is_empty() {
            let message = match self.state.active_connection().map(|c| &c.state) {
                None => "Select a context to connect.".to_owned(),
                Some(ConnectionState::Connecting) => "Connecting…".to_owned(),
                Some(ConnectionState::Idle) => "Not connected.".to_owned(),
                // A failure is already in the banner; do not repeat the reason,
                // but never leave the table looking merely empty.
                Some(state) if state.is_problem() => {
                    format!("No rows to show — the cluster is {}.", state.label())
                }
                Some(_) if self.state.counts().0 > 0 => {
                    "Nothing matches the current filter.".to_owned()
                }
                Some(_) => match self.state.kind() {
                    Some(kind) => format!("No {kind} here."),
                    None => "No kind selected.".to_owned(),
                },
            };
            table::placeholder(message, cx).into_any_element()
        } else {
            table::body(
                cx.entity(),
                rows,
                columns.clone(),
                namespaced,
                self.state.detail().map(|detail| detail.key().clone()),
                SystemTime::now(),
            )
            .into_any_element()
        };

        v_flex()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(self.toolbar(cx))
            .child(table::header(&columns, namespaced, cx))
            .child(body)
    }

    /// The detail pane: YAML, events and owners for one object.
    fn detail_pane(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let detail = self.state.detail()?;
        let title = format!("{} · {}", detail.kind(), detail.key());

        // Secrets are masked until asked for; the button says which state it is
        // in, so nobody has to guess whether they are looking at real values.
        let reveal_label = match detail {
            Detail::Ready { object, .. } if object.maskable => Some(if object.revealed {
                "Values shown"
            } else {
                "Reveal values"
            }),
            _ => None,
        };

        let masked_note = match detail {
            Detail::Ready { object, .. } if object.maskable && !object.revealed => {
                Some("values hidden")
            }
            _ => None,
        };

        let body = match detail {
            Detail::Loading { .. } => div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Loading…")
                .into_any_element(),
            Detail::Failed { reason, .. } => div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(reason.clone())
                .into_any_element(),
            Detail::Ready { object, .. } => {
                let owners: Vec<_> = object
                    .owners
                    .iter()
                    .map(|owner| {
                        let label = format!("{}/{}", owner.kind, owner.name);
                        let kind = owner_kind(owner);
                        let key = ResourceKey::new(&*detail.key().namespace, &*owner.name);

                        Button::new(SharedString::from(format!("owner-{label}")))
                            .outline()
                            .small()
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_elsewhere(kind.clone(), key.clone(), cx);
                            }))
                    })
                    .collect();

                let events: Vec<_> = object
                    .events
                    .iter()
                    .rev()
                    .take(20)
                    .map(|event| {
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_start()
                            .text_xs()
                            .child(
                                div()
                                    .w(px(70.))
                                    .flex_none()
                                    .text_color(if event.is_warning() {
                                        cx.theme().danger
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(event.reason.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(cx.theme().foreground)
                                    .child(event.message.to_string()),
                            )
                            .children((event.count > 1).then(|| {
                                div()
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("×{}", event.count))
                            }))
                    })
                    .collect();

                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .children((!owners.is_empty()).then(|| {
                        h_flex()
                            .w_full()
                            .flex_none()
                            .gap_2()
                            .items_center()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("OWNED BY"),
                            )
                            .children(owners)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .p_2()
                            .child(Input::new(&self.yaml_view).h_full()),
                    )
                    .children((!events.is_empty()).then(|| {
                        v_flex()
                            .w_full()
                            .flex_none()
                            .max_h(px(180.))
                            .id("events")
                            .overflow_y_scroll()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("EVENTS"),
                            )
                            .children(events)
                    }))
                    .into_any_element()
            }
        };

        Some(
            v_flex()
                .w(px(560.))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .w_full()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .truncate()
                                        .text_color(cx.theme().foreground)
                                        .child(title),
                                )
                                .children(masked_note.map(|note| {
                                    div()
                                        .text_xs()
                                        .flex_none()
                                        .text_color(cx.theme().warning)
                                        .child(note)
                                })),
                        )
                        .children(reveal_label.map(|label| {
                            Button::new("reveal")
                                .outline()
                                .small()
                                .label(label)
                                .on_click(cx.listener(|this, _, _, cx| this.reveal(cx)))
                        }))
                        .child(
                            Button::new("close-detail")
                                .outline()
                                .small()
                                .label("Close")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.dismiss(&Dismiss, window, cx);
                                })),
                        ),
                )
                .child(body),
        )
    }

    /// The fuzzy jump palette, drawn over everything else.
    fn palette_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.palette_open {
            return None;
        }

        let results: Vec<_> = self
            .palette_matches
            .iter()
            .enumerate()
            .take(12)
            .map(|(index, found)| {
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .when(index == self.palette_index, |row| row.bg(cx.theme().accent))
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(cx.theme().foreground)
                            .child(found.candidate.label.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .child(found.candidate.detail.clone()),
                    )
            })
            .collect();

        let empty = self.palette_matches.is_empty().then(|| {
            div()
                .px_3()
                .py_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("no matches")
        });

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(gpui::black().opacity(0.4))
                .child(
                    v_flex()
                        .absolute()
                        .top(px(120.))
                        .left_1_2()
                        .ml(px(-320.))
                        .w(px(640.))
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .child(
                            div()
                                .p_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(Input::new(&self.palette_input)),
                        )
                        .child(v_flex().p_2().gap_0p5().children(results).children(empty)),
                ),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.cold_start.is_none() {
            let elapsed = self.started.elapsed();
            self.cold_start = Some(elapsed);
            tracing::info!(cold_start_ms = elapsed.as_millis() as u64, "first paint");
        }

        // The YAML editor is a separate entity that needs a `Window` to be
        // written to, which the event pump does not have; render time is where
        // the two meet.
        if let Some(Detail::Ready { object, .. }) = self.state.detail() {
            if self.yaml_showing.as_ref() != Some(&object.key) {
                let yaml = object.yaml.to_string();
                self.yaml_showing = Some(object.key.clone());
                self.yaml_view.update(cx, |input, cx| {
                    input.set_value(yaml, window, cx);
                });
            }
        } else if self.yaml_showing.is_some() {
            self.yaml_showing = None;
        }

        let frame = self.frames.start(Instant::now());
        if frame.is_some() {
            // Keep frames coming, so there is a continuous series to measure.
            // Nothing else in the app redraws when nothing has changed.
            window.request_animation_frame();
        }

        let rendered = div()
            .relative()
            .size_full()
            // The palette's own keys — up, down, enter — only bind while it is
            // open, so Enter in a filter field is not swallowed.
            .key_context(if self.palette_open {
                "Palette"
            } else {
                "Periscope"
            })
            .track_focus(&self.palette_focus)
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .child(
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
                            .child(self.content(cx))
                            .children(self.detail_pane(cx)),
                    )
                    .child(self.footer(cx)),
            )
            .children(self.palette_overlay(cx));

        if let Some(stats) = self.frames.finish(frame, Instant::now()) {
            tracing::info!(
                frames = stats.frames,
                fps = format!("{:.1}", stats.fps()),
                p50_ms = format!("{:.2}", stats.p50.as_secs_f64() * 1_000.0),
                p95_ms = format!("{:.2}", stats.p95.as_secs_f64() * 1_000.0),
                max_ms = format!("{:.2}", stats.max.as_secs_f64() * 1_000.0),
                build_p50_us = stats.build_p50.as_micros() as u64,
                over_budget = stats.over_budget,
                hitches = stats.hitches,
                rows = self.state.rows().len(),
                "frames"
            );
        }

        rendered
    }
}

/// A heading in the sidebar.
fn section(title: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_3()
        .py_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(title)
}

/// The kind an owner reference points at.
///
/// Owner references carry `apiVersion` and `kind` but not the plural the API
/// path needs, so it is derived the way Kubernetes itself does by convention:
/// lowercase the kind and add `s`. Kinds ending in `s` (`Ingress`) take `es`,
/// and `y` becomes `ies` (`NetworkPolicy`).
fn owner_kind(owner: &periscope_bridge::OwnerRef) -> KindId {
    let (group, version) = match owner.api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", &*owner.api_version),
    };

    let lower = owner.kind.to_lowercase();
    let plural = if let Some(stem) = lower.strip_suffix('y') {
        format!("{stem}ies")
    } else if lower.ends_with('s') || lower.ends_with("ch") || lower.ends_with("sh") {
        format!("{lower}es")
    } else {
        format!("{lower}s")
    };

    KindId::new(group, version, &*owner.kind, plural)
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
    use gpui::{TestAppContext, WindowHandle};
    use gpui_component::Root;
    use periscope_bridge::{
        ColumnSpec, CommandReceiver, ContextInfo, KindInfo, ResourceRow, RowState, command_channel,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A window built exactly the way the binary builds it.
    ///
    /// The view renders as soon as the window opens, and both the theme and
    /// `gpui-component`'s key handling live under its `Root`, so a test that
    /// skips either diverges from the real app in ways that hide bugs.
    struct Harness {
        window: WindowHandle<Root>,
        workspace: Entity<Workspace>,
    }

    impl Harness {
        fn read<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&Workspace) -> R) -> R {
            self.workspace.read_with(cx, |workspace, _| f(workspace))
        }

        /// Runs something that needs a `Window`, as a click or a key press does.
        fn update<R>(
            &self,
            cx: &mut TestAppContext,
            f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
        ) -> R {
            let workspace = self.workspace.clone();
            self.window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| f(workspace, window, cx))
                })
                .expect("the window is open")
        }

        /// Keystrokes go through GPUI's dispatch, the same path a user's do.
        fn keys(&self, cx: &mut TestAppContext, keystrokes: &str) {
            let mut window = gpui::VisualTestContext::from_window(self.window.into(), cx);
            window.simulate_keystrokes(keystrokes);
        }
    }

    fn workspace(cx: &mut TestAppContext) -> (Harness, CommandReceiver) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::init(cx);
        });

        let (tx, rx) = command_channel(64);
        let slot: Rc<RefCell<Option<Entity<Workspace>>>> = Rc::new(RefCell::new(None));
        let captured = Rc::clone(&slot);

        let window = cx.add_window(|window, cx| {
            let workspace = cx.new(|cx| Workspace::new(tx, Instant::now(), false, window, cx));
            *captured.borrow_mut() = Some(workspace.clone());
            Root::new(workspace, window, cx)
        });

        let workspace = slot.borrow_mut().take().expect("the workspace was built");
        (Harness { window, workspace }, rx)
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

    fn pods() -> KindId {
        KindId::new("", "v1", "Pod", "pods")
    }

    fn deployments() -> KindId {
        KindId::new("apps", "v1", "Deployment", "deployments")
    }

    fn kinds_event(cluster: &str) -> ClusterEvent {
        ClusterEvent::Kinds {
            cluster: cluster.into(),
            kinds: Arc::from([
                KindInfo {
                    id: pods(),
                    namespaced: true,
                    watchable: true,
                    custom: false,
                },
                KindInfo {
                    id: deployments(),
                    namespaced: true,
                    watchable: true,
                    custom: false,
                },
            ]),
        }
    }

    fn row(name: &str) -> ResourceRow {
        ResourceRow {
            key: ResourceKey::new("default", name),
            uid: None,
            cells: Arc::from([Arc::from("Running")]),
            state: RowState::Healthy,
            created: None,
        }
    }

    fn reset(cluster: &str, kind: KindId, names: &[&str]) -> ClusterEvent {
        ClusterEvent::ResourceReset {
            cluster: cluster.into(),
            kind,
            columns: Arc::from([ColumnSpec::fixed("STATUS", 100)]),
            rows: names.iter().map(|name| row(name)).collect(),
        }
    }

    fn apply(harness: &Harness, cx: &mut TestAppContext, events: Vec<ClusterEvent>) {
        harness.update(cx, |workspace, _window, cx| {
            workspace.apply_events(events, FlushStats::default(), cx);
        });
    }

    fn drain(rx: &CommandReceiver) -> Vec<ClusterCommand> {
        std::iter::from_fn(|| rx.try_recv()).collect()
    }

    #[gpui::test]
    fn the_view_asks_for_contexts_as_soon_as_it_opens(cx: &mut TestAppContext) {
        let (_harness, rx) = workspace(cx);
        assert_eq!(drain(&rx), vec![ClusterCommand::ListContexts]);
    }

    #[gpui::test]
    fn the_current_context_is_connected_to_exactly_once(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        drain(&rx);

        apply(&harness, cx, vec![contexts(&["prod", "staging"], "prod")]);
        // A second batch must not re-issue the connect: repainting is not a
        // reason to open another set of watches.
        apply(&harness, cx, vec![contexts(&["prod", "staging"], "prod")]);

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Connect {
                cluster: ClusterId::new("prod")
            }]
        );
    }

    #[gpui::test]
    fn discovery_starts_a_watch_on_pods(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        drain(&rx);

        apply(&harness, cx, vec![kinds_event("prod")]);

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Watch {
                cluster: ClusterId::new("prod"),
                kind: pods(),
                namespace: None,
                selector: None,
            }]
        );
    }

    #[gpui::test]
    fn switching_kinds_stops_the_previous_watch(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(deployments(), cx);
        });

        // Leaving the old watch running would keep paying for data nothing
        // renders.
        assert_eq!(
            drain(&rx),
            vec![
                ClusterCommand::StopWatch {
                    cluster: ClusterId::new("prod"),
                    kind: pods(),
                },
                ClusterCommand::Watch {
                    cluster: ClusterId::new("prod"),
                    kind: deployments(),
                    namespace: None,
                    selector: None,
                },
            ]
        );
    }

    #[gpui::test]
    fn rows_arriving_for_the_active_kind_become_the_table(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![reset("prod", pods(), &["api-0", "api-1"])],
        );

        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().rows().len(), 2);
            assert_eq!(&*workspace.state().columns()[0].name, "STATUS");
        });
    }

    #[gpui::test]
    fn clicking_a_row_asks_for_that_object(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(&harness, cx, vec![reset("prod", pods(), &["api-0"])]);
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.open_object(ResourceKey::new("default", "api-0"), window, cx);
        });

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::FetchObject {
                cluster: ClusterId::new("prod"),
                kind: pods(),
                key: ResourceKey::new("default", "api-0"),
                reveal: false,
            }]
        );
        harness.read(cx, |workspace| {
            assert!(matches!(
                workspace.state().detail(),
                Some(periscope_store::Detail::Loading { .. })
            ));
        });
    }

    #[gpui::test]
    fn cmd_k_opens_the_palette_and_escape_closes_it(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, "cmd-k");
        assert!(harness.read(cx, |workspace| workspace.palette_open()));

        harness.keys(cx, "escape");
        assert!(!harness.read(cx, |workspace| workspace.palette_open()));
    }

    #[gpui::test]
    fn typing_in_the_palette_and_pressing_enter_switches_kinds(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, "cmd-k");
        harness.keys(cx, "d e p");
        harness.keys(cx, "enter");

        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().kind(), Some(&deployments()));
            assert!(!workspace.palette_open());
        });
    }

    #[gpui::test]
    fn arrow_keys_move_through_the_palette_results(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, "cmd-k");
        let first = harness.read(cx, |workspace| workspace.palette_index);
        harness.keys(cx, "down down");
        let moved = harness.read(cx, |workspace| workspace.palette_index);
        harness.keys(cx, "up");
        let back = harness.read(cx, |workspace| workspace.palette_index);

        assert_eq!(first, 0);
        assert_eq!(moved, 2);
        assert_eq!(back, 1);
    }

    #[gpui::test]
    fn the_palette_jumps_to_a_kind(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.toggle_palette(&TogglePalette, window, cx);
            workspace.palette_matches = workspace.palette.search("deployments");
            workspace.palette_index = 0;
            workspace.confirm(&Confirm, window, cx);
        });

        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().kind(), Some(&deployments()));
            assert!(!workspace.palette_open());
        });
        assert!(drain(&rx).contains(&ClusterCommand::Watch {
            cluster: ClusterId::new("prod"),
            kind: deployments(),
            namespace: None,
            selector: None,
        }));
    }

    #[gpui::test]
    fn escape_closes_the_palette_before_the_detail_pane(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        harness.update(cx, |workspace, window, cx| {
            workspace.open_object(ResourceKey::new("default", "api-0"), window, cx);
            workspace.toggle_palette(&TogglePalette, window, cx);

            workspace.dismiss(&Dismiss, window, cx);
            assert!(!workspace.palette_open());
            // The detail pane is still open: one Escape, one dismissal.
            assert!(workspace.state().detail().is_some());

            workspace.dismiss(&Dismiss, window, cx);
            assert!(workspace.state().detail().is_none());
        });
    }

    #[gpui::test]
    fn revealing_a_secret_re_fetches_it_with_the_values(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        let key = ResourceKey::new("default", "db");
        harness.update(cx, |workspace, window, cx| {
            workspace.open_object(key.clone(), window, cx);
        });
        // Opening never asks for the values.
        assert!(drain(&rx).contains(&ClusterCommand::FetchObject {
            cluster: ClusterId::new("prod"),
            kind: pods(),
            key: key.clone(),
            reveal: false,
        }));

        harness.update(cx, |workspace, _window, cx| workspace.reveal(cx));

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::FetchObject {
                cluster: ClusterId::new("prod"),
                kind: pods(),
                key,
                reveal: true,
            }]
        );
    }

    #[gpui::test]
    fn an_auth_failure_is_offered_with_a_retry(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::AuthFailed {
                    reason: "exec plugin `aws` exited 255".into(),
                },
            }],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| workspace.reconnect(cx));

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Connect {
                cluster: ClusterId::new("prod")
            }]
        );
    }

    #[gpui::test]
    fn a_dead_runtime_surfaces_an_error_rather_than_doing_nothing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        drop(rx);

        harness.update(cx, |workspace, _window, cx| workspace.reload_contexts(cx));

        harness.read(cx, |workspace| {
            let message = workspace.last_error.as_ref().expect("error surfaced");
            assert!(message.contains("Restart Periscope"), "{message}");
        });
    }

    #[gpui::test]
    fn a_kubeconfig_failure_is_reported_verbatim(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![ClusterEvent::ConfigFailed {
                reason: "open /home/x/.kube/config: permission denied".into(),
            }],
        );

        harness.read(cx, |workspace| {
            assert_eq!(
                workspace.state().config_error(),
                Some("open /home/x/.kube/config: permission denied")
            );
        });
    }

    #[test]
    fn owner_kinds_are_pluralised_the_way_kubernetes_does() {
        let owner = |api_version: &str, kind: &str| periscope_bridge::OwnerRef {
            api_version: Arc::from(api_version),
            kind: Arc::from(kind),
            name: Arc::from("x"),
            controller: true,
        };

        assert_eq!(
            owner_kind(&owner("apps/v1", "ReplicaSet")),
            KindId::new("apps", "v1", "ReplicaSet", "replicasets")
        );
        assert_eq!(
            owner_kind(&owner("v1", "Pod")),
            KindId::new("", "v1", "Pod", "pods")
        );
        assert_eq!(
            owner_kind(&owner("networking.k8s.io/v1", "NetworkPolicy")),
            KindId::new(
                "networking.k8s.io",
                "v1",
                "NetworkPolicy",
                "networkpolicies"
            )
        );
        assert_eq!(
            &*owner_kind(&owner("networking.k8s.io/v1", "Ingress")).plural,
            "ingresses"
        );
    }
}
