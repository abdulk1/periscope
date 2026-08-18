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
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::resizable::{h_resizable, resizable_panel};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, CommandError, CommandSender, ConnectionState,
    ExecTarget, FlushStats, ForwardId, ForwardTarget, KindId, LogTarget, Mutation, ResourceKey,
};
use periscope_config::ThemeChoice;
use periscope_store::{AppState, Detail, FilterSpec};

use crate::palette::{Palette, Target};
// The two predicates that decide which actions a kind offers live with the
// mutations themselves, so the UI cannot disagree with what the cluster layer
// will accept.
use crate::perf::FrameMeter;
use crate::{logview, table, theme};
use periscope_cluster::mutate as periscope_cluster_actions;

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
        /// Tail the logs of whatever is selected.
        ToggleLogs,
        /// Stick the log view to the newest line, or let it stay put.
        ToggleFollow,
        /// Show two clusters side by side, or go back to one.
        ToggleSplit,
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
        KeyBinding::new("cmd-l", ToggleLogs, None),
        KeyBinding::new("ctrl-l", ToggleLogs, None),
        KeyBinding::new("cmd-shift-f", ToggleFollow, None),
        KeyBinding::new("cmd-\\", ToggleSplit, None),
        KeyBinding::new("ctrl-\\", ToggleSplit, None),
    ]);
}

/// How often the view repaints with no events, so the age column keeps moving.
const TICK: Duration = Duration::from_secs(1);

/// The server-side filters a watch was started with.
///
/// Compared as a whole: a change to either means the running stream is watching
/// the wrong thing and has to be replaced.
type WatchFilters = (Option<Arc<str>>, Option<Arc<str>>);

/// How often idle clusters are swept.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

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

/// Something waiting for the user to agree to it.
///
/// A command is here alongside mutations rather than beside port forwards,
/// because it is a change: `kubectl exec` needs `create` on `pods/exec`, and
/// what it runs is arbitrary. One dialog, one rule, no exceptions to remember.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Proposal {
    /// A change to an object.
    Mutation(Arc<Mutation>),
    /// A command to run in a container.
    Command(Arc<ExecTarget>),
}

impl Proposal {
    /// The sentence the dialog shows.
    fn confirmation(&self, cluster: &ClusterId) -> String {
        match self {
            Self::Mutation(mutation) => mutation.confirmation(cluster),
            Self::Command(target) => target.confirmation(cluster),
        }
    }

    /// Whether the confirm button is the red one.
    fn is_destructive(&self) -> bool {
        match self {
            Self::Mutation(mutation) => mutation.is_destructive(),
            // A command can be anything, and Periscope cannot tell `ls` from
            // `rm -rf /`. Treating every one as destructive is the honest
            // reading, and it is the safe one.
            Self::Command(_) => true,
        }
    }

    /// The line under the sentence, when there is something more to say.
    fn warning(&self) -> Option<&'static str> {
        match self {
            Self::Mutation(mutation) => mutation
                .is_destructive()
                .then_some("This cannot be undone."),
            Self::Command(_) => {
                Some("Periscope cannot tell what a command does before it runs it.")
            }
        }
    }

    /// What the confirm button says.
    fn verb(&self) -> &'static str {
        match self {
            Self::Mutation(mutation) => match &**mutation {
                Mutation::Delete { .. } => "Delete",
                Mutation::Scale { .. } => "Scale",
                Mutation::Restart { .. } => "Restart",
                Mutation::Cordon { cordon: true, .. } => "Cordon",
                Mutation::Cordon { .. } => "Uncordon",
                Mutation::Drain { .. } => "Drain",
                Mutation::Apply { dry_run: true, .. } => "Dry run",
                Mutation::Apply { .. } => "Apply",
            },
            Self::Command(_) => "Run",
        }
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
    /// The watches currently running, so the same one is not started twice and
    /// a cluster nobody is looking at can be found and stopped.
    watching: std::collections::HashMap<(ClusterId, KindId), WatchFilters>,
    /// How long a cluster stays warm after its last pane closes.
    idle_timeout: Duration,
    /// When idle clusters were last swept.
    last_sweep: Instant,
    /// The most recent thing that went wrong locally, shown verbatim.
    last_error: Option<SharedString>,

    namespace_input: Entity<InputState>,
    selector_input: Entity<InputState>,
    search_input: Entity<InputState>,
    /// The YAML pane, a read-only syntax-highlighted editor.
    yaml_view: Entity<InputState>,
    /// Which object's YAML the editor currently holds.
    yaml_showing: Option<ResourceKey>,

    log_filter_input: Entity<InputState>,
    log_selector_input: Entity<InputState>,
    log_container_input: Entity<InputState>,
    log_scroll: gpui::UniformListScrollHandle,
    /// The command output's scroll position, kept apart from the log view's so
    /// the two panes do not fight over it.
    exec_scroll: gpui::UniformListScrollHandle,
    /// How many lines were visible last frame, so following only scrolls when
    /// there is something new to scroll to.
    log_visible: usize,
    /// The same, for command output.
    exec_visible: usize,
    /// What the log view last told the user about an export.
    log_notice: Option<SharedString>,

    palette: Palette,
    palette_open: bool,
    palette_input: Entity<InputState>,
    palette_matches: Vec<crate::palette::Match>,
    palette_index: usize,
    palette_focus: FocusHandle,

    /// A tail asked for on the command line, opened once the cluster connects.
    pending_tail: Option<Arc<LogTarget>>,
    /// A change waiting for the user to confirm it.
    ///
    /// Nothing is sent while this is `Some`: the confirmation *is* the gate.
    pending: Option<Proposal>,
    /// Replica count for a scale, as typed.
    replicas_input: Entity<InputState>,
    /// Container port to forward, as typed.
    port_input: Entity<InputState>,
    /// The command to run in a container, as typed.
    exec_input: Entity<InputState>,
    /// Whether the forwards panel is open.
    forwards_open: bool,
    /// How many lines a session keeps before dropping the oldest.
    log_capacity: usize,
    /// Counts exports, so two in one session do not overwrite each other.
    exports: u32,
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
        Self::with_tail(commands, started, perf, None, window, cx)
    }

    /// Builds the root view and opens a tail as soon as the cluster connects.
    ///
    /// This is what `--tail` uses: it is also the only way to measure the log
    /// view under load in an environment where nothing can click a button.
    pub fn with_tail(
        commands: CommandSender,
        started: Instant,
        perf: bool,
        tail: Option<LogTarget>,
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
        let replicas_input = cx.new(|cx| InputState::new(window, cx).placeholder("replicas"));
        let port_input = cx.new(|cx| InputState::new(window, cx).placeholder("port"));
        let exec_input = cx.new(|cx| InputState::new(window, cx).placeholder("command"));
        let log_filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter lines"));
        let log_selector_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("label selector (all pods)"));
        let log_container_input = cx.new(|cx| InputState::new(window, cx).placeholder("container"));
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
            // Filtering never restarts the stream, so it applies as it is typed.
            cx.subscribe(&log_filter_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let pattern = input.read(cx).value().to_string();
                    this.apply_log_filter(pattern, cx);
                }
            }),
            // Re-targeting does restart it, so those apply on Enter.
            cx.subscribe(&log_selector_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.retarget_logs(cx);
                }
            }),
            cx.subscribe(&log_container_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.retarget_logs(cx);
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
            watching: std::collections::HashMap::new(),
            idle_timeout: periscope_store::app::DEFAULT_IDLE_TIMEOUT,
            last_sweep: started,
            last_error: None,
            namespace_input,
            selector_input,
            search_input,
            log_filter_input,
            log_selector_input,
            log_container_input,
            log_scroll: gpui::UniformListScrollHandle::new(),
            exec_scroll: gpui::UniformListScrollHandle::new(),
            log_visible: 0,
            exec_visible: 0,
            log_notice: None,
            yaml_view,
            yaml_showing: None,
            palette: Palette::new(),
            palette_open: false,
            palette_input,
            palette_matches: Vec::new(),
            palette_index: 0,
            palette_focus: root_focus,
            pending_tail: tail.map(Arc::new),
            pending: None,
            replicas_input,
            port_input,
            exec_input,
            forwards_open: false,
            log_capacity: periscope_store::logs::DEFAULT_CAPACITY,
            exports: 0,
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
        // actually asked for by starting the app. Every pane's cluster is
        // connected, not just the focused one.
        for cluster in self.state.clusters_in_view() {
            self.connect_once(cluster);
        }

        // A tail asked for on the command line waits for the connection, not
        // for discovery: logs need no kind.
        if let (Some(target), Some(cluster)) =
            (self.pending_tail.clone(), self.state.active().cloned())
            && self
                .state
                .connection(&cluster)
                .is_some_and(|connection| connection.state == ConnectionState::Connected)
        {
            self.pending_tail = None;
            self.state
                .open_logs(cluster.clone(), Arc::clone(&target), self.log_capacity);
            self.send(ClusterCommand::StartLogs { cluster, target });
        }

        // Discovery decides which kind can be opened, so the first watch can
        // only start once the kinds have arrived. Each pane picks its own,
        // because two clusters need not serve the same kinds.
        for index in 0..self.state.panes().len() {
            if self.state.panes()[index].kind().is_none() {
                let cluster = self.state.panes()[index].cluster().cloned();
                if let Some(kind) = self.default_kind_of(cluster.as_ref()) {
                    let focus = self.state.focus();
                    self.state.focus_pane(index);
                    self.state.select_kind(kind);
                    self.state.focus_pane(focus);
                }
            }
        }
        self.ensure_watches();
        self.sweep_idle_clusters(Instant::now(), cx);

        cx.notify();
    }

    /// Sets which clusters may be changed, from settings.
    pub fn set_permissions(&mut self, permissions: periscope_store::Permissions) {
        self.state.set_permissions(permissions);
    }

    /// Applies the configured appearance.
    ///
    /// Separate from the constructor because the theme is applied to the whole
    /// app, not to this view: it needs a `Window`, which only exists once one
    /// is open.
    pub fn set_theme(&mut self, choice: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.theme = choice;
        theme::apply(choice, Some(window), cx);
        cx.notify();
    }

    /// Applies the configured limits: how long clusters stay warm, how many
    /// rows one may hold, and how many lines a buffer keeps.
    pub fn set_limits(&mut self, limits: periscope_config::Limits) {
        self.idle_timeout = limits.idle_timeout.get();
        self.log_capacity = limits.log_buffer;
        self.state.set_budget(limits.row_budget);
    }

    /// Running bridge counters.
    pub fn stats(&self) -> BridgeStats {
        self.stats
    }

    /// How many clusters are connected right now.
    fn connected_clusters(&self) -> usize {
        self.state
            .contexts()
            .iter()
            .filter(|context| {
                self.state
                    .connection(&ClusterId::new(&*context.name))
                    .is_some_and(|connection| {
                        matches!(
                            connection.state,
                            ConnectionState::Connected | ConnectionState::Degraded { .. }
                        )
                    })
            })
            .count()
    }

    /// The state being rendered.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Whether the palette is open.
    pub fn palette_open(&self) -> bool {
        self.palette_open
    }

    /// Pods if the focused pane's cluster serves them, otherwise the first
    /// watchable kind.
    fn default_kind(&self) -> Option<KindId> {
        self.default_kind_of(self.state.active())
    }

    /// The same, for any cluster.
    fn default_kind_of(&self, cluster: Option<&ClusterId>) -> Option<KindId> {
        let kinds = self.state.kinds_of(cluster);
        kinds
            .iter()
            .find(|info| info.id.is_core() && &*info.id.kind == "Pod")
            .or_else(|| kinds.iter().find(|info| info.watchable))
            .map(|info| info.id.clone())
    }

    /// Starts whatever watches the panes need, and stops none of them.
    ///
    /// A cluster that scrolls out of view keeps streaming: that is what makes
    /// switching back instant. Letting go is the idle sweep's job.
    fn ensure_watches(&mut self) {
        for (cluster, kind, filters) in self.state.watch_targets() {
            let wanted = (filters.namespace.clone(), filters.selector.clone());
            let key = (cluster.clone(), kind.clone());

            if self.watching.get(&key) == Some(&wanted) {
                continue;
            }

            self.send(ClusterCommand::Watch {
                cluster,
                kind,
                namespace: wanted.0.clone(),
                selector: wanted.1.clone(),
            });
            self.watching.insert(key, wanted);
        }
    }

    /// Stops watching clusters nobody has looked at for a while, and drops
    /// their rows.
    ///
    /// Without this, opening ten clusters over a working day would leave ten
    /// sets of watches running and ten tables in memory for the rest of the
    /// session. The connection itself is kept: re-opening a cluster should not
    /// mean re-running an exec plugin.
    fn sweep_idle_clusters(&mut self, now: Instant, cx: &mut Context<Self>) {
        if now.saturating_duration_since(self.last_sweep) < SWEEP_INTERVAL {
            return;
        }
        self.last_sweep = now;

        for cluster in self.state.idle_clusters(now, self.idle_timeout) {
            let kinds: Vec<KindId> = self
                .watching
                .keys()
                .filter(|(watched, _)| watched == &cluster)
                .map(|(_, kind)| kind.clone())
                .collect();

            for kind in kinds {
                tracing::info!(%cluster, %kind, "releasing an idle cluster");
                self.send(ClusterCommand::StopWatch {
                    cluster: cluster.clone(),
                    kind: kind.clone(),
                });
                self.watching.remove(&(cluster.clone(), kind));
            }

            self.state.release(&cluster);
            self.attempted.remove(&cluster);
            cx.notify();
        }
    }

    /// Reads the namespace and selector inputs and re-lists with them.
    fn apply_server_filters(&mut self, cx: &mut Context<Self>) {
        let namespace = self.namespace_input.read(cx).value().to_string();
        let selector = self.selector_input.read(cx).value().to_string();

        self.state
            .set_namespace(Some(Arc::from(namespace.as_str())));
        self.state.set_selector(Some(Arc::from(selector.as_str())));
        self.ensure_watches();
        cx.notify();
    }

    /// Points the focused pane at a cluster, connecting if this session has not
    /// yet.
    fn select_cluster(&mut self, cluster: ClusterId, cx: &mut Context<Self>) {
        self.state.select_cluster(cluster.clone());
        self.state.touch_cluster(&cluster, Instant::now());
        self.connect_once(cluster.clone());

        // The new cluster's kinds may not have arrived; the watch starts when
        // they do.
        if let Some(kind) = self.default_kind() {
            self.state.select_kind(kind);
        }
        self.ensure_watches();
        cx.notify();
    }

    /// Switches which kind the table shows.
    fn select_kind(&mut self, kind: KindId, cx: &mut Context<Self>) {
        self.state.select_kind(kind);
        self.ensure_watches();
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
        self.ensure_watches();

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

    // --- mutations ----------------------------------------------------------

    /// Puts a mutation in front of the user. Nothing is sent until they agree.
    fn propose(&mut self, mutation: Mutation, cx: &mut Context<Self>) {
        self.pending = Some(Proposal::Mutation(Arc::new(mutation)));
        cx.notify();
    }

    /// Drops the proposal without doing anything.
    fn cancel_mutation(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// Sends whatever was proposed, if the store allows it.
    fn confirm_mutation(&mut self, cx: &mut Context<Self>) {
        let (Some(pending), Some(cluster)) = (self.pending.take(), self.state.active().cloned())
        else {
            return;
        };

        match pending {
            Proposal::Mutation(mutation) => self.send_mutation(cluster, mutation),
            Proposal::Command(target) => self.send_command(cluster, target),
        }
        cx.notify();
    }

    /// Sends an authorised mutation, or records why it was refused.
    fn send_mutation(&mut self, cluster: ClusterId, mutation: Arc<Mutation>) {
        match self.state.authorize(&cluster, Arc::clone(&mutation)) {
            Ok(authorized) => {
                let (cluster, mutation) = authorized.into_parts();
                tracing::info!(
                    %cluster,
                    verb = mutation.verb(),
                    object = %mutation.key(),
                    "sending mutation"
                );
                self.send(ClusterCommand::Mutate { cluster, mutation });
            }
            Err(refusal) => {
                // A refusal the user cannot see is indistinguishable from a
                // bug, so it joins the activity list like any other outcome.
                tracing::warn!(%cluster, reason = refusal.reason(), "mutation refused");
                self.state
                    .record_refusal(cluster, mutation, &refusal, Instant::now());
            }
        }
    }

    /// The replica count typed into the scale field, if it is a number.
    fn typed_replicas(&self, cx: &App) -> Option<u32> {
        self.replicas_input.read(cx).value().trim().parse().ok()
    }

    /// What the table says a workload's replica count currently is.
    ///
    /// Read from the row rather than fetched: the confirmation says "from 3 to
    /// 5", and being one watch event stale there is better than a round trip
    /// before every dialog.
    fn current_replicas(&self, key: &ResourceKey) -> Option<u32> {
        let row = self.state.rows().iter().find(|row| &row.key == key)?;
        // Deployments and StatefulSets render READY as `ready/desired`.
        let (_, desired) = row.cell(0).split_once('/')?;
        desired.trim().parse().ok()
    }

    // --- forwards -----------------------------------------------------------

    /// Opens a local port onto the pod in the detail pane.
    ///
    /// A forward is not a mutation — it changes nothing in the cluster — so it
    /// needs no confirmation, and it works on read-only clusters.
    fn start_forward(&mut self, cx: &mut Context<Self>) {
        let (Some(cluster), Some(detail)) = (self.state.active().cloned(), self.state.detail())
        else {
            return;
        };
        let key = detail.key().clone();

        let Some(port) = self
            .port_input
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
        else {
            self.last_error = Some(SharedString::from(
                "Type the container port to forward, next to Forward.",
            ));
            cx.notify();
            return;
        };

        let id = self.state.next_forward_id();
        let target = Arc::new(ForwardTarget::new(&*key.namespace, &*key.name, port));
        tracing::info!(%cluster, %id, target = %target.label(), "starting a forward");

        self.forwards_open = true;
        self.send(ClusterCommand::StartForward {
            cluster,
            id,
            target,
        });
        cx.notify();
    }

    /// Tears a forward down.
    fn stop_forward(&mut self, cluster: ClusterId, id: ForwardId, cx: &mut Context<Self>) {
        self.state.forget_forward(&cluster, id);
        self.send(ClusterCommand::StopForward { cluster, id });
        cx.notify();
    }

    fn toggle_forwards(&mut self, cx: &mut Context<Self>) {
        self.forwards_open = !self.forwards_open;
        cx.notify();
    }

    /// Copies a forward's address, which is the only thing anyone wants from it.
    fn copy_address(&mut self, address: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(address.clone()));
        self.last_error = None;
        self.log_notice = Some(SharedString::from(format!("Copied {address}")));
        cx.notify();
    }

    // --- commands -----------------------------------------------------------

    /// Proposes running the typed command in the pod in the detail pane.
    ///
    /// Nothing is sent here: like every other change, it goes through the
    /// confirmation dialog first.
    fn run_command(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.state.detail() else {
            return;
        };
        let key = detail.key().clone();
        let typed = self.exec_input.read(cx).value().to_string();

        // No container is named, so the apiserver picks the pod's default —
        // the same thing `kubectl exec` does without `-c`.
        let Some(target) = ExecTarget::parse(&*key.namespace, &*key.name, None, &typed) else {
            self.last_error = Some(SharedString::from(
                "Type a command to run next to Run, such as `ls -la /etc`.",
            ));
            cx.notify();
            return;
        };

        self.pending = Some(Proposal::Command(Arc::new(target)));
        cx.notify();
    }

    /// Sends an authorised command, or says why it was refused.
    fn send_command(&mut self, cluster: ClusterId, target: Arc<ExecTarget>) {
        match self.state.authorize_exec(&cluster, target) {
            Ok(authorized) => {
                let (cluster, target) = authorized.into_parts();
                tracing::info!(%cluster, target = %target.label(), "running a command");

                // The pane opens now, empty, rather than when the first line
                // arrives: a command that prints nothing still ran.
                self.state
                    .open_exec(cluster.clone(), Arc::clone(&target), self.log_capacity);
                self.send(ClusterCommand::Exec { cluster, target });
            }
            Err(refusal) => {
                tracing::warn!(%cluster, reason = refusal.reason(), "command refused");
                self.last_error = Some(SharedString::from(refusal.reason()));
            }
        }
    }

    /// Stops the running command.
    fn stop_command(&mut self, cx: &mut Context<Self>) {
        let Some(cluster) = self.state.exec().map(|session| session.cluster.clone()) else {
            return;
        };
        self.state.cancel_exec();
        self.send(ClusterCommand::CancelExec { cluster });
        cx.notify();
    }

    /// Closes the output pane, stopping the command if it is still running.
    fn close_command(&mut self, cx: &mut Context<Self>) {
        if self
            .state
            .exec()
            .is_some_and(periscope_store::ExecSession::is_running)
        {
            self.stop_command(cx);
        }
        self.state.close_exec();
        cx.notify();
    }

    // --- logs ---------------------------------------------------------------

    /// Tails whatever is selected, or closes the tail if one is open.
    fn toggle_logs(&mut self, _: &ToggleLogs, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.logs().is_some() {
            self.close_logs(cx);
            return;
        }
        self.open_logs(window, cx);
    }

    /// Opens a tail for the object in the detail pane, or for the pods the
    /// table's filters describe.
    fn open_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cluster) = self.state.active().cloned() else {
            return;
        };

        let target = match self.log_target() {
            Ok(target) => target,
            Err(reason) => {
                self.log_notice = Some(SharedString::from(reason));
                cx.notify();
                return;
            }
        };

        self.log_notice = None;
        self.log_selector_input.update(cx, |input, cx| {
            let value = match &target.selector {
                periscope_bridge::LogSelector::Labels(selector) => selector.to_string(),
                periscope_bridge::LogSelector::Pod(_) => String::new(),
            };
            input.set_value(value, window, cx);
        });

        let target = Arc::new(target);
        self.state
            .open_logs(cluster.clone(), Arc::clone(&target), self.log_capacity);
        self.send(ClusterCommand::StartLogs { cluster, target });
        cx.notify();
    }

    /// Works out what to tail, or why nothing can be.
    ///
    /// A pod in the detail pane is the obvious case. Otherwise the table's own
    /// filters say it: a label selector picks the pods, and a namespace is
    /// required because logs are a pod subresource — there is no cluster-wide
    /// log endpoint to fall back on.
    fn log_target(&self) -> Result<LogTarget, String> {
        let container = self
            .state
            .logs()
            .and_then(|session| session.target.container.as_ref().map(Arc::clone));

        if let Some(detail) = self.state.detail()
            && detail.kind().is_core()
            && &*detail.kind().kind == "Pod"
        {
            return Ok(
                LogTarget::pod(&*detail.key().namespace, &*detail.key().name).container(container),
            );
        }

        let filters = self.state.filters();
        let Some(namespace) = filters.namespace.clone() else {
            return Err(
                "Open a pod, or set a namespace filter, before tailing logs: log streams are \
                 per-namespace."
                    .to_owned(),
            );
        };

        match filters.selector.clone() {
            Some(selector) => Ok(LogTarget::labels(&*namespace, &*selector).container(container)),
            None => Err(format!(
                "Set a label selector to tail every pod in {namespace}, or open one pod."
            )),
        }
    }

    /// Restarts the session with whatever the selector and container fields say.
    fn retarget_logs(&mut self, cx: &mut Context<Self>) {
        let (Some(cluster), Some(session)) = (self.state.active().cloned(), self.state.logs())
        else {
            return;
        };

        let namespace = Arc::clone(&session.target.namespace);
        let previous = session.target.previous;
        let selector = self.log_selector_input.read(cx).value().to_string();
        let container = self.log_container_input.read(cx).value().to_string();
        let container = (!container.is_empty()).then(|| Arc::from(container.as_str()));

        let target = if selector.is_empty() {
            match &session.target.selector {
                periscope_bridge::LogSelector::Pod(pod) => LogTarget::pod(&*namespace, &**pod),
                periscope_bridge::LogSelector::Labels(_) => {
                    self.log_notice = Some(SharedString::from(
                        "A selector is needed to tail several pods.",
                    ));
                    cx.notify();
                    return;
                }
            }
        } else {
            LogTarget::labels(&*namespace, selector.as_str())
        };

        let target = Arc::new(target.container(container).previous(previous));
        self.state
            .open_logs(cluster.clone(), Arc::clone(&target), self.log_capacity);
        self.send(ClusterCommand::StartLogs { cluster, target });
        cx.notify();
    }

    /// Switches between the running container's logs and the previous one's.
    fn toggle_previous(&mut self, cx: &mut Context<Self>) {
        let (Some(cluster), Some(session)) = (self.state.active().cloned(), self.state.logs())
        else {
            return;
        };

        let target = Arc::new(LogTarget {
            previous: !session.target.previous,
            ..(*session.target).clone()
        });
        self.state
            .open_logs(cluster.clone(), Arc::clone(&target), self.log_capacity);
        self.send(ClusterCommand::StartLogs { cluster, target });
        cx.notify();
    }

    fn close_logs(&mut self, cx: &mut Context<Self>) {
        if let Some(cluster) = self.state.active().cloned() {
            self.send(ClusterCommand::StopLogs { cluster });
        }
        self.state.close_logs();
        self.log_notice = None;
        cx.notify();
    }

    /// Applies the filter box to the open session.
    fn apply_log_filter(&mut self, pattern: String, cx: &mut Context<Self>) {
        let Some(session) = self.state.logs() else {
            return;
        };
        let spec = FilterSpec {
            pattern,
            regex: session.buffer.filter().regex,
            case_sensitive: session.buffer.filter().case_sensitive,
        };
        if self.state.set_log_filter(spec) {
            cx.notify();
        }
    }

    /// Flips one of the filter's modes without touching the pattern.
    fn toggle_filter_mode(&mut self, regex: bool, cx: &mut Context<Self>) {
        let Some(session) = self.state.logs() else {
            return;
        };
        let current = session.buffer.filter().clone();
        let spec = if regex {
            FilterSpec {
                regex: !current.regex,
                ..current
            }
        } else {
            FilterSpec {
                case_sensitive: !current.case_sensitive,
                ..current
            }
        };
        if self.state.set_log_filter(spec) {
            cx.notify();
        }
    }

    fn toggle_follow(&mut self, _: &ToggleFollow, _window: &mut Window, cx: &mut Context<Self>) {
        let following = self
            .state
            .logs()
            .map(|session| !session.following)
            .unwrap_or(true);
        self.state.set_following(following);
        cx.notify();
    }

    /// Copies the visible lines to the clipboard.
    fn copy_logs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.state.logs() else {
            return;
        };
        let text = session.buffer.to_text();
        let lines = session.buffer.visible_len();

        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.log_notice = Some(SharedString::from(format!("Copied {lines} lines")));
        cx.notify();
    }

    /// Writes the visible lines to a file and says where it went.
    fn export_logs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.state.logs() else {
            return;
        };

        let name = format!(
            "periscope-{}-{}.log",
            session
                .target
                .label()
                .replace(['/', ' ', '[', ']', '(', ')'], "-"),
            self.exports
        );
        self.exports += 1;

        let path = periscope_config::paths::export_dir()
            .map(|dir| dir.join(&name))
            .and_then(|path| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, session.buffer.to_text())?;
                Ok(path)
            });

        self.log_notice = Some(SharedString::from(match path {
            Ok(path) => format!("Exported to {}", path.display()),
            // Failing to write a file is not a reason to lose the logs; say
            // what happened and leave them on screen.
            Err(error) => format!("Could not export: {error}"),
        }));
        cx.notify();
    }

    /// Connects to a cluster once per session. Reconnecting is explicit.
    fn connect_once(&mut self, cluster: ClusterId) {
        if self.attempted.insert(cluster.clone()) {
            self.send(ClusterCommand::Connect { cluster });
        }
    }

    /// Shows two clusters side by side, or goes back to one.
    fn toggle_split(&mut self, _: &ToggleSplit, _window: &mut Window, cx: &mut Context<Self>) {
        let changed = if self.state.is_split() {
            self.state.unsplit()
        } else {
            self.state.split()
        };

        if changed {
            // The new pane's cluster may never have been opened.
            if let Some(cluster) = self.state.active().cloned() {
                self.connect_once(cluster);
            }
            if let Some(kind) = self.default_kind() {
                self.state.select_kind(kind);
            }
            self.ensure_watches();
            cx.notify();
        }
    }

    /// Points commands at a pane.
    pub fn focus_pane(&mut self, index: usize, cx: &mut Context<Self>) {
        self.state.focus_pane(index);
        cx.notify();
    }

    /// Retries the focused pane's cluster after a failure.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(cluster) = self.state.active().cloned() {
            self.attempted.insert(cluster.clone());
            self.watching.retain(|(watched, _), _| watched != &cluster);
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
            self.state.all_rows(),
            self.state.active(),
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
        if self.pending.is_some() {
            // Escape closes the most dangerous thing on screen first.
            self.cancel_mutation(cx);
        } else if self.palette_open {
            self.close_palette(cx);
        } else if self.state.exec().is_some() {
            self.close_command(cx);
        } else if self.state.logs().is_some() {
            self.close_logs(cx);
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
            Target::Object { cluster, kind, key } => {
                // A hit on another cluster moves the focused pane there first;
                // otherwise the object would open against the wrong client.
                if self.state.active() != Some(&cluster) {
                    self.select_cluster(cluster, cx);
                }
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
                    .children(
                        self.state
                            .active()
                            .filter(|cluster| !self.state.may_mutate(cluster))
                            .map(|_| {
                                // A cluster that refuses changes says so where
                                // the user is looking, not only when they try.
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(cx.theme().secondary)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("read-only")
                            }),
                    )
                    .child(
                        Button::new("palette")
                            .outline()
                            .small()
                            .label("Jump  ⌘K")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_palette(&TogglePalette, window, cx);
                            })),
                    )
                    .children((self.state.forward_count() > 0).then(|| {
                        Button::new("forwards")
                            .outline()
                            .small()
                            .label(if self.state.broken_forwards() > 0 {
                                format!("Forwards {} ⚠", self.state.forward_count())
                            } else {
                                format!("Forwards {}", self.state.forward_count())
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_forwards(cx)))
                    }))
                    .child(
                        Button::new("split")
                            .outline()
                            .small()
                            .label(if self.state.is_split() {
                                "Single  ⌘\\"
                            } else {
                                "Split  ⌘\\"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_split(&ToggleSplit, window, cx)
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
                let in_other_pane = self.state.panes().iter().enumerate().any(|(index, pane)| {
                    index != self.state.focus() && pane.cluster() == Some(&id)
                });
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
                            )
                            // A cluster open in the other pane is still open;
                            // showing only the focused one would read as if it
                            // had been closed.
                            .children(in_other_pane.then(|| {
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("◫")
                            })),
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
            .children(self.state.last_activity().map(|activity| {
                div()
                    .text_xs()
                    .text_color(if activity.outcome.is_problem() {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(format!(
                        "{} {} · {}",
                        activity.mutation.verb(),
                        activity.mutation.key(),
                        activity.outcome.message()
                    ))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(format!("{} kinds · {state}", self.state.kinds().len()))
                    // What every warm cluster is costing, in the units the
                    // budget is written in.
                    .child(format!(
                        "· {} connected · {} rows held",
                        self.connected_clusters(),
                        self.state.total_rows()
                    ))
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

    /// One pane: its cluster, its table, and whether it has focus.
    fn pane_view(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.state.panes()[index];
        let focused = self.state.focus() == index;
        let split = self.state.is_split();

        let rows = pane.rows_shared();
        let columns: Arc<[periscope_bridge::ColumnSpec]> = Arc::from(pane.columns().to_vec());
        let namespaced = pane
            .kind()
            .and_then(|kind| self.state.kind_info(kind))
            .is_none_or(|info| info.namespaced);

        let connection = pane
            .cluster()
            .and_then(|cluster| self.state.connection(cluster));

        let body = if rows.is_empty() {
            let message = match connection.map(|connection| &connection.state) {
                None => "Select a context to connect.".to_owned(),
                Some(ConnectionState::Connecting) => "Connecting…".to_owned(),
                Some(ConnectionState::Idle) => "Not connected.".to_owned(),
                // A failure is already in the banner; do not repeat the reason,
                // but never leave the table looking merely empty.
                Some(state) if state.is_problem() => {
                    format!("No rows to show — the cluster is {}.", state.label())
                }
                Some(_) => match pane.kind() {
                    Some(kind) => format!("No {kind} here."),
                    None => "No kind selected.".to_owned(),
                },
            };
            table::placeholder(message, cx).into_any_element()
        } else {
            table::body(
                cx.entity(),
                index,
                rows,
                columns.clone(),
                namespaced,
                self.state.detail().map(|detail| detail.key().clone()),
                SystemTime::now(),
            )
            .into_any_element()
        };

        // Only a split needs a per-pane header: with one pane the window's own
        // header already says which cluster this is.
        let header = split.then(|| {
            let cluster = pane
                .cluster()
                .map(ClusterId::to_string)
                .unwrap_or_else(|| "no cluster".to_owned());
            let kind = pane
                .kind()
                .map(KindId::label)
                .unwrap_or_else(|| "no kind".to_owned());
            let state = connection
                .map(|connection| &connection.state)
                .unwrap_or(&ConnectionState::Idle);

            h_flex()
                .w_full()
                .flex_none()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(if focused {
                    cx.theme().accent
                } else {
                    cx.theme().secondary
                })
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().size(px(8.)).rounded_full().bg(state_color(state, cx)))
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(cx.theme().foreground)
                        .child(format!("{cluster} · {kind}")),
                )
        });

        v_flex()
            .id(SharedString::from(format!("pane-{index}")))
            .flex_1()
            .h_full()
            .overflow_hidden()
            .when(split && focused, |pane| {
                pane.border_l_2().border_color(cx.theme().accent)
            })
            .on_click(cx.listener(move |this, _, _, cx| this.focus_pane(index, cx)))
            .children(header)
            .children((index == self.state.focus()).then(|| self.toolbar(cx)))
            .child(table::header(&columns, namespaced, cx))
            .child(body)
    }

    /// The table area: one pane, or two side by side.
    fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.state.is_split() {
            return v_flex()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .child(self.pane_view(0, cx))
                .into_any_element();
        }

        h_resizable("panes")
            .child(resizable_panel().child(self.pane_view(0, cx)))
            .child(resizable_panel().child(self.pane_view(1, cx)))
            .into_any_element()
    }

    /// The log view: toolbar, source legend, and the lines themselves.
    fn log_pane(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.state.logs()?;
        let buffer = &session.buffer;
        let spec = buffer.filter();
        let show_source = buffer.sources().len() > 1
            || matches!(
                session.target.selector,
                periscope_bridge::LogSelector::Labels(_)
            );

        let counts = if buffer.visible_len() == buffer.len() {
            format!("{} lines", buffer.len())
        } else {
            format!("{} of {} lines", buffer.visible_len(), buffer.len())
        };
        let dropped = (buffer.dropped() > 0).then(|| format!("· {} dropped", buffer.dropped()));
        let streaming = format!("· {} streaming", buffer.streaming());

        let toolbar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("follow")
                    .outline()
                    .small()
                    .label(if session.following {
                        "Following"
                    } else {
                        "Paused"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_follow(&ToggleFollow, window, cx)
                    })),
            )
            .child(
                div()
                    .w(px(200.))
                    .child(Input::new(&self.log_filter_input).small()),
            )
            .child(
                Button::new("regex")
                    .outline()
                    .small()
                    .label(if spec.regex { ".* on" } else { ".* off" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_filter_mode(true, cx))),
            )
            .child(
                Button::new("case")
                    .outline()
                    .small()
                    .label(if spec.case_sensitive { "Aa" } else { "aa" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_filter_mode(false, cx))),
            )
            .child(
                div()
                    .w(px(180.))
                    .child(Input::new(&self.log_selector_input).small()),
            )
            .child(
                div()
                    .w(px(130.))
                    .child(Input::new(&self.log_container_input).small()),
            )
            .child(
                Button::new("previous")
                    .outline()
                    .small()
                    .label(if session.target.previous {
                        "Previous"
                    } else {
                        "Current"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_previous(cx))),
            )
            .child(
                Button::new("copy-logs")
                    .outline()
                    .small()
                    .label("Copy")
                    .on_click(cx.listener(|this, _, _, cx| this.copy_logs(cx))),
            )
            .child(
                Button::new("export-logs")
                    .outline()
                    .small()
                    .label("Export")
                    .on_click(cx.listener(|this, _, _, cx| this.export_logs(cx))),
            )
            .child(
                Button::new("close-logs")
                    .outline()
                    .small()
                    .label("Close")
                    .on_click(cx.listener(|this, _, _, cx| this.close_logs(cx))),
            );

        // Anything the session wants to say — a failure, a bad pattern, where
        // an export went — goes on one line rather than into a dialog.
        let notice = buffer
            .error()
            .map(|reason| (reason.to_owned(), true))
            .or_else(|| {
                buffer
                    .filter_error()
                    .map(|reason| (format!("filter: {reason}"), true))
            })
            .or_else(|| {
                self.log_notice
                    .as_ref()
                    .map(|notice| (notice.to_string(), false))
            });

        let lines = Arc::<[Arc<periscope_bridge::LogLine>]>::from(buffer.visible_lines());
        let body = if lines.is_empty() {
            let message = if !buffer.is_empty() {
                "Nothing matches the current filter.".to_owned()
            } else if buffer.streaming() > 0 {
                "Attached — waiting for output.".to_owned()
            } else {
                "Attaching…".to_owned()
            };
            logview::placeholder(message, cx).into_any_element()
        } else {
            logview::body(lines, show_source, self.log_scroll.clone()).into_any_element()
        };

        Some(
            v_flex()
                .flex_1()
                .h_full()
                .overflow_hidden()
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
                            div()
                                .text_sm()
                                .truncate()
                                .text_color(cx.theme().foreground)
                                .child(format!("logs · {}", session.target.label())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{counts} {streaming} {}",
                                    dropped.unwrap_or_default()
                                )),
                        ),
                )
                .child(toolbar)
                .children(notice.map(|(text, bad)| {
                    div()
                        .w_full()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(if bad {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(text)
                }))
                .children(
                    (!buffer.sources().is_empty()).then(|| logview::sources(buffer.sources(), cx)),
                )
                .child(body),
        )
    }

    /// The output of a command run in a container.
    ///
    /// Deliberately shaped like the log pane and not like a terminal: the
    /// output is a list of lines with the stream they came from, because that
    /// is what it is. See `docs/DECISIONS.md` ADR-0033.
    fn exec_pane(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let session = self.state.exec()?;
        let buffer = &session.buffer;
        let running = session.is_running();

        let status = session.summary();
        let bad = session
            .status
            .as_ref()
            .is_some_and(periscope_bridge::ExecStatus::is_problem);

        let lines = Arc::<[Arc<periscope_bridge::LogLine>]>::from(buffer.visible_lines());
        let body = if lines.is_empty() {
            let message = if running {
                "Running — waiting for output.".to_owned()
            } else {
                // A command that printed nothing is a result, not a bug, and
                // saying so is better than an empty box.
                "The command printed nothing.".to_owned()
            };
            logview::placeholder(message, cx).into_any_element()
        } else {
            // The source column is the stream — stdout or stderr — which is the
            // one thing worth distinguishing here.
            logview::body(lines, true, self.exec_scroll.clone()).into_any_element()
        };

        Some(
            v_flex()
                .flex_1()
                .h_full()
                .overflow_hidden()
                .child(
                    h_flex()
                        .w_full()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_sm()
                                .truncate()
                                .text_color(cx.theme().foreground)
                                .child(format!("exec · {}", session.target.label())),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .flex_none()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if bad {
                                            cx.theme().danger
                                        } else {
                                            cx.theme().muted_foreground
                                        })
                                        .child(status),
                                )
                                .children(running.then(|| {
                                    Button::new("stop-command")
                                        .outline()
                                        .small()
                                        .label("Stop")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.stop_command(cx)),
                                        )
                                }))
                                .child(
                                    Button::new("close-command")
                                        .outline()
                                        .small()
                                        .label("Close")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.close_command(cx)),
                                        ),
                                ),
                        ),
                )
                .child(body),
        )
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

        let is_pod = detail.kind().is_core() && &*detail.kind().kind == "Pod";
        let is_node = detail.kind().is_core() && &*detail.kind().kind == "Node";
        let kind = detail.kind().clone();
        let key = detail.key().clone();
        let writable = self
            .state
            .active()
            .is_some_and(|cluster| self.state.may_mutate(cluster));

        // Actions are offered only where they mean something, and only where
        // the cluster allows changes at all.
        let actions: Vec<_> = if !writable {
            Vec::new()
        } else {
            let mut actions = Vec::new();

            if periscope_cluster_actions::is_scalable(&kind) {
                let (scale_kind, scale_key) = (kind.clone(), key.clone());
                actions.push(
                    Button::new("scale")
                        .outline()
                        .small()
                        .label("Scale")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some(replicas) = this.typed_replicas(cx) else {
                                this.last_error = Some(SharedString::from(
                                    "Type a replica count next to Scale first.",
                                ));
                                cx.notify();
                                return;
                            };
                            let current = this.current_replicas(&scale_key);
                            this.propose(
                                Mutation::Scale {
                                    kind: scale_kind.clone(),
                                    key: scale_key.clone(),
                                    replicas,
                                    current,
                                },
                                cx,
                            );
                        })),
                );
            }

            if periscope_cluster_actions::is_restartable(&kind) {
                let (restart_kind, restart_key) = (kind.clone(), key.clone());
                actions.push(
                    Button::new("restart")
                        .outline()
                        .small()
                        .label("Restart")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.propose(
                                Mutation::Restart {
                                    kind: restart_kind.clone(),
                                    key: restart_key.clone(),
                                },
                                cx,
                            );
                        })),
                );
            }

            if is_node {
                let node = key.name.clone();
                let cordoned = self
                    .state
                    .rows()
                    .iter()
                    .find(|row| row.key == key)
                    .is_some_and(|row| row.cell(0).contains("SchedulingDisabled"));
                let drain_node = key.name.clone();
                actions.push(
                    Button::new("drain")
                        .outline()
                        .small()
                        .label("Drain")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.propose(
                                Mutation::Drain {
                                    node: Arc::clone(&drain_node),
                                    grace_period: None,
                                },
                                cx,
                            );
                        })),
                );
                actions.push(
                    Button::new("cordon")
                        .outline()
                        .small()
                        .label(if cordoned { "Uncordon" } else { "Cordon" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.propose(
                                Mutation::Cordon {
                                    node: Arc::clone(&node),
                                    cordon: !cordoned,
                                },
                                cx,
                            );
                        })),
                );
            }

            let (dry_kind, dry_key) = (kind.clone(), key.clone());
            actions.push(
                Button::new("dry-run")
                    .outline()
                    .small()
                    .label("Dry run")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let yaml = this.yaml_view.read(cx).value().to_string();
                        this.propose(
                            Mutation::Apply {
                                kind: dry_kind.clone(),
                                key: dry_key.clone(),
                                yaml: Arc::from(yaml.as_str()),
                                dry_run: true,
                            },
                            cx,
                        );
                    })),
            );

            let (apply_kind, apply_key) = (kind.clone(), key.clone());
            actions.push(
                Button::new("apply")
                    .outline()
                    .small()
                    .label("Apply")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let yaml = this.yaml_view.read(cx).value().to_string();
                        this.propose(
                            Mutation::Apply {
                                kind: apply_kind.clone(),
                                key: apply_key.clone(),
                                yaml: Arc::from(yaml.as_str()),
                                dry_run: false,
                            },
                            cx,
                        );
                    })),
            );

            let (delete_kind, delete_key) = (kind.clone(), key.clone());
            actions.push(
                Button::new("delete")
                    .danger()
                    .small()
                    .label("Delete")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.propose(
                            Mutation::Delete {
                                kind: delete_kind.clone(),
                                key: delete_key.clone(),
                                grace_period: None,
                            },
                            cx,
                        );
                    })),
            );

            actions
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
                        .children(actions)
                        .children(
                            is_pod.then(|| {
                                div().w(px(70.)).child(Input::new(&self.port_input).small())
                            }),
                        )
                        .children(is_pod.then(|| {
                            Button::new("forward")
                                .outline()
                                .small()
                                .label("Forward")
                                .on_click(cx.listener(|this, _, _, cx| this.start_forward(cx)))
                        }))
                        .children((is_pod && writable).then(|| {
                            div()
                                .w(px(150.))
                                .child(Input::new(&self.exec_input).small())
                        }))
                        .children((is_pod && writable).then(|| {
                            Button::new("run-command")
                                .outline()
                                .small()
                                .label("Run")
                                .on_click(cx.listener(|this, _, _, cx| this.run_command(cx)))
                        }))
                        .children(is_pod.then(|| {
                            Button::new("tail-logs")
                                .outline()
                                .small()
                                .label("Logs")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_logs(window, cx)),
                                )
                        }))
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

    /// The list of open forwards.
    ///
    /// Always reachable from the header, because a forward is a thing running
    /// on the user's machine: leaving one open by accident should be visible,
    /// not something you have to remember.
    fn forwards_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.forwards_open || self.state.forward_count() == 0 {
            return None;
        }

        let rows: Vec<_> = self
            .state
            .forwards()
            .map(|(cluster, forward)| {
                let (cluster, id) = (cluster.clone(), forward.id);
                let address = forward.address();
                let problem = forward.state.detail().map(str::to_owned);

                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(if forward.state.is_problem() {
                                        cx.theme().danger
                                    } else {
                                        cx.theme().foreground
                                    })
                                    .child(forward.summary()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(match &problem {
                                        Some(reason) => reason.clone(),
                                        None => format!(
                                            "{cluster} · {} connections",
                                            forward.connections
                                        ),
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_none()
                            .children(address.map(|address| {
                                Button::new(SharedString::from(format!("copy-{id}")))
                                    .outline()
                                    .small()
                                    .label("Copy")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_address(address.clone(), cx)
                                    }))
                            }))
                            .child(
                                Button::new(SharedString::from(format!("stop-{id}")))
                                    .outline()
                                    .small()
                                    .label("Stop")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.stop_forward(cluster.clone(), id, cx)
                                    })),
                            ),
                    )
            })
            .collect();

        Some(
            v_flex()
                .w(px(420.))
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
                            div()
                                .text_sm()
                                .text_color(cx.theme().foreground)
                                .child("Port forwards"),
                        )
                        .child(
                            Button::new("close-forwards")
                                .outline()
                                .small()
                                .label("Close")
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_forwards(cx))),
                        ),
                )
                .child(v_flex().id("forwards").overflow_y_scroll().children(rows)),
        )
    }

    /// The confirmation dialog.
    ///
    /// This is the guardrail the whole phase turns on, so it is deliberately
    /// dull: one sentence naming the cluster, the namespace, the object and the
    /// operation, and two buttons. Nothing is pre-selected, and the destructive
    /// confirm is the only red thing on screen.
    fn confirmation(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let pending = self.pending.clone()?;
        let cluster = self.state.active()?.clone();
        let sentence = pending.confirmation(&cluster);
        let destructive = pending.is_destructive();

        let connection = self.state.connection(&cluster);
        let health = connection
            .filter(|connection| connection.state.is_problem())
            .map(|connection| format!("This cluster is currently {}.", connection.state.label()));

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(gpui::black().opacity(0.5))
                .child(
                    v_flex()
                        .absolute()
                        .top(px(160.))
                        .left_1_2()
                        .ml(px(-280.))
                        .w(px(560.))
                        .rounded_lg()
                        .border_1()
                        .border_color(if destructive {
                            cx.theme().danger
                        } else {
                            cx.theme().border
                        })
                        .bg(cx.theme().background)
                        .child(
                            v_flex()
                                .gap_2()
                                .p_4()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().foreground)
                                        .child(sentence),
                                )
                                .children(health.map(|health| {
                                    div().text_xs().text_color(cx.theme().warning).child(health)
                                }))
                                .children(pending.warning().map(|warning| {
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(warning)
                                })),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .justify_end()
                                .gap_2()
                                .px_4()
                                .pb_4()
                                .child(
                                    Button::new("cancel-mutation")
                                        .outline()
                                        .small()
                                        .label("Cancel")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.cancel_mutation(cx)),
                                        ),
                                )
                                .child(
                                    Button::new("confirm-mutation")
                                        .small()
                                        .when(destructive, |button| button.danger())
                                        .when(!destructive, |button| button.primary())
                                        .label(pending.verb())
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.confirm_mutation(cx)),
                                        ),
                                ),
                        ),
                ),
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

        // Following means the newest line stays on screen. Scrolling only when
        // the count changed keeps a paused view exactly where the user left it.
        if let Some(session) = self.state.logs() {
            let visible = session.buffer.visible_len();
            if session.following && visible > 0 && visible != self.log_visible {
                self.log_scroll
                    .scroll_to_item(visible - 1, gpui::ScrollStrategy::Top);
            }
            self.log_visible = visible;
        } else if self.log_visible != 0 {
            self.log_visible = 0;
        }

        let frame = self.frames.start(Instant::now());
        if frame.is_some() {
            // Keep frames coming, so there is a continuous series to measure.
            // Nothing else in the app redraws when nothing has changed.
            window.request_animation_frame();
        }

        // The newest line of command output stays on screen the same way a
        // followed tail does; there is no "pause" here because a command ends
        // by itself.
        if let Some(session) = self.state.exec() {
            let visible = session.buffer.visible_len();
            if visible > 0 && visible != self.exec_visible {
                self.exec_scroll
                    .scroll_to_item(visible - 1, gpui::ScrollStrategy::Top);
            }
            self.exec_visible = visible;
        } else if self.exec_visible != 0 {
            self.exec_visible = 0;
        }

        // A command's output takes the main pane while it is open: it was just
        // asked for, and closing it falls back to whatever was there before.
        let main = self
            .exec_pane(cx)
            .map(gpui::IntoElement::into_any_element)
            .or_else(|| self.log_pane(cx).map(gpui::IntoElement::into_any_element));
        let main = match main {
            Some(pane) => pane,
            None => self.content(cx).into_any_element(),
        };

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
            .on_action(cx.listener(Self::toggle_logs))
            .on_action(cx.listener(Self::toggle_follow))
            .on_action(cx.listener(Self::toggle_split))
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
                            // A tail takes the main pane: reading logs is not a
                            // thing anyone does while watching a table.
                            .child(main)
                            .children(self.detail_pane(cx))
                            .children(self.forwards_panel(cx)),
                    )
                    .child(self.footer(cx)),
            )
            .children(self.confirmation(cx))
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
                log_lines = self
                    .state
                    .logs()
                    .map(|session| session.buffer.visible_len())
                    .unwrap_or(0),
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
    fn switching_kinds_leaves_the_previous_watch_running(cx: &mut TestAppContext) {
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

        // Phase 4 keeps what you have looked at warm: switching back must not
        // re-list. Letting go is the idle sweep's job, not the switch's.
        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::Watch {
                cluster: ClusterId::new("prod"),
                kind: deployments(),
                namespace: None,
                selector: None,
            }]
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(pods(), cx);
        });
        assert!(
            drain(&rx).is_empty(),
            "going back to a warm kind should re-request nothing"
        );
    }

    #[gpui::test]
    fn a_cluster_nobody_is_looking_at_is_let_go_after_the_idle_timeout(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), kinds_event("prod")],
        );
        apply(&harness, cx, vec![reset("prod", pods(), &["api-0"])]);
        drain(&rx);

        // Move both the pane and the clock along.
        harness.update(cx, |workspace, _window, cx| {
            workspace.select_cluster(ClusterId::new("staging"), cx);
        });
        drain(&rx);

        let later = Instant::now() + Duration::from_secs(3_600);
        harness.update(cx, |workspace, _window, cx| {
            workspace.sweep_idle_clusters(later, cx);
        });

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::StopWatch {
                cluster: ClusterId::new("prod"),
                kind: pods(),
            }]
        );
        harness.read(cx, |workspace| {
            // Its rows go with it: warm is a memory cost, and this is where it
            // is given back.
            assert_eq!(workspace.state().cluster_rows(&ClusterId::new("prod")), 0);
        });
    }

    #[gpui::test]
    fn the_configured_theme_is_applied_not_just_parsed(cx: &mut TestAppContext) {
        // `theme` was read from settings.toml and then never used: the app
        // always started on the system appearance whatever the file said.
        let (harness, _rx) = workspace(cx);

        harness.update(cx, |workspace, window, cx| {
            workspace.set_theme(ThemeChoice::Dark, window, cx);
            assert!(crate::theme::is_dark(cx));
        });
        harness.update(cx, |workspace, window, cx| {
            workspace.set_theme(ThemeChoice::Light, window, cx);
            assert!(!crate::theme::is_dark(cx));
        });
    }

    #[gpui::test]
    fn configured_limits_replace_the_built_in_ones(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), kinds_event("prod")],
        );
        apply(&harness, cx, vec![reset("prod", pods(), &["api-0"])]);

        harness.update(cx, |workspace, _window, cx| {
            workspace.set_limits(periscope_config::Limits {
                idle_timeout: periscope_config::Span(Duration::from_secs(30)),
                row_budget: 1_000,
                log_buffer: 500,
            });
            workspace.select_cluster(ClusterId::new("staging"), cx);
        });
        drain(&rx);

        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().budget(), 1_000);
        });

        // A minute is past the configured 30s but well inside the five-minute
        // default, so this only passes if the setting is the one being used.
        let later = Instant::now() + Duration::from_secs(60);
        harness.update(cx, |workspace, _window, cx| {
            workspace.sweep_idle_clusters(later, cx);
        });

        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::StopWatch {
                cluster: ClusterId::new("prod"),
                kind: pods(),
            }]
        );
    }

    #[gpui::test]
    fn a_configured_log_buffer_bounds_a_tail(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        harness.update(cx, |workspace, _window, _cx| {
            workspace.set_limits(periscope_config::Limits {
                log_buffer: 3,
                ..periscope_config::Limits::default()
            });
        });
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");
        drain(&rx);

        let lines: Vec<_> = (0..10)
            .map(|index| periscope_bridge::LogLine {
                source: periscope_bridge::LogSource::new("api-0", "api"),
                timestamp: None,
                text: Arc::from(format!("line {index}").as_str()),
            })
            .collect();
        apply(
            &harness,
            cx,
            vec![ClusterEvent::LogBatch {
                cluster: "prod".into(),
                lines: Arc::from(lines),
            }],
        );

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().logs().expect("a tail").buffer;
            assert_eq!(buffer.len(), 3, "the configured cap is what bounds it");
            assert_eq!(buffer.dropped(), 7);
        });
    }

    #[gpui::test]
    fn splitting_opens_a_second_cluster_and_watches_it(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![
                contexts(&["prod", "staging"], "prod"),
                kinds_event("prod"),
                kinds_event("staging"),
            ],
        );
        drain(&rx);

        harness.keys(cx, "cmd-\\");

        let sent = drain(&rx);
        assert!(
            sent.contains(&ClusterCommand::Connect {
                cluster: ClusterId::new("staging")
            }),
            "the second pane's cluster should be connected: {sent:?}"
        );
        assert!(
            sent.contains(&ClusterCommand::Watch {
                cluster: ClusterId::new("staging"),
                kind: pods(),
                namespace: None,
                selector: None,
            }),
            "the second pane should be watching: {sent:?}"
        );

        harness.read(cx, |workspace| {
            assert!(workspace.state().is_split());
            assert_eq!(workspace.state().panes().len(), 2);
            assert_eq!(
                workspace.state().panes()[0].cluster(),
                Some(&ClusterId::new("prod"))
            );
            assert_eq!(
                workspace.state().panes()[1].cluster(),
                Some(&ClusterId::new("staging"))
            );
        });

        harness.keys(cx, "cmd-\\");
        assert!(!harness.read(cx, |workspace| workspace.state().is_split()));
    }

    #[gpui::test]
    fn both_panes_render_their_own_cluster(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![
                contexts(&["prod", "staging"], "prod"),
                kinds_event("prod"),
                kinds_event("staging"),
            ],
        );
        harness.keys(cx, "cmd-\\");
        drain(&rx);

        apply(
            &harness,
            cx,
            vec![
                reset("prod", pods(), &["prod-a", "prod-b"]),
                reset("staging", pods(), &["staging-a"]),
            ],
        );

        harness.read(cx, |workspace| {
            let panes = workspace.state().panes();
            assert_eq!(panes[0].rows().len(), 2);
            assert_eq!(panes[1].rows().len(), 1);
        });
    }

    #[gpui::test]
    fn one_broken_cluster_does_not_disturb_the_other_pane(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![
                contexts(&["prod", "staging"], "prod"),
                kinds_event("prod"),
                kinds_event("staging"),
            ],
        );
        harness.keys(cx, "cmd-\\");
        drain(&rx);

        apply(
            &harness,
            cx,
            vec![
                reset("prod", pods(), &["prod-a"]),
                ClusterEvent::Status {
                    cluster: "staging".into(),
                    state: ConnectionState::AuthFailed {
                        reason: "token expired".into(),
                    },
                },
            ],
        );

        harness.read(cx, |workspace| {
            // The healthy pane still has its rows, and the broken one still
            // says exactly what went wrong.
            assert_eq!(workspace.state().panes()[0].rows().len(), 1);
            let staging = workspace
                .state()
                .connection(&ClusterId::new("staging"))
                .expect("tracked");
            assert_eq!(staging.state.detail(), Some("token expired"));
            let prod = workspace
                .state()
                .connection(&ClusterId::new("prod"))
                .expect("tracked");
            assert!(!prod.state.is_problem());
        });
    }

    #[gpui::test]
    fn the_palette_finds_objects_on_a_cluster_that_is_not_on_screen(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), kinds_event("prod")],
        );
        // Rows for a cluster no pane is showing.
        apply(&harness, cx, vec![reset("staging", pods(), &["needle-0"])]);
        drain(&rx);

        harness.keys(cx, "cmd-k");
        harness.keys(cx, "n e e d l e");
        harness.keys(cx, "enter");

        harness.read(cx, |workspace| {
            // Jumping moved the pane to the cluster the object is on.
            assert_eq!(workspace.state().active(), Some(&ClusterId::new("staging")));
            let detail = workspace.state().detail().expect("the object opened");
            assert_eq!(&*detail.key().name, "needle-0");
        });
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

    fn open_pod_detail(harness: &Harness, cx: &mut TestAppContext, name: &str) {
        harness.update(cx, |workspace, window, cx| {
            workspace.open_object(ResourceKey::new("default", name), window, cx);
        });
    }

    #[gpui::test]
    fn tailing_a_pod_starts_a_session_for_that_pod(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.keys(cx, "cmd-l");

        let sent = drain(&rx);
        assert_eq!(sent.len(), 1, "{sent:?}");
        match &sent[0] {
            ClusterCommand::StartLogs { cluster, target } => {
                assert_eq!(cluster, &ClusterId::new("prod"));
                assert_eq!(target.label(), "default/api-0");
            }
            other => panic!("expected a log session, got {other:?}"),
        }
        assert!(harness.read(cx, |workspace| workspace.state().logs().is_some()));
    }

    #[gpui::test]
    fn tailing_without_a_pod_or_a_selector_explains_what_is_missing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, "cmd-l");

        // Nothing was sent, and the reason is on screen rather than nowhere.
        assert!(drain(&rx).is_empty());
        harness.read(cx, |workspace| {
            assert!(workspace.state().logs().is_none());
            let notice = workspace.log_notice.as_ref().expect("a reason is shown");
            assert!(notice.contains("namespace"), "{notice}");
        });
    }

    #[gpui::test]
    fn a_namespace_and_selector_tail_every_matching_pod(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        harness.update(cx, |workspace, _window, cx| {
            workspace.state.set_namespace(Some(Arc::from("payments")));
            workspace.state.set_selector(Some(Arc::from("app=api")));
            let _ = cx;
        });
        drain(&rx);

        harness.keys(cx, "cmd-l");

        match drain(&rx).first() {
            Some(ClusterCommand::StartLogs { target, .. }) => {
                assert_eq!(target.label(), "payments/app=api");
            }
            other => panic!("expected a selector-based session, got {other:?}"),
        }
    }

    #[gpui::test]
    fn log_lines_reach_the_buffer_and_the_filter_applies_without_restarting(
        cx: &mut TestAppContext,
    ) {
        use periscope_bridge::{LogLine, LogSource};

        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");
        drain(&rx);

        let lines = |texts: &[&str]| ClusterEvent::LogBatch {
            cluster: "prod".into(),
            lines: texts
                .iter()
                .map(|text| LogLine {
                    source: LogSource::new("api-0", "api"),
                    timestamp: None,
                    text: Arc::from(*text),
                })
                .collect(),
        };

        apply(&harness, cx, vec![lines(&["starting", "an error", "done"])]);
        assert_eq!(
            harness.read(cx, |workspace| workspace
                .state()
                .logs()
                .unwrap()
                .buffer
                .visible_len()),
            3
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.apply_log_filter("error".to_owned(), cx);
        });

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().logs().unwrap().buffer;
            assert_eq!(buffer.visible_len(), 1);
            assert_eq!(buffer.len(), 3);
        });
        // Filtering is local: nothing was re-requested from the cluster.
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn following_can_be_paused(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");
        drain(&rx);

        assert!(harness.read(cx, |workspace| workspace.state().logs().unwrap().following));

        harness.keys(cx, "cmd-shift-f");
        assert!(!harness.read(cx, |workspace| workspace.state().logs().unwrap().following));
    }

    #[gpui::test]
    fn escape_closes_the_tail_and_stops_the_session(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");
        drain(&rx);

        harness.keys(cx, "escape");

        assert!(harness.read(cx, |workspace| workspace.state().logs().is_none()));
        assert_eq!(
            drain(&rx),
            vec![ClusterCommand::StopLogs {
                cluster: ClusterId::new("prod")
            }]
        );
    }

    #[gpui::test]
    fn switching_to_previous_logs_restarts_the_session(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| workspace.toggle_previous(cx));

        match drain(&rx).first() {
            Some(ClusterCommand::StartLogs { target, .. }) => {
                assert!(target.previous);
                assert_eq!(target.label(), "default/api-0 (previous)");
            }
            other => panic!("expected a restarted session, got {other:?}"),
        }
    }

    #[gpui::test]
    fn a_mutation_is_not_sent_until_it_is_confirmed(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        drain(&rx);

        let delete = Mutation::Delete {
            kind: pods(),
            key: ResourceKey::new("default", "api-0"),
            grace_period: None,
        };
        harness.update(cx, |workspace, _window, cx| {
            workspace.propose(delete, cx);
        });

        // Proposing sends nothing at all.
        assert!(drain(&rx).is_empty());
        assert!(harness.read(cx, |workspace| workspace.pending.is_some()));

        harness.update(cx, |workspace, _window, cx| workspace.confirm_mutation(cx));

        match drain(&rx).as_slice() {
            [ClusterCommand::Mutate { cluster, mutation }] => {
                assert_eq!(cluster, &ClusterId::new("prod"));
                assert_eq!(mutation.verb(), "delete");
            }
            other => panic!("expected one mutation, got {other:?}"),
        }
    }

    #[gpui::test]
    fn cancelling_sends_nothing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.propose(
                Mutation::Delete {
                    kind: pods(),
                    key: ResourceKey::new("default", "api-0"),
                    grace_period: None,
                },
                cx,
            );
        });
        harness.keys(cx, "escape");

        assert!(harness.read(cx, |workspace| workspace.pending.is_none()));
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn escape_cancels_a_mutation_before_anything_else(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        harness.update(cx, |workspace, window, cx| {
            workspace.open_object(ResourceKey::new("default", "api-0"), window, cx);
            workspace.propose(
                Mutation::Delete {
                    kind: pods(),
                    key: ResourceKey::new("default", "api-0"),
                    grace_period: None,
                },
                cx,
            );
        });
        drain(&rx);

        harness.keys(cx, "escape");
        harness.read(cx, |workspace| {
            // The dialog went; the detail pane behind it stayed.
            assert!(workspace.pending.is_none());
            assert!(workspace.state().detail().is_some());
        });
    }

    #[gpui::test]
    fn a_command_is_not_run_until_it_is_confirmed(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("ls -la /etc", window, cx));
            workspace.run_command(cx);
        });

        // A command is a change, so it waits behind the same dialog as one.
        assert!(drain(&rx).is_empty());
        harness.read(cx, |workspace| {
            let sentence = workspace
                .pending
                .as_ref()
                .expect("a command is waiting")
                .confirmation(&ClusterId::new("prod"));
            assert!(sentence.contains("prod"), "{sentence}");
            assert!(sentence.contains("api-0"), "{sentence}");
            assert!(sentence.contains("ls -la /etc"), "{sentence}");
        });

        harness.update(cx, |workspace, _window, cx| workspace.confirm_mutation(cx));

        match drain(&rx).as_slice() {
            [ClusterCommand::Exec { cluster, target }] => {
                assert_eq!(cluster, &ClusterId::new("prod"));
                assert_eq!(target.command_line(), "ls -la /etc");
                assert_eq!(&*target.pod, "api-0");
            }
            other => panic!("expected one exec, got {other:?}"),
        }
        // The pane opens straight away: a command that prints nothing still ran.
        assert!(harness.read(cx, |workspace| workspace.state().exec().is_some()));
    }

    #[gpui::test]
    fn command_output_and_its_exit_status_reach_the_pane(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("cat /missing", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });
        drain(&rx);

        apply(
            &harness,
            cx,
            vec![
                ClusterEvent::ExecOutput {
                    cluster: "prod".into(),
                    lines: Arc::from([periscope_bridge::LogLine {
                        source: periscope_bridge::LogSource::new("api-0", "stderr"),
                        timestamp: None,
                        text: Arc::from("cat: /missing: No such file or directory"),
                    }]),
                },
                ClusterEvent::ExecFinished {
                    cluster: "prod".into(),
                    status: periscope_bridge::ExecStatus::Exited {
                        code: Some(1),
                        message: "command terminated with exit code 1".to_owned(),
                    },
                },
            ],
        );

        harness.read(cx, |workspace| {
            let session = workspace.state().exec().expect("a session");
            assert_eq!(session.buffer.len(), 1);
            assert!(!session.is_running());
            // A non-zero exit is a result the user must be able to see.
            assert!(
                session.summary().contains("exited 1"),
                "{}",
                session.summary()
            );
        });
    }

    #[gpui::test]
    fn stopping_a_command_cancels_it_and_says_so(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("sleep 600", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| workspace.stop_command(cx));

        match drain(&rx).as_slice() {
            [ClusterCommand::CancelExec { cluster }] => {
                assert_eq!(cluster, &ClusterId::new("prod"));
            }
            other => panic!("expected a cancel, got {other:?}"),
        }
        // The pane does not sit on "running…" waiting for the cluster to agree.
        harness.read(cx, |workspace| {
            let session = workspace.state().exec().expect("a session");
            assert!(!session.is_running());
            assert_eq!(session.summary(), "cancelled");
        });
    }

    #[gpui::test]
    fn a_read_only_cluster_runs_no_command_and_sends_nothing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            let mut permissions = periscope_store::Permissions::permissive();
            permissions.deny(ClusterId::new("prod"));
            workspace.state.set_permissions(permissions);

            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("rm -rf /data", window, cx));
            workspace.run_command(cx);
            // Confirmed anyway: the store is what refuses, not the dialog.
            workspace.confirm_mutation(cx);
        });

        assert!(drain(&rx).is_empty());
        harness.read(cx, |workspace| {
            assert!(workspace.state().exec().is_none());
            let error = workspace.last_error.as_ref().expect("the refusal is shown");
            assert!(error.contains("read-only"), "{error}");
        });
    }

    #[gpui::test]
    fn an_empty_command_says_what_to_type_rather_than_running_nothing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| workspace.run_command(cx));

        assert!(drain(&rx).is_empty());
        harness.read(cx, |workspace| {
            assert!(workspace.pending.is_none());
            assert!(workspace.last_error.is_some());
        });
    }

    #[gpui::test]
    fn escape_closes_the_command_pane_and_stops_it(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("sleep 600", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });
        drain(&rx);

        harness.keys(cx, "escape");

        // Closing the pane must not leave a command running with nowhere to
        // report to.
        match drain(&rx).as_slice() {
            [ClusterCommand::CancelExec { .. }] => {}
            other => panic!("expected a cancel, got {other:?}"),
        }
        harness.read(cx, |workspace| {
            assert!(workspace.state().exec().is_none());
            // The detail pane behind it stayed.
            assert!(workspace.state().detail().is_some());
        });
    }

    #[gpui::test]
    fn a_read_only_cluster_refuses_in_the_ui_and_sends_nothing(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::Status {
                cluster: "prod".into(),
                state: ConnectionState::Connected,
            }],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| {
            let mut permissions = periscope_store::Permissions::permissive();
            permissions.deny(ClusterId::new("prod"));
            workspace.state.set_permissions(permissions);

            workspace.propose(
                Mutation::Delete {
                    kind: pods(),
                    key: ResourceKey::new("default", "api-0"),
                    grace_period: None,
                },
                cx,
            );
            workspace.confirm_mutation(cx);
        });

        // Confirmed, and still nothing went out.
        assert!(drain(&rx).is_empty());
        harness.read(cx, |workspace| {
            let last = workspace.state().last_activity().expect("recorded");
            assert!(last.outcome.is_problem());
            assert!(last.outcome.message().contains("read-only"));
        });
    }

    #[gpui::test]
    fn a_confirmation_sentence_names_the_cluster_and_the_object(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.propose(
                Mutation::Scale {
                    kind: deployments(),
                    key: ResourceKey::new("payments", "api"),
                    replicas: 0,
                    current: Some(3),
                },
                cx,
            );

            let cluster = workspace.state().active().unwrap().clone();
            let sentence = workspace.pending.as_ref().unwrap().confirmation(&cluster);

            assert!(sentence.contains("prod"), "{sentence}");
            assert!(sentence.contains("payments"), "{sentence}");
            assert!(sentence.contains("api"), "{sentence}");
            assert!(sentence.contains("from 3"), "{sentence}");
            assert!(sentence.contains("to 0"), "{sentence}");
        });
    }

    #[gpui::test]
    fn an_outcome_from_the_cluster_reaches_the_activity_line(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        apply(
            &harness,
            cx,
            vec![ClusterEvent::MutationDone {
                cluster: "prod".into(),
                mutation: Arc::new(Mutation::Restart {
                    kind: deployments(),
                    key: ResourceKey::new("payments", "api"),
                }),
                outcome: periscope_bridge::MutationOutcome::Applied {
                    detail: "api restarting".into(),
                },
            }],
        );

        harness.read(cx, |workspace| {
            let last = workspace.state().last_activity().expect("recorded");
            assert_eq!(last.outcome.message(), "api restarting");
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
