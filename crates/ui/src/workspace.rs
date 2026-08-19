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
use gpui::{Animation, AnimationExt as _, pulsating_between};
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
        /// Move the table's keyboard cursor down.
        RowDown,
        /// Move the table's keyboard cursor up.
        RowUp,
        /// Put the cursor on the first row.
        RowTop,
        /// Put the cursor on the last row.
        RowBottom,
        /// Move the cursor half a screen down.
        HalfPageDown,
        /// Move the cursor half a screen up.
        HalfPageUp,
        /// Open whatever the cursor is on.
        OpenRow,
    ]
);

/// Registers Periscope's default key bindings.
///
/// Called once at startup, after `gpui_component::init`, which binds the keys
/// the text inputs need.
pub fn init(cx: &mut App) {
    let problems = init_with_keys(cx, &periscope_config::Keys::default());
    debug_assert!(problems.is_empty(), "the built-in keymap is valid");
}

/// Where anything typable applies: everywhere except inside a text field.
///
/// A single-letter binding is the whole point of a k9s-style keymap and also
/// its whole danger — `l` tails logs, and almost every namespace anyone types
/// contains an `l`. GPUI dispatches from the focused element upwards, so a
/// binding on the root fires even while an input has focus unless it says
/// otherwise. This is that "otherwise", and a test types into a filter field to
/// prove it.
const OUTSIDE_TEXT_FIELDS: &str = "!Input && !NumberInput && !SearchPanel";

/// The table's keys, where the table is: not inside a field, not in the palette.
const TABLE: &str = "Periscope && !Input && !NumberInput && !SearchPanel";

/// Where a command's keys apply.
///
/// `Palette` bindings only fire while the palette is open, so `enter` in a
/// filter field is not swallowed by the palette's own confirm. Everything else
/// depends on the keystroke rather than the command: `cmd-l` is unambiguous
/// anywhere, while a bare `l` is a letter somebody may be trying to type.
fn context_of(command: periscope_config::Command, keystroke: &str) -> Option<&'static str> {
    use periscope_config::Command;

    match command {
        Command::Next | Command::Previous | Command::Confirm => Some("Palette"),
        // The table's keys are the palette's keys — `down`, `up`, `enter` —
        // and only one of the two may answer. `Periscope` is the root's context
        // while the palette is closed, so this says "the table, not the list".
        Command::RowDown
        | Command::RowUp
        | Command::RowTop
        | Command::RowBottom
        | Command::OpenRow => Some(if is_typable(keystroke) {
            TABLE
        } else {
            "Periscope"
        }),
        // The third case, and the reason this is not simply "is it typable":
        // `ctrl-d` and `ctrl-u` carry a modifier, so nobody is typing them, but
        // they are what readline gave every text field on this machine for
        // deleting a character and clearing a line. `gpui-component`'s input
        // binds neither, so a root binding would swallow both inside every
        // filter box and the YAML pane. Scoped to the table, whatever they are
        // remapped to.
        Command::HalfPageDown | Command::HalfPageUp => Some(TABLE),
        _ if is_typable(keystroke) => Some(OUTSIDE_TEXT_FIELDS),
        _ => None,
    }
}

/// Whether a keystroke is something a person could be typing into a field.
///
/// One key, one character, and no modifier that would take it out of the text
/// stream. `shift` stays typable: `:` is shift-semicolon on most keyboards and
/// is very much a character.
fn is_typable(keystroke: &str) -> bool {
    let Ok(parsed) = gpui::Keystroke::parse(keystroke) else {
        return false;
    };
    let modifiers = parsed.modifiers;

    !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
        && parsed.key.chars().count() == 1
}

/// Registers key bindings from settings, reporting the ones that make no sense.
///
/// Every keystroke is parsed before it is bound: `KeyBinding::new` panics on a
/// malformed one, and a typo in a config file must not be able to take the app
/// down before it has a window to complain in. Whatever is returned here is
/// shown to the user; the rest of the keymap still works.
pub fn init_with_keys(cx: &mut App, keys: &periscope_config::Keys) -> Vec<String> {
    use periscope_config::Command;

    let mut problems = Vec::new();
    let mut bindings = Vec::new();

    for (command, keystrokes) in keys.bindings() {
        for keystroke in keystrokes {
            let context = context_of(command, &keystroke);

            if let Err(error) = validate(&keystroke) {
                problems.push(format!(
                    "`{keystroke}` is not a key ({} in settings.toml): {error}",
                    command.name()
                ));
                continue;
            }

            // One arm per command because the action types are distinct types,
            // not values of one enum.
            bindings.push(match command {
                Command::Palette => KeyBinding::new(&keystroke, TogglePalette, context),
                Command::Dismiss => KeyBinding::new(&keystroke, Dismiss, context),
                Command::Logs => KeyBinding::new(&keystroke, ToggleLogs, context),
                Command::Follow => KeyBinding::new(&keystroke, ToggleFollow, context),
                Command::Split => KeyBinding::new(&keystroke, ToggleSplit, context),
                Command::Next => KeyBinding::new(&keystroke, SelectNext, context),
                Command::Previous => KeyBinding::new(&keystroke, SelectPrevious, context),
                Command::Confirm => KeyBinding::new(&keystroke, Confirm, context),
                Command::RowDown => KeyBinding::new(&keystroke, RowDown, context),
                Command::RowUp => KeyBinding::new(&keystroke, RowUp, context),
                Command::RowTop => KeyBinding::new(&keystroke, RowTop, context),
                Command::RowBottom => KeyBinding::new(&keystroke, RowBottom, context),
                Command::HalfPageDown => KeyBinding::new(&keystroke, HalfPageDown, context),
                Command::HalfPageUp => KeyBinding::new(&keystroke, HalfPageUp, context),
                Command::OpenRow => KeyBinding::new(&keystroke, OpenRow, context),
            });
        }
    }

    cx.bind_keys(bindings);
    problems
}

/// Whether GPUI can make sense of a keystroke, without binding it.
fn validate(keystrokes: &str) -> Result<(), String> {
    if keystrokes.trim().is_empty() {
        return Err("it is empty".to_owned());
    }

    for keystroke in keystrokes.split_whitespace() {
        gpui::Keystroke::parse(keystroke).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// How often the view repaints with no events, so the age column keeps moving.
const TICK: Duration = Duration::from_secs(1);

/// The heading the recently opened kinds sit under.
const RECENT_HEADING: &str = "RECENT";

/// Whether the sidebar's kind filter matches a kind. An empty filter matches
/// everything, which is what makes the box narrow rather than select.
/// What to say in place of a table with no rows in it.
///
/// Never just "empty". Every reason a table has nothing in it is a different
/// thing to do next, and the one that used to be invisible matters most: an
/// RBAC refusal looks exactly like an empty namespace, and only one of the two
/// means the answer is "there are none".
fn empty_message(
    unavailable: Option<&str>,
    kind: Option<&KindId>,
    state: Option<&ConnectionState>,
) -> String {
    // A refusal outranks every other explanation: the cluster is connected, the
    // kind exists, and the reason there is nothing here is that this identity
    // is not allowed to know.
    if let Some(reason) = unavailable {
        return match kind {
            Some(kind) => format!("Not allowed to list {kind} here — {reason}"),
            None => reason.to_owned(),
        };
    }

    match state {
        None => "Select a context to connect.".to_owned(),
        Some(ConnectionState::Connecting) => "Connecting…".to_owned(),
        Some(ConnectionState::Idle) => "Not connected.".to_owned(),
        // A failure is already in the banner; do not repeat the reason, but
        // never leave the table looking merely empty.
        Some(state) if state.is_problem() => {
            format!("No rows to show — the cluster is {}.", state.label())
        }
        Some(_) => match kind {
            Some(kind) => format!("No {kind} here."),
            None => "No kind selected.".to_owned(),
        },
    }
}

fn matches_filter(info: &periscope_bridge::KindInfo, filter: &str) -> bool {
    filter.is_empty() || info.id.label().to_lowercase().contains(filter)
}

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
    Mutation {
        /// The cluster the sentence named.
        cluster: ClusterId,
        /// What it does.
        mutation: Arc<Mutation>,
    },
    /// A command to run in a container.
    Command {
        /// The cluster the sentence named.
        cluster: ClusterId,
        /// What will be run.
        target: Arc<ExecTarget>,
    },
}

impl Proposal {
    /// The cluster this was proposed against.
    ///
    /// Captured when it was proposed rather than read again when it is
    /// confirmed. The palette can change the active cluster while the dialog is
    /// open, and resolving it twice meant the sentence could name one cluster
    /// and the request go to another — which is the single thing ADR-0030
    /// exists to make impossible.
    fn cluster(&self) -> &ClusterId {
        match self {
            Self::Mutation { cluster, .. } | Self::Command { cluster, .. } => cluster,
        }
    }
}

impl Proposal {
    /// The sentence the dialog shows.
    fn confirmation(&self) -> String {
        match self {
            Self::Mutation { cluster, mutation } => mutation.confirmation(cluster),
            Self::Command { cluster, target } => target.confirmation(cluster),
        }
    }

    /// Whether the confirm button is the red one.
    fn is_destructive(&self) -> bool {
        match self {
            Self::Mutation { mutation, .. } => mutation.is_destructive(),
            // A command can be anything, and Periscope cannot tell `ls` from
            // `rm -rf /`. Treating every one as destructive is the honest
            // reading, and it is the safe one.
            Self::Command { .. } => true,
        }
    }

    /// The line under the sentence, when there is something more to say.
    fn warning(&self) -> Option<&'static str> {
        match self {
            Self::Mutation { mutation, .. } => mutation
                .is_destructive()
                .then_some("This cannot be undone."),
            Self::Command { .. } => {
                Some("Periscope cannot tell what a command does before it runs it.")
            }
        }
    }

    /// What the confirm button says.
    fn verb(&self) -> &'static str {
        match self {
            Self::Mutation { mutation, .. } => match &**mutation {
                Mutation::Delete { .. } => "Delete",
                Mutation::Scale { .. } => "Scale",
                Mutation::Restart { .. } => "Restart",
                Mutation::Cordon { cordon: true, .. } => "Cordon",
                Mutation::Cordon { .. } => "Uncordon",
                Mutation::Drain { .. } => "Drain",
                Mutation::Apply { dry_run: true, .. } => "Dry run",
                Mutation::Apply { .. } => "Apply",
            },
            Self::Command { .. } => "Run",
        }
    }
}

/// Which part of an object the detail pane is showing.
///
/// The pane used to stack all three: YAML in the middle, events squeezed into
/// 180 pixels at the bottom, owners in a strip at the top. Every other console
/// tabs them, and for the same reason — the YAML of a real object is longer
/// than a screen, and so is the event list of anything that is misbehaving.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DetailTab {
    /// The object itself.
    #[default]
    Yaml,
    /// What has happened to it.
    Events,
    /// What owns it.
    Related,
}

impl DetailTab {
    /// The label on the tab, with a count when there is something to count.
    fn label(self, events: usize, owners: usize) -> String {
        match self {
            Self::Yaml => "YAML".to_owned(),
            Self::Events if events > 0 => format!("Events ({events})"),
            Self::Events => "Events".to_owned(),
            Self::Related if owners > 0 => format!("Related ({owners})"),
            Self::Related => "Related".to_owned(),
        }
    }
}

/// Which of the two panes of lines a shared control is acting on.
///
/// A log tail and a command's output are the same [`periscope_store::LogBuffer`]
/// behind the same filter, copy and export; naming the pane is the only
/// difference between the two sets of buttons, so it is the only thing passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lines {
    /// The log view.
    Logs,
    /// The output of a command run in a container.
    Command,
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
    /// Narrows the sidebar's kind list. Seventy-seven kinds is a lot to scroll.
    kind_filter_input: Entity<InputState>,
    /// What the sidebar looked like last time: which sections were open, and
    /// which kinds were looked at on which cluster.
    remembered: periscope_config::UiState,
    /// Where that is written back to. `None` in tests, which is what stops a
    /// test window from writing over a real session.
    state_file: Option<periscope_config::StateFile>,
    /// The YAML pane, a read-only syntax-highlighted editor.
    yaml_view: Entity<InputState>,
    /// Which object's YAML the editor currently holds.
    yaml_showing: Option<ResourceKey>,

    log_filter_input: Entity<InputState>,
    log_selector_input: Entity<InputState>,
    log_container_input: Entity<InputState>,
    /// The time to jump to, as typed. See [`periscope_store::parse_time`].
    log_time_input: Entity<InputState>,
    /// Whether long lines wrap. Off by default: wrapping costs virtualisation,
    /// so the view can only show a window of the buffer while it is on.
    log_wrap: bool,
    /// Where a jump left the view, as an index into the visible lines. This is
    /// what moves the wrapped window; `None` means "the newest lines".
    log_anchor: Option<usize>,
    /// One per pane, so moving the cursor can bring its row into view.
    table_scroll: [gpui::UniformListScrollHandle; 2],
    log_scroll: gpui::UniformListScrollHandle,
    /// The wrapped body is an ordinary scrolling column, not a `uniform_list`,
    /// so it needs a scroll handle of the other kind.
    log_wrap_scroll: gpui::ScrollHandle,
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
    /// Narrows the command output, exactly as `log_filter_input` narrows a tail.
    exec_filter_input: Entity<InputState>,
    /// The same one-line answer, for the command output's own controls.
    exec_notice: Option<SharedString>,

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
    /// The containers of the pod in the detail pane, read from the object it
    /// fetched. Empty for anything that is not a pod.
    exec_containers: Vec<periscope_cluster::pods::PodContainer>,
    /// Which of them to run the command in. `None` is the pod's default, which
    /// is what `kubectl exec` picks without `-c`.
    exec_container: Option<Arc<str>>,
    /// Whether the container list is showing.
    container_menu_open: bool,
    /// Whether the forwards panel is open.
    forwards_open: bool,
    /// Whether the namespace list is showing.
    namespace_menu_open: bool,
    /// Which tab the detail pane is on.
    detail_tab: DetailTab,
    /// How many lines a session keeps before dropping the oldest.
    log_capacity: usize,
    /// Which columns each kind shows.
    columns: periscope_store::Layout,
    /// Rows that changed recently, refreshed once per frame rather than looked
    /// up per row: the table is virtualised but the map is one allocation.
    changed: Arc<std::collections::BTreeMap<ResourceKey, Instant>>,
    /// The namespaces the focused table's rows live in, refreshed once per
    /// frame for the same reason `changed` is: the toolbar wants a count and
    /// the menu wants the list, and neither should walk the rows to get one.
    namespaces: Arc<[Arc<str>]>,
    /// The visible log and command lines, taken once per frame.
    ///
    /// Both lists are virtualised, but the view indexes into the whole of
    /// each, and a hundred thousand `Arc` clones per frame is what asking the
    /// buffer for them each time used to cost.
    log_lines: Arc<[Arc<periscope_bridge::LogLine>]>,
    exec_lines: Arc<[Arc<periscope_bridge::LogLine>]>,
    /// Where refusals are written.
    ///
    /// The cluster layer records everything it is asked to do, but a refusal by
    /// the store's gate is never sent, so the cluster layer never sees it — and
    /// a read-only cluster turning down a delete is the single event the audit
    /// log exists to be able to prove afterwards. `None` only in tests.
    audit: Option<Arc<periscope_config::AuditLog>>,
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
        let kind_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter kinds"));
        let exec_input = cx.new(|cx| InputState::new(window, cx).placeholder("command"));
        let log_filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter lines"));
        let exec_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter output"));
        let log_selector_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("label selector (all pods)"));
        let log_container_input = cx.new(|cx| InputState::new(window, cx).placeholder("container"));
        let log_time_input = cx.new(|cx| InputState::new(window, cx).placeholder("14:32 or -5m"));
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
            // Local to the sidebar: nothing is re-fetched, so it applies as typed.
            cx.subscribe(
                &kind_filter_input,
                |_this, _input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ),
            // Filtering never restarts the stream, so it applies as it is typed.
            cx.subscribe(&log_filter_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let pattern = input.read(cx).value().to_string();
                    this.apply_filter(Lines::Logs, pattern, cx);
                }
            }),
            cx.subscribe(&exec_filter_input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let pattern = input.read(cx).value().to_string();
                    this.apply_filter(Lines::Command, pattern, cx);
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
            // A jump is a deliberate act, and half a typed time is a different
            // time, so it happens on Enter and never as it is typed.
            cx.subscribe(&log_time_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.jump_to_time(cx);
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
            kind_filter_input,
            remembered: periscope_config::UiState::default(),
            state_file: None,
            log_filter_input,
            log_selector_input,
            log_container_input,
            log_time_input,
            log_wrap: false,
            log_anchor: None,
            table_scroll: [
                gpui::UniformListScrollHandle::new(),
                gpui::UniformListScrollHandle::new(),
            ],
            log_scroll: gpui::UniformListScrollHandle::new(),
            log_wrap_scroll: gpui::ScrollHandle::new(),
            exec_scroll: gpui::UniformListScrollHandle::new(),
            log_visible: 0,
            exec_visible: 0,
            log_notice: None,
            exec_filter_input,
            exec_notice: None,
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
            exec_containers: Vec::new(),
            exec_container: None,
            container_menu_open: false,
            forwards_open: false,
            namespace_menu_open: false,
            detail_tab: DetailTab::default(),
            log_capacity: periscope_store::logs::DEFAULT_CAPACITY,
            columns: periscope_store::Layout::default(),
            changed: Arc::default(),
            namespaces: Arc::from([] as [Arc<str>; 0]),
            log_lines: Arc::from([] as [Arc<periscope_bridge::LogLine>; 0]),
            exec_lines: Arc::from([] as [Arc<periscope_bridge::LogLine>; 0]),
            audit: None,
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
                if let Some(kind) = self.opening_kind_of(cluster.as_ref()) {
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

    /// Sets where refusals are recorded.
    pub fn set_audit(&mut self, audit: periscope_config::AuditLog) {
        self.audit = Some(Arc::new(audit));
    }

    /// Writes a refusal to the audit log.
    ///
    /// The store refuses before anything is sent, so this is the only place
    /// that can record it. Failing to write is never fatal and never silent,
    /// exactly as the cluster layer treats it.
    fn audit_refusal(
        &self,
        cluster: &ClusterId,
        key: &ResourceKey,
        kind: &str,
        verb: &str,
        detail: String,
        reason: String,
    ) {
        let Some(audit) = self.audit.as_ref() else {
            return;
        };

        let entry = periscope_config::AuditEntry::new(
            cluster.as_str(),
            &*key.namespace,
            kind,
            &*key.name,
            verb,
        )
        .detail(detail)
        // Ours today — "prod is read-only" — but the rule is that nothing
        // reaches this file unredacted, so that it stays true when the reason
        // one day comes from the apiserver.
        .outcome(
            periscope_config::AuditOutcome::Refused,
            periscope_cluster::redact::text(&reason),
        );

        if let Err(error) = audit.append(&entry) {
            tracing::error!(%cluster, %error, "could not write the audit log");
        }
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

    /// Puts something the user needs to know on screen, verbatim.
    ///
    /// Used for problems found before the window existed — a keystroke in
    /// settings.toml that is not a key, for instance — which would otherwise
    /// only ever appear in a log file nobody has opened.
    pub fn report(&mut self, problem: impl Into<SharedString>) {
        self.last_error = Some(problem.into());
    }

    /// Restores the sidebar the last session left behind, and says where to
    /// write it back.
    ///
    /// Both together, because remembering something and never writing it down
    /// is the failure this exists to fix. A window built without this — every
    /// test window — remembers nothing and writes nothing.
    pub fn restore(
        &mut self,
        remembered: periscope_config::UiState,
        file: periscope_config::StateFile,
    ) {
        self.remembered = remembered;
        self.state_file = Some(file);
    }

    /// Writes the remembered sidebar back out.
    ///
    /// Failing to write it is worth a log line and nothing else: the session
    /// carries on with what is in memory, and the next change tries again.
    fn remember(&self) {
        let Some(file) = &self.state_file else {
            return;
        };
        if let Err(error) = file.write(&self.remembered) {
            tracing::warn!(%error, path = %file.path().display(), "could not write the remembered sidebar");
        }
    }

    /// Applies the configured column choices.
    pub fn set_columns(&mut self, columns: periscope_config::Columns) {
        self.columns = periscope_store::Layout::new(columns);
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

    /// The state being rendered, for the few reads that memoise as they go.
    #[cfg(test)]
    fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    /// Whether the palette is open.
    pub fn palette_open(&self) -> bool {
        self.palette_open
    }

    /// What a pane opens on when its cluster's kinds arrive.
    ///
    /// The kind that cluster was last left on, if it still serves it, and pods
    /// otherwise. Remembered per cluster because a kind is only meaningful on
    /// the cluster that serves it: half of what a cluster with cert-manager
    /// offers does not exist on the next one along.
    fn opening_kind_of(&self, cluster: Option<&ClusterId>) -> Option<KindId> {
        cluster
            .and_then(|cluster| {
                let last = self.remembered.last_kind(&cluster.to_string())?;
                periscope_store::recent(self.state.kinds_of(Some(cluster)), [last])
                    .first()
                    .map(|info| info.id.clone())
            })
            .or_else(|| self.default_kind_of(cluster))
    }

    /// Pods if the cluster serves them, otherwise the first watchable kind.
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
        // they do. Whichever kind it opens on is the one that cluster was left
        // on, in this session or the last, so switching back and forth between
        // two clusters keeps two places rather than resetting both to pods.
        if let Some(kind) = self.opening_kind_of(self.state.active()) {
            self.state.select_kind(kind);
        }
        self.ensure_watches();
        cx.notify();
    }

    /// Switches which kind the table shows.
    fn select_kind(&mut self, kind: KindId, cx: &mut Context<Self>) {
        self.visit(&kind);
        self.state.select_kind(kind);
        self.ensure_watches();
        cx.notify();
    }

    /// Records a kind having been opened on the focused pane's cluster.
    ///
    /// The context name and the kind, and nothing else: it is the same pair the
    /// audit log already writes, with no object, namespace or server in it.
    fn visit(&mut self, kind: &KindId) {
        let Some(cluster) = self.state.active() else {
            return;
        };
        // Selecting the kind that is already showing is not a visit — repainting
        // or clicking the row you are on must not reorder the recents list.
        if self.state.kind() == Some(kind) {
            return;
        }

        self.remembered.visited(cluster.to_string(), kind.label());
        self.remember();
    }

    /// Opens an object in the detail pane and asks for its YAML.
    pub fn open_object(&mut self, key: ResourceKey, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some(cluster), Some(kind)) =
            (self.state.active().cloned(), self.state.kind().cloned())
        else {
            return;
        };

        // A new object starts on YAML: the tab you were on for the last one
        // says nothing about this one, and Events is empty for most objects.
        self.detail_tab = DetailTab::default();
        self.forget_containers();
        self.state
            .open_detail(cluster.clone(), kind.clone(), key.clone());
        self.send(ClusterCommand::FetchObject {
            cluster,
            kind,
            key,
            // Secrets open masked. Revealing is a separate, deliberate act.
            reveal: false,
        });
        cx.notify();
    }

    /// Forgets the containers of the object that was on screen.
    ///
    /// Called when the pane is pointed somewhere else, not when its object
    /// arrives: between the two there is a window in which Run is on screen and
    /// the list belongs to the previous pod, and a command aimed at a container
    /// name from another pod is exactly the mistake the selector exists to
    /// prevent.
    fn forget_containers(&mut self) {
        self.exec_containers = Vec::new();
        self.exec_container = None;
        self.container_menu_open = false;
    }

    /// Re-fetches the object on screen with its secret values shown.
    fn reveal(&mut self, cx: &mut Context<Self>) {
        let (Some(cluster), Some(detail)) = (self.state.active().cloned(), self.state.detail())
        else {
            return;
        };
        let (kind, key) = (detail.kind().clone(), detail.key().clone());

        tracing::info!(%cluster, %kind, %key, "revealing secret values");
        self.state
            .open_detail(cluster.clone(), kind.clone(), key.clone());
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
        self.visit(&kind);
        self.state.select_kind(kind.clone());
        self.ensure_watches();

        if let Some(cluster) = self.state.active().cloned() {
            self.forget_containers();
            self.state
                .open_detail(cluster.clone(), kind.clone(), key.clone());
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
        let Some(cluster) = self.state.active().cloned() else {
            return;
        };
        self.pending = Some(Proposal::Mutation {
            cluster,
            mutation: Arc::new(mutation),
        });
        cx.notify();
    }

    /// Drops the proposal without doing anything.
    fn cancel_mutation(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    /// Sends whatever was proposed, if the store allows it.
    fn confirm_mutation(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };

        // The cluster comes from the proposal, not from whatever is selected
        // now. Both gates check it again, and the store's gate refuses a
        // cluster that is no longer connected.
        match pending {
            Proposal::Mutation { cluster, mutation } => self.send_mutation(cluster, mutation),
            Proposal::Command { cluster, target } => self.send_command(cluster, target),
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
                self.audit_refusal(
                    &cluster,
                    &mutation.key(),
                    &mutation.kind().label(),
                    mutation.verb(),
                    mutation.detail(),
                    refusal.reason(),
                );
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

        // `None` leaves the choice to the apiserver, which picks the pod's
        // default — the same thing `kubectl exec` does without `-c`. Either way
        // the confirmation sentence says which container it will be.
        let container = self.exec_container.clone();
        let Some(target) = ExecTarget::parse(&*key.namespace, &*key.name, container, &typed) else {
            self.last_error = Some(SharedString::from(
                "Type a command to run next to Run, such as `ls -la /etc`.",
            ));
            cx.notify();
            return;
        };

        let Some(cluster) = self.state.active().cloned() else {
            return;
        };
        self.pending = Some(Proposal::Command {
            cluster,
            target: Arc::new(target),
        });
        cx.notify();
    }

    /// Sends an authorised command, or says why it was refused.
    fn send_command(&mut self, cluster: ClusterId, target: Arc<ExecTarget>) {
        // Kept for the refusal path: `authorize_exec` takes the target, and a
        // refusal still has to say what was asked for.
        let asked = Arc::clone(&target);
        match self.state.authorize_exec(&cluster, target) {
            Ok(authorized) => {
                let (cluster, target) = authorized.into_parts();
                tracing::info!(%cluster, target = %target.label(), "running a command");

                // Running a second command keeps the filter the first one was
                // being read through, because the filter box on screen still
                // says so — and a control that disagrees with what is under it
                // is worse than either behaviour.
                let filter = self
                    .state
                    .exec()
                    .map(|session| session.buffer.filter().clone());

                // The pane opens now, empty, rather than when the first line
                // arrives: a command that prints nothing still ran.
                self.state
                    .open_exec(cluster.clone(), Arc::clone(&target), self.log_capacity);
                if let Some(filter) = filter {
                    self.state.set_exec_filter(filter);
                }
                self.exec_notice = None;
                self.send(ClusterCommand::Exec { cluster, target });
            }
            Err(refusal) => {
                tracing::warn!(%cluster, reason = refusal.reason(), "command refused");
                self.audit_refusal(
                    &cluster,
                    &ResourceKey::new(&*asked.namespace, &*asked.pod),
                    "pods",
                    "exec",
                    asked.command_line(),
                    refusal.reason(),
                );
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
    fn close_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .state
            .exec()
            .is_some_and(periscope_store::ExecSession::is_running)
        {
            self.stop_command(cx);
        }
        self.state.close_exec();
        // The filter box goes with the pane it belonged to: the next command
        // starts with the buffer unfiltered, and a box left holding a pattern
        // nothing is applying would say otherwise.
        self.exec_filter_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.exec_notice = None;
        cx.notify();
    }

    /// Shows or hides the list of the open pod's containers.
    fn toggle_container_menu(&mut self, cx: &mut Context<Self>) {
        self.container_menu_open = !self.container_menu_open;
        cx.notify();
    }

    /// Aims the next command at a container, or back at the pod's default.
    ///
    /// Nothing runs here: which container is part of what the confirmation
    /// sentence has to say, so picking one only changes what will be proposed.
    fn pick_container(&mut self, container: Option<Arc<str>>, cx: &mut Context<Self>) {
        self.exec_container = container;
        self.container_menu_open = false;
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
        self.log_anchor = None;
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
        self.log_anchor = None;
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
        self.log_anchor = None;
        cx.notify();
    }

    /// The lines one of the two panes is showing, if it is open.
    fn buffer(&self, lines: Lines) -> Option<&periscope_store::LogBuffer> {
        match lines {
            Lines::Logs => self.state.logs().map(|session| &session.buffer),
            Lines::Command => self.state.exec().map(|session| &session.buffer),
        }
    }

    /// What a pane is being read through, if it is open.
    fn filter_of(&self, lines: Lines) -> Option<FilterSpec> {
        self.buffer(lines).map(|buffer| buffer.filter()).cloned()
    }

    /// Narrows one of the two panes, reporting whether anything changed.
    fn set_filter(&mut self, lines: Lines, spec: FilterSpec) -> bool {
        match lines {
            Lines::Logs => self.state.set_log_filter(spec),
            Lines::Command => self.state.set_exec_filter(spec),
        }
    }

    /// What the pane's controls put on their one-line notice.
    fn say(&mut self, lines: Lines, said: String) {
        let said = Some(SharedString::from(said));
        match lines {
            Lines::Logs => self.log_notice = said,
            Lines::Command => self.exec_notice = said,
        }
    }

    /// Applies a filter box to the pane it belongs to.
    fn apply_filter(&mut self, lines: Lines, pattern: String, cx: &mut Context<Self>) {
        let Some(current) = self.filter_of(lines) else {
            return;
        };
        let spec = FilterSpec { pattern, ..current };

        if self.set_filter(lines, spec) {
            // A narrower filter renumbers every line, so the index a jump left
            // behind no longer names the line it landed on. Only the log view
            // has a jump; the command output is never anchored anywhere.
            if lines == Lines::Logs {
                self.log_anchor = None;
            }
            cx.notify();
        }
    }

    /// Flips one of the filter's modes without touching the pattern.
    fn toggle_filter_mode(&mut self, lines: Lines, regex: bool, cx: &mut Context<Self>) {
        let Some(current) = self.filter_of(lines) else {
            return;
        };
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

        if self.set_filter(lines, spec) {
            if lines == Lines::Logs {
                self.log_anchor = None;
            }
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
        // Following means "show me the newest line", which is the opposite of
        // sitting where a jump put the view.
        if following {
            self.log_anchor = None;
        }
        cx.notify();
    }

    /// Scrolls the view to the first line at or after the time in the field.
    ///
    /// Jumping stops the view following: sticking to the newest line would
    /// scroll away from what was just asked for on the next batch.
    fn jump_to_time(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.state.logs() else {
            return;
        };

        let typed = self.log_time_input.read(cx).value().to_string();
        let at = match periscope_store::parse_time(&typed, SystemTime::now()) {
            Ok(at) => at,
            Err(error) => {
                self.log_notice = Some(SharedString::from(error.to_string()));
                cx.notify();
                return;
            }
        };

        let found = session.buffer.seek(at);
        let held = session.buffer.visible_len();

        match found {
            Some(index) => {
                self.state.set_following(false);
                self.log_anchor = Some(index);
                self.log_scroll
                    .scroll_to_item(index, gpui::ScrollStrategy::Top);
                // The wrapped body rebuilds around the anchor, so the line is
                // the first one in it; the leftover offset is not.
                self.log_wrap_scroll.set_offset(gpui::point(px(0.), px(0.)));
                self.log_notice = Some(SharedString::from(format!(
                    "Jumped to {} — line {} of {held}",
                    logview::format_time(at),
                    index + 1,
                )));
            }
            // Distinguishing "nothing that late" from "nothing at all" is the
            // difference between a filter to widen and a wait to do.
            None => {
                self.log_notice = Some(SharedString::from(format!(
                    "No line at or after {} in the {held} lines held",
                    logview::format_time(at),
                )));
            }
        }
        cx.notify();
    }

    /// Wraps long lines, or clips them again.
    fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        self.log_wrap = !self.log_wrap;
        cx.notify();
    }

    /// Orders the lines by timestamp, or puts them back in arrival order.
    fn toggle_order(&mut self, cx: &mut Context<Self>) {
        let order = match self.state.logs().map(|session| session.buffer.order()) {
            Some(periscope_store::LogOrder::Timestamp) => periscope_store::LogOrder::Arrival,
            _ => periscope_store::LogOrder::Timestamp,
        };
        if self.state.set_log_order(order) {
            // Every index the view was holding meant a different line a moment
            // ago, the jump anchor included.
            self.log_anchor = None;
            cx.notify();
        }
    }

    /// How a pane names itself: the tail's target, or the command that ran.
    fn label_of(&self, lines: Lines) -> Option<String> {
        match lines {
            Lines::Logs => self.state.logs().map(|session| session.target.label()),
            Lines::Command => self.state.exec().map(|session| session.target.label()),
        }
    }

    /// Copies the visible lines to the clipboard.
    fn copy_lines(&mut self, lines: Lines, cx: &mut Context<Self>) {
        let Some(buffer) = self.buffer(lines) else {
            return;
        };
        let text = buffer.to_text();
        let copied = buffer.visible_len();

        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.say(lines, format!("Copied {copied} lines"));
        cx.notify();
    }

    /// Writes the visible lines to a file and says where it went.
    fn export_lines(&mut self, lines: Lines, cx: &mut Context<Self>) {
        let (Some(label), Some(text)) = (
            self.label_of(lines),
            self.buffer(lines).map(periscope_store::LogBuffer::to_text),
        ) else {
            return;
        };

        let name = export_name(&label, self.exports);
        self.exports += 1;

        let path = periscope_config::paths::export_dir()
            .map(|dir| dir.join(&name))
            .and_then(|path| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, text)?;
                // An export is a cluster's logs verbatim, sitting in a file
                // that outlives the session.
                periscope_config::paths::restrict(&path)?;
                Ok(path)
            });

        self.say(
            lines,
            match path {
                Ok(path) => format!("Exported to {}", path.display()),
                // Failing to write a file is not a reason to lose the lines;
                // say what happened and leave them on screen.
                Err(error) => format!("Could not export: {error}"),
            },
        );
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
            if let Some(kind) = self.opening_kind_of(self.state.active()) {
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
            self.close_palette(window, cx);
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

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.palette_matches.clear();
        // Focus goes back to the root. Leaving it on the palette's input —
        // which is no longer on screen — strands every key that is only bound
        // outside text fields, and quietly sends typing into an invisible box.
        window.focus(&self.palette_focus);
        cx.notify();
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            // Escape closes the most dangerous thing on screen first.
            self.cancel_mutation(cx);
        } else if self.namespace_menu_open {
            self.namespace_menu_open = false;
            cx.notify();
        } else if self.container_menu_open {
            self.container_menu_open = false;
            cx.notify();
        } else if self.palette_open {
            self.close_palette(window, cx);
        } else if self.state.exec().is_some() {
            self.close_command(window, cx);
        } else if self.state.logs().is_some() {
            self.close_logs(cx);
        } else if self.state.detail().is_some() {
            self.state.close_detail();
            self.yaml_showing = None;
            cx.notify();
        }
    }

    fn row_down(&mut self, _: &RowDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    fn row_up(&mut self, _: &RowUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn half_page_down(&mut self, _: &HalfPageDown, _window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_rows();
        if self.state.move_cursor_half_page(true, visible) {
            self.reveal_cursor(cx);
        }
    }

    fn half_page_up(&mut self, _: &HalfPageUp, _window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible_rows();
        if self.state.move_cursor_half_page(false, visible) {
            self.reveal_cursor(cx);
        }
    }

    /// How many rows fit in the focused pane, as of the last layout.
    ///
    /// Read from the list's own scroll handle rather than tracked through
    /// render: the table is virtualised, so the height it was laid out at is
    /// already recorded there, and a second copy could only ever disagree with
    /// it. Zero before the first frame, and zero rather than a panic if the
    /// handle is borrowed — a keystroke must not be able to take the app down.
    fn visible_rows(&self) -> usize {
        let Some(handle) = self.table_scroll.get(self.state.focus()) else {
            return 0;
        };
        let Ok(scroll) = handle.0.try_borrow() else {
            return 0;
        };

        let height = f32::from(scroll.base_handle.bounds().size.height);
        (height / table::ROW_HEIGHT) as usize
    }

    fn row_top(&mut self, _: &RowTop, _window: &mut Window, cx: &mut Context<Self>) {
        if self.state.set_cursor(0) {
            self.reveal_cursor(cx);
        }
    }

    fn row_bottom(&mut self, _: &RowBottom, _window: &mut Window, cx: &mut Context<Self>) {
        if self.state.cursor_to_end() {
            self.reveal_cursor(cx);
        }
    }

    /// Opens whatever the cursor is on.
    fn open_row(&mut self, _: &OpenRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.state.current_row().map(|row| row.key.clone()) else {
            return;
        };
        self.open_object(key, window, cx);
    }

    /// Switches which part of the object the detail pane shows.
    fn show_detail_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        self.detail_tab = tab;
        cx.notify();
    }

    /// Shows or hides the namespace list.
    fn toggle_namespace_menu(&mut self, cx: &mut Context<Self>) {
        self.namespace_menu_open = !self.namespace_menu_open;
        cx.notify();
    }

    /// Applies a namespace picked from the list.
    ///
    /// `None` is "all namespaces", which is a real choice rather than an empty
    /// field: it is how you get back to seeing everything.
    fn pick_namespace(
        &mut self,
        namespace: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = namespace.as_deref().unwrap_or_default().to_owned();
        self.namespace_input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
        });

        self.namespace_menu_open = false;
        self.apply_server_filters(cx);
    }

    /// Sorts a pane by one of its columns.
    pub fn sort_by(
        &mut self,
        pane: usize,
        key: periscope_store::app::SortKey,
        cx: &mut Context<Self>,
    ) {
        // Clicking a heading also focuses its pane, as clicking a row does.
        self.focus_pane(pane, cx);
        self.state.sort_by(key);
        cx.notify();
    }

    /// Puts the cursor on a row, without opening it.
    pub fn set_cursor(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.state.set_cursor(index) {
            cx.notify();
        }
    }

    /// Moves the cursor and scrolls it into view.
    fn move_cursor(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.state.move_cursor(by) {
            self.reveal_cursor(cx);
        }
    }

    /// Keeps the cursor on screen after it moves.
    ///
    /// `scroll_to_item` is non-strict — it does nothing when the row is already
    /// fully visible and otherwise scrolls the least it can — which is what
    /// makes holding `j` feel like a list rather than a slideshow.
    fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        let pane = self.state.focus();
        if let Some(scroll) = self.table_scroll.get(pane) {
            scroll.scroll_to_item(self.state.cursor(), gpui::ScrollStrategy::Top);
        }
        cx.notify();
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

        self.close_palette(window, cx);
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
            (Some(_), Some(kind)) => kind.label(),
            (Some(_), None) => "no kind selected".to_owned(),
            _ => "no cluster selected".to_owned(),
        };

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(crate::style::hairline(cx))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    // The cluster is the title. The window is already called
                    // Periscope, and a console's first question is always
                    // "which cluster am I about to change".
                    .children(self.state.active().map(|cluster| {
                        let state = self
                            .state
                            .active_connection()
                            .map(|connection| connection.state.clone())
                            .unwrap_or(ConnectionState::Idle);
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(status_dot(&state, cx))
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(crate::style::text(cx))
                                    .child(cluster.to_string()),
                            )
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(crate::style::text_faint(cx))
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
                            .ghost()
                            .small()
                            .label("Jump  ⌘K")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_palette(&TogglePalette, window, cx);
                            })),
                    )
                    .children((self.state.forward_count() > 0).then(|| {
                        Button::new("forwards")
                            .ghost()
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
                            .ghost()
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
                            .ghost()
                            .small()
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_contexts(cx))),
                    )
                    .child(
                        Button::new("theme")
                            .ghost()
                            .small()
                            // The word "Theme" is the one thing the button
                            // cannot be about anything else.
                            .label(self.theme.label())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cycle_theme(window, cx)),
                            ),
                    ),
            )
    }

    /// The recently opened kinds the focused pane's cluster still serves.
    fn recent_kinds(&self) -> Vec<periscope_bridge::KindInfo> {
        let Some(cluster) = self.state.active() else {
            return Vec::new();
        };
        periscope_store::recent(
            self.state.kinds_of(Some(cluster)),
            self.remembered.recent_kinds(&cluster.to_string()),
        )
    }

    /// One collapsible heading: a chevron, the name, and how many kinds are
    /// under it.
    fn section_heading(
        &self,
        title: String,
        open: bool,
        by_choice: bool,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let heading = title.clone();

        div()
            .id(SharedString::from(format!("section-{title}")))
            .w_full()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().accent.opacity(0.4)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_section(heading.clone(), by_choice, cx);
            }))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(if open { "▾" } else { "▸" })
                            .child(title),
                    )
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    /// One kind in the sidebar, under whichever heading is showing it.
    ///
    /// `group` keeps the element ids apart: a kind under RECENT is also under
    /// its own section, and two elements with one id are one element.
    fn kind_row(
        &self,
        info: &periscope_bridge::KindInfo,
        group: &str,
        label: String,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let count = self.state.row_count(&info.id);
        let click_kind = info.id.clone();
        let custom = info.custom;

        div()
            .id(SharedString::from(format!("{group}-{}", info.id.label())))
            .w_full()
            .pl_5()
            .pr_3()
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
                            .text_color(if custom {
                                cx.theme().info
                            } else {
                                cx.theme().foreground
                            })
                            .child(label),
                    )
                    .children((count > 0).then(|| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(count.to_string())
                    })),
            )
            .into_any_element()
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
                    .h(px(crate::style::ITEM_HEIGHT))
                    .flex_none()
                    .px_2()
                    .rounded_md()
                    .when(selected, |row| row.bg(crate::style::selected(cx)))
                    .hover(|row| row.bg(crate::style::hover(cx)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_cluster(click_id.clone(), cx);
                    }))
                    .child(
                        h_flex()
                            .h_full()
                            .gap_2()
                            .items_center()
                            .child(status_dot(state, cx))
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(if selected {
                                        crate::style::text(cx)
                                    } else {
                                        crate::style::text_dim(cx)
                                    })
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

        let filter = self
            .kind_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let current_kind = self.state.kind().cloned();
        let mut sections: Vec<gpui::AnyElement> = Vec::new();

        // What this cluster was last looked at, above the catalog. It needs no
        // curation and no pinning gesture: opening a kind is the only thing
        // anybody has to do to get on it.
        let recent = self.recent_kinds();
        let matching: Vec<_> = recent
            .iter()
            .filter(|info| matches_filter(info, &filter))
            .collect();
        if !matching.is_empty() {
            let by_choice = self.heading_is_open(RECENT_HEADING, true);
            let open = !filter.is_empty() || by_choice;

            sections.push(self.section_heading(
                RECENT_HEADING.to_owned(),
                open,
                by_choice,
                matching.len(),
                cx,
            ));

            if open {
                for info in matching {
                    let selected = current_kind.as_ref() == Some(&info.id);
                    // The full label here: there is no group heading above a
                    // recents list to say which `certificates` this is.
                    sections.push(self.kind_row(info, "recent", info.id.label(), selected, cx));
                }
            }
        }

        for (category, kinds) in periscope_store::sections(self.state.kinds()) {
            let matching: Vec<_> = kinds
                .iter()
                .filter(|info| matches_filter(info, &filter))
                .collect();
            if matching.is_empty() {
                continue;
            }

            let title = category.title();
            // Three things open a section: the user, a filter that found
            // something in it, and the selection being inside it — the last so
            // that what you are looking at is never hidden behind a chevron.
            let holds_selection = current_kind
                .as_ref()
                .is_some_and(|kind| matching.iter().any(|info| &info.id == kind));
            let by_choice = self.section_is_open(&category);
            let open = !filter.is_empty() || holds_selection || by_choice;

            sections.push(self.section_heading(title, open, by_choice, matching.len(), cx));

            if !open {
                continue;
            }

            for info in matching {
                let selected = current_kind.as_ref() == Some(&info.id);
                // Inside a section the group is the heading, so the plural
                // alone reads better than `deployments.apps`.
                sections.push(self.kind_row(
                    info,
                    "kind",
                    info.id.plural.to_string(),
                    selected,
                    cx,
                ));
            }
        }

        let nothing = sections.is_empty().then(|| {
            div()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(if self.state.kinds().is_empty() {
                    "connect to discover kinds"
                } else {
                    "no kind matches that filter"
                })
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
            .child(section("RESOURCES", cx))
            .child(
                div()
                    .w_full()
                    .px_1()
                    .pb_1()
                    .child(Input::new(&self.kind_filter_input).small()),
            )
            .children(sections)
            .children(nothing)
    }

    /// Namespace, selector and text filters.
    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (total, shown) = self.state.counts();
        let namespaces = self.namespaces.len();

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            // The filters read as one strip rather than four boxes: they are
            // the same kind of thing, and a bordered field for each made them
            // the loudest part of a window whose subject is the table below.
            .child(
                Button::new("namespaces")
                    .ghost()
                    .small()
                    .label(match self.state.filters().namespace.as_deref() {
                        Some(namespace) => format!("{namespace}  ▾"),
                        None => "All namespaces  ▾".to_owned(),
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_namespace_menu(cx))),
            )
            .child(Self::field(180., &self.namespace_input, cx))
            .child(Self::field(200., &self.selector_input, cx))
            .child(Self::field(200., &self.search_input, cx))
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

    /// One filter field: a quiet surface that only shows an edge when it is
    /// being typed into.
    fn field(width: f32, input: &Entity<InputState>, cx: &App) -> gpui::Div {
        div()
            .w(px(width))
            .rounded(px(crate::style::RADIUS))
            .bg(crate::style::raised(cx))
            .child(Input::new(input).small().appearance(false).bordered(false))
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
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(crate::style::hairline(cx))
            .text_xs()
            .text_color(crate::style::text_faint(cx))
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
                    // It used to read "77 kinds · connected · 1 connected · 24
                    // rows held": the connection state and the count of warm
                    // clusters both rendered as the word "connected", next to
                    // each other. The state is the dot in the header now, and
                    // the cluster count only appears when there is more than
                    // one to count.
                    .child(format!("{} kinds", self.state.kinds().len()))
                    .children(
                        (self.connected_clusters() > 1)
                            .then(|| format!("· {} clusters warm", self.connected_clusters())),
                    )
                    .child(format!("· {} rows held", self.state.total_rows()))
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

    /// What the sidebar's kind filter currently holds, for tests.
    #[cfg(test)]
    fn kind_filter(&self, cx: &App) -> String {
        self.kind_filter_input.read(cx).value().to_string()
    }

    /// Whether a sidebar section is showing its kinds.
    ///
    /// The user's choice if they made one — in this session or in any before it
    /// — and the category's own default otherwise: everything except workloads
    /// starts closed, because a list of seventy-seven kinds is the thing
    /// sections exist to remove.
    fn section_is_open(&self, category: &periscope_store::Category) -> bool {
        self.heading_is_open(&category.title(), category.open_by_default())
    }

    /// The same, for a heading that is not one of the catalog's categories.
    fn heading_is_open(&self, heading: &str, by_default: bool) -> bool {
        self.remembered.section(heading).unwrap_or(by_default)
    }

    /// Opens a closed section, or closes an open one, and remembers which.
    ///
    /// `open` is the section's *own* state, not what is on screen: a section
    /// forced open by a filter or by holding the selection still collapses back
    /// to closed when its heading is clicked, rather than needing two clicks.
    fn toggle_section(&mut self, title: String, open: bool, cx: &mut Context<Self>) {
        self.remembered.set_section(title, !open);
        self.remember();
        cx.notify();
    }

    /// The columns a pane renders, each paired with its position in the kind's
    /// full set.
    ///
    /// The pairing is what lets a subset be chosen: a row's cells are indexed
    /// by the original position, so dropping a column must not shift the rest.
    pub fn visible_columns(&self, index: usize) -> Vec<(usize, periscope_bridge::ColumnSpec)> {
        let pane = &self.state.panes()[index];
        let specs = pane.columns();

        match pane
            .kind()
            .and_then(|kind| self.columns.indices(kind, specs))
        {
            Some(chosen) => chosen
                .into_iter()
                .map(|index| (index, specs[index].clone()))
                .collect(),
            None => specs.iter().cloned().enumerate().collect(),
        }
    }

    /// One pane: its cluster, its table, and whether it has focus.
    fn pane_view(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.state.panes()[index];
        let focused = self.state.focus() == index;
        let split = self.state.is_split();

        let rows = pane.rows_shared();
        let columns: Arc<[(usize, periscope_bridge::ColumnSpec)]> =
            Arc::from(self.visible_columns(index));
        let namespaced = pane
            .kind()
            .and_then(|kind| self.state.kind_info(kind))
            .is_none_or(|info| info.namespaced);

        let connection = pane
            .cluster()
            .and_then(|cluster| self.state.connection(cluster));

        let body = if rows.is_empty() {
            let message = empty_message(
                pane.unavailable().map(|reason| &**reason),
                pane.kind(),
                connection.map(|connection| &connection.state),
            );
            table::placeholder(message, cx).into_any_element()
        } else {
            table::body(table::View {
                workspace: cx.entity(),
                pane: index,
                rows,
                columns: columns.clone(),
                namespaced,
                opened: self.state.detail().map(|detail| detail.key().clone()),
                cursor: pane.cursor(),
                changed: Arc::clone(&self.changed),
                scroll: self.table_scroll[index.min(1)].clone(),
                now: SystemTime::now(),
            })
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
            .child(table::header(
                cx.entity(),
                index,
                &columns,
                namespaced,
                pane.sort(),
                cx,
            ))
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
        let sorted = buffer.order() == periscope_store::LogOrder::Timestamp;
        let window = logview::wrap_window(buffer.visible_len(), self.log_anchor);

        let toolbar =
            h_flex()
                .w_full()
                .flex_none()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                // There are more controls here than fit a narrow window, and a
                // button that has fallen off the right edge might as well not exist.
                .flex_wrap()
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
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_filter_mode(Lines::Logs, true, cx)
                        })),
                )
                .child(
                    Button::new("case")
                        .outline()
                        .small()
                        .label(if spec.case_sensitive { "Aa" } else { "aa" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_filter_mode(Lines::Logs, false, cx)
                        })),
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
                    div()
                        .w(px(120.))
                        .child(Input::new(&self.log_time_input).small()),
                )
                .child(
                    Button::new("jump")
                        .outline()
                        .small()
                        .label("Jump")
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_time(cx))),
                )
                .child(
                    Button::new("wrap")
                        .outline()
                        .small()
                        .label(if self.log_wrap { "Wrap on" } else { "Wrap off" })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_wrap(cx))),
                )
                .child(
                    Button::new("order")
                        .outline()
                        .small()
                        .label(if sorted { "By time" } else { "By arrival" })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_order(cx))),
                )
                .child(
                    Button::new("copy-logs")
                        .outline()
                        .small()
                        .label("Copy")
                        .on_click(cx.listener(|this, _, _, cx| this.copy_lines(Lines::Logs, cx))),
                )
                .child(
                    Button::new("export-logs")
                        .outline()
                        .small()
                        .label("Export")
                        .on_click(cx.listener(|this, _, _, cx| this.export_lines(Lines::Logs, cx))),
                )
                .child(
                    Button::new("close-logs")
                        .outline()
                        .small()
                        .label("Close")
                        .on_click(cx.listener(|this, _, _, cx| this.close_logs(cx))),
                );

        // Anything the session wants to say — a failure, a bad pattern, where
        // an export went — goes on one line rather than into a dialog. A
        // failure replaces the rest of it, because a session that is broken has
        // nothing else worth reading; the standing notes about the two modes
        // that change what is on screen share the line with each other and with
        // whatever the last action said.
        let notice = buffer
            .error()
            .map(|reason| (reason.to_owned(), true))
            .or_else(|| {
                buffer
                    .filter_error()
                    .map(|reason| (format!("filter: {reason}"), true))
            })
            .or_else(|| {
                let mut notes: Vec<String> = Vec::new();
                if let Some(said) = &self.log_notice {
                    notes.push(said.to_string());
                }
                // Where the lines with no timestamp went is not something
                // anyone should have to guess at, or read the docs for.
                if sorted && buffer.untimestamped() > 0 {
                    notes.push(format!(
                        "No timestamp on {} of these lines; they are listed last, in arrival order.",
                        buffer.untimestamped()
                    ));
                }
                // Wrapping is the one mode that cannot show the whole buffer,
                // so it says which part of it is on screen while it is on.
                if self.log_wrap && window.len() < buffer.visible_len() {
                    notes.push(format!(
                        "Wrapped: showing lines {}–{} of {}. Filter or jump to reach the rest.",
                        window.start + 1,
                        window.end,
                        buffer.visible_len()
                    ));
                }
                (!notes.is_empty()).then(|| (notes.join(" · "), false))
            });

        let lines = Arc::clone(&self.log_lines);
        let body = if lines.is_empty() {
            let message = if !buffer.is_empty() {
                "Nothing matches the current filter.".to_owned()
            } else if buffer.streaming() > 0 {
                "Attached — waiting for output.".to_owned()
            } else {
                "Attaching…".to_owned()
            };
            logview::placeholder(message, cx).into_any_element()
        } else if self.log_wrap {
            logview::wrapped(&lines[window], show_source, &self.log_wrap_scroll, cx)
                .into_any_element()
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
        let spec = buffer.filter();

        let status = session.summary();
        let bad = session
            .status
            .as_ref()
            .is_some_and(periscope_bridge::ExecStatus::is_problem);

        let counts = if buffer.visible_len() == buffer.len() {
            format!("{} lines", buffer.len())
        } else {
            format!("{} of {} lines", buffer.visible_len(), buffer.len())
        };

        // The same controls as the log toolbar, over the same buffer, because
        // grepping a command's output is the same act as grepping a tail.
        let toolbar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .flex_wrap()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(px(200.))
                    .child(Input::new(&self.exec_filter_input).small()),
            )
            .child(
                Button::new("exec-regex")
                    .outline()
                    .small()
                    .label(if spec.regex { ".* on" } else { ".* off" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_filter_mode(Lines::Command, true, cx)
                    })),
            )
            .child(
                Button::new("exec-case")
                    .outline()
                    .small()
                    .label(if spec.case_sensitive { "Aa" } else { "aa" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_filter_mode(Lines::Command, false, cx)
                    })),
            )
            .child(
                Button::new("copy-command")
                    .outline()
                    .small()
                    .label("Copy")
                    .on_click(cx.listener(|this, _, _, cx| this.copy_lines(Lines::Command, cx))),
            )
            .child(
                Button::new("export-command")
                    .outline()
                    .small()
                    .label("Export")
                    .on_click(cx.listener(|this, _, _, cx| this.export_lines(Lines::Command, cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(counts),
            );

        // A pattern that will not compile is the one thing here that has to
        // shout: without it the pane looks like a command that printed nothing.
        let notice = buffer
            .filter_error()
            .map(|reason| (format!("filter: {reason}"), true))
            .or_else(|| {
                self.exec_notice
                    .as_ref()
                    .map(|said| (said.to_string(), false))
            });

        let lines = Arc::clone(&self.exec_lines);
        let body = if lines.is_empty() {
            let message = if !buffer.is_empty() {
                "Nothing matches the current filter.".to_owned()
            } else if running {
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
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_command(window, cx)
                                        })),
                                ),
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

            // Applying a masked Secret would send `<hidden, 7 bytes>` as the
            // value. The apiserver rejects it — it is not base64 — so this is a
            // confusing failure rather than a destroyed secret, but offering a
            // button whose only outcome is an error is its own defect.
            let editable = !matches!(
                detail,
                Detail::Ready { object, .. } if object.maskable && !object.revealed
            );

            let (dry_kind, dry_key) = (kind.clone(), key.clone());
            actions.extend(editable.then(|| {
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
                    }))
            }));

            let (apply_kind, apply_key) = (kind.clone(), key.clone());
            actions.extend(editable.then(|| {
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
                    }))
            }));

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
                    .map(|event| {
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_start()
                            .py_0p5()
                            .text_xs()
                            .child(
                                div()
                                    .w(px(90.))
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

                // One tab at a time, each with the whole pane: an object's YAML
                // is longer than a screen, and so is the event list of anything
                // that is misbehaving.
                let tabs = [DetailTab::Yaml, DetailTab::Events, DetailTab::Related].map(|tab| {
                    let active = self.detail_tab == tab;
                    Button::new(SharedString::from(format!("detail-tab-{tab:?}")))
                        .small()
                        .when(active, gpui_component::button::ButtonVariants::primary)
                        .when(!active, gpui_component::button::ButtonVariants::ghost)
                        .label(tab.label(object.events.len(), object.owners.len()))
                        .on_click(cx.listener(move |this, _, _, cx| this.show_detail_tab(tab, cx)))
                });

                let content = match self.detail_tab {
                    DetailTab::Yaml => div()
                        .flex_1()
                        .overflow_hidden()
                        .p_2()
                        .child(Input::new(&self.yaml_view).h_full())
                        .into_any_element(),
                    DetailTab::Events if events.is_empty() => div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing has happened to this object recently.")
                        .into_any_element(),
                    DetailTab::Events => v_flex()
                        .id("events")
                        .flex_1()
                        .overflow_y_scroll()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .children(events)
                        .into_any_element(),
                    DetailTab::Related if owners.is_empty() => div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing owns this object.")
                        .into_any_element(),
                    DetailTab::Related => v_flex()
                        .flex_1()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("OWNED BY"),
                        )
                        .children(owners)
                        .into_any_element(),
                };

                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_none()
                            .gap_1()
                            .px_3()
                            .py_1()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .children(tabs),
                    )
                    .child(content)
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
                        .gap_2()
                        .px_3()
                        .py_2()
                        // A pod with actions, a port, a container, a command and
                        // three buttons is more than 560 pixels of controls, and
                        // one that has fallen off the right edge might as well
                        // not exist — the log toolbar wraps for the same reason.
                        .flex_wrap()
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
                        // Only where there is something to choose between: a pod
                        // with one container has no choice to offer, and one
                        // whose object has not arrived yet does not know.
                        .children((is_pod && writable && self.exec_containers.len() > 1).then(
                            || {
                                Button::new("exec-container")
                                    .outline()
                                    .small()
                                    .label(match &self.exec_container {
                                        Some(container) => container.to_string(),
                                        None => "Default container".to_owned(),
                                    })
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.toggle_container_menu(cx)
                                        }),
                                    )
                            },
                        ))
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
        let cluster = pending.cluster().clone();
        let sentence = pending.confirmation();
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

    /// The list of namespaces the loaded rows are in.
    ///
    /// Only what is held, which is what can be known without another request —
    /// so the text field stays, for a namespace that has no rows yet.
    fn namespace_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.namespace_menu_open {
            return None;
        }

        let current = self.state.filters().namespace.clone();
        let mut entries: Vec<gpui::AnyElement> = Vec::new();

        let all_selected = current.is_none();
        entries.push(
            div()
                .id("namespace-all")
                .w_full()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(all_selected, |row| row.bg(cx.theme().accent))
                .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pick_namespace(None, window, cx);
                }))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child("All namespaces"),
                )
                .into_any_element(),
        );

        for namespace in self.namespaces.iter() {
            let selected = current.as_deref() == Some(&**namespace);
            let picked = Arc::clone(namespace);

            entries.push(
                div()
                    .id(SharedString::from(format!("namespace-{namespace}")))
                    .w_full()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_namespace(Some(Arc::clone(&picked)), window, cx);
                    }))
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(cx.theme().foreground)
                            .child(namespace.to_string()),
                    )
                    .into_any_element(),
            );
        }

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                // Clicking anywhere else puts it away, which is what everybody
                // expects of a menu and what `escape` also does.
                .id("namespace-backdrop")
                .on_click(cx.listener(|this, _, _, cx| this.toggle_namespace_menu(cx)))
                .child(
                    v_flex()
                        .absolute()
                        .top(px(96.))
                        .left(px(500.))
                        .w(px(260.))
                        .max_h(px(320.))
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .p_1()
                        .id("namespace-menu")
                        .overflow_y_scroll()
                        .children(entries),
                ),
        )
    }

    /// The containers a command can be aimed at.
    ///
    /// Read from the object the detail pane already fetched, so opening this
    /// costs no request: see the note in [`Render::render`] on where the names
    /// come from.
    fn container_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.container_menu_open {
            return None;
        }

        let mut entries: Vec<gpui::AnyElement> = Vec::new();
        let row = |id: String, selected: bool, label: String, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(id))
                .w_full()
                .px_3()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(selected, |row| row.bg(cx.theme().accent))
                .hover(|row| row.bg(cx.theme().accent.opacity(0.6)))
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().foreground)
                        .child(label),
                )
        };

        entries.push(
            row(
                "container-default".to_owned(),
                self.exec_container.is_none(),
                "Default container".to_owned(),
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| this.pick_container(None, cx)))
            .into_any_element(),
        );

        for container in &self.exec_containers {
            let picked = Arc::clone(&container.name);
            let selected = self.exec_container.as_deref() == Some(&*container.name);
            // An init container is worth flagging: unless it is a sidecar, it
            // has already exited by the time anyone is looking at the pod, and
            // the apiserver's refusal would otherwise be a surprise.
            let label = if container.init {
                format!("{} (init)", container.name)
            } else {
                container.name.to_string()
            };

            entries.push(
                row(format!("container-{}", container.name), selected, label, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_container(Some(Arc::clone(&picked)), cx);
                    }))
                    .into_any_element(),
            );
        }

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .id("container-backdrop")
                .on_click(cx.listener(|this, _, _, cx| this.toggle_container_menu(cx)))
                .child(
                    v_flex()
                        .absolute()
                        .top(px(96.))
                        // The detail pane is the right-hand 560 pixels of the
                        // window, and the button that opens this is in it.
                        .right(px(24.))
                        .w(px(240.))
                        .max_h(px(320.))
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .p_1()
                        .id("container-menu")
                        .overflow_y_scroll()
                        .children(entries),
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

        // First thing in the frame, because everything below it — pruning the
        // changed set, reparsing a newly-opened object's YAML, moving the log
        // scroll — is work this frame does and used to be measured as free.
        let frame = self.frames.start(Instant::now());
        if frame.is_some() {
            // Keep frames coming, so there is a continuous series to measure.
            // Nothing else in the app redraws when nothing has changed.
            window.request_animation_frame();
        }

        // Which rows changed lately, read once for the frame. Reading it also
        // prunes it, which is why it happens here rather than per row.
        let focus = self.state.focus();
        self.changed = self
            .state
            .changed_recently(focus, Instant::now(), crate::style::FLASH);
        self.namespaces = self.state.namespaces();
        self.log_lines = self
            .state
            .logs_mut()
            .map(|session| session.buffer.visible_lines())
            .unwrap_or_else(|| Arc::from([] as [Arc<periscope_bridge::LogLine>; 0]));
        self.exec_lines = self
            .state
            .exec_mut()
            .map(|session| session.buffer.visible_lines())
            .unwrap_or_else(|| Arc::from([] as [Arc<periscope_bridge::LogLine>; 0]));

        // The YAML editor is a separate entity that needs a `Window` to be
        // written to, which the event pump does not have; render time is where
        // the two meet.
        if let Some(Detail::Ready { object, .. }) = self.state.detail() {
            if self.yaml_showing.as_ref() != Some(&object.key) {
                let yaml = object.yaml.to_string();

                // Which containers a pod has is already known: it is in the
                // object the detail pane fetched, so the selector next to Run
                // needs no request of its own. Reading them means parsing the
                // YAML back, which is what holding the object as text costs;
                // it happens once per object opened, not once per frame.
                self.exec_containers = periscope_cluster::yaml::parse(&yaml)
                    .map(|value| periscope_cluster::pods::containers(&value))
                    .unwrap_or_default();
                // A container named in the pod before this one means nothing
                // here, and running a command in the wrong one is not a
                // mistake worth making possible.
                self.exec_container = None;

                self.yaml_showing = Some(object.key.clone());
                self.yaml_view.update(cx, |input, cx| {
                    input.set_value(yaml, window, cx);
                });
            }
        } else if self.yaml_showing.is_some() {
            self.yaml_showing = None;
            self.exec_containers = Vec::new();
            self.exec_container = None;
        }

        // Following means the newest line stays on screen. Scrolling only when
        // the count changed keeps a paused view exactly where the user left it.
        if let Some(session) = self.state.logs() {
            let visible = session.buffer.visible_len();
            if session.following && visible > 0 && visible != self.log_visible {
                self.log_scroll
                    .scroll_to_item(visible - 1, gpui::ScrollStrategy::Top);
                // The wrapped body is rebuilt around the newest lines while it
                // is following, so the bottom of it is the newest line.
                self.log_wrap_scroll.scroll_to_bottom();
            }
            self.log_visible = visible;
        } else if self.log_visible != 0 {
            self.log_visible = 0;
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
            .on_action(cx.listener(Self::row_down))
            .on_action(cx.listener(Self::row_up))
            .on_action(cx.listener(Self::row_top))
            .on_action(cx.listener(Self::row_bottom))
            .on_action(cx.listener(Self::half_page_down))
            .on_action(cx.listener(Self::half_page_up))
            .on_action(cx.listener(Self::open_row))
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
            .children(self.namespace_menu(cx))
            .children(self.container_menu(cx))
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

/// How many characters of a pane's label an export file name keeps.
///
/// A command line is part of the command pane's label and has no length limit;
/// a file name does, and on most filesystems it is 255 bytes.
const EXPORT_LABEL_LIMIT: usize = 80;

/// The file an export goes to, derived from what the pane is showing.
///
/// `exports` counts within the session so two exports of the same pane do not
/// overwrite each other. Everything a shell or a path would read as structure
/// becomes a dash, because the point of the name is to say what is in the file.
fn export_name(label: &str, exports: u32) -> String {
    let sanitized: String = label
        .replace(
            ['/', ' ', '[', ']', '(', ')', '$', '*', '"', '\'', '\\'],
            "-",
        )
        .chars()
        .take(EXPORT_LABEL_LIMIT)
        .collect();

    format!("periscope-{sanitized}-{exports}.log")
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

/// The dot that carries connection state at a glance.
///
/// It pulses while connecting. A spinner would claim more attention than the
/// event deserves, and a static dot cannot tell "reaching the apiserver" from
/// "reached it" — which is the one moment somebody actually watches for.
fn status_dot(state: &ConnectionState, cx: &App) -> gpui::AnyElement {
    let dot = div()
        .size(px(7.))
        .rounded_full()
        .flex_none()
        .bg(state_color(state, cx));

    if matches!(state, ConnectionState::Connecting) {
        return dot
            .with_animation(
                "connecting",
                Animation::new(std::time::Duration::from_millis(1_100))
                    .repeat()
                    .with_easing(pulsating_between(0.35, 1.0)),
                |dot, delta| dot.opacity(delta),
            )
            .into_any_element();
    }

    dot.into_any_element()
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

        /// Paints a frame, which is what fills in the element bounds a layout
        /// assertion reads back.
        fn draw(&self, cx: &mut TestAppContext) {
            let mut window = gpui::VisualTestContext::from_window(self.window.into(), cx);
            window.refresh().expect("the window is open");
            window.run_until_parked();
        }
    }

    fn workspace(cx: &mut TestAppContext) -> (Harness, CommandReceiver) {
        workspace_with_keys(cx, &periscope_config::Keys::default())
    }

    fn workspace_with_keys(
        cx: &mut TestAppContext,
        keys: &periscope_config::Keys,
    ) -> (Harness, CommandReceiver) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let problems = init_with_keys(cx, keys);
            assert!(problems.is_empty(), "{problems:?}");
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

    #[test]
    fn a_refused_kind_is_explained_rather_than_looking_empty() {
        // "No secrets here." is a reassuring sentence and, for somebody whose
        // role does not grant secrets, a false one.
        let kind = KindId::new("", "v1", "Secret", "secrets");
        let refused = empty_message(
            Some("secrets is forbidden: User \"dev\" cannot list resource"),
            Some(&kind),
            Some(&ConnectionState::Connected),
        );

        assert!(refused.contains("Not allowed"), "{refused}");
        assert!(refused.contains("forbidden"), "{refused}");
        assert!(!refused.contains("No secrets here"), "{refused}");

        // A refusal outranks the connection state, which is the whole point:
        // the cluster is fine.
        assert_eq!(
            empty_message(None, Some(&kind), Some(&ConnectionState::Connected)),
            "No secrets here."
        );
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

    /// A reset carrying several columns, so a choice between them is possible.
    fn wide_reset(cluster: &str, kind: KindId) -> ClusterEvent {
        ClusterEvent::ResourceReset {
            cluster: cluster.into(),
            kind,
            columns: Arc::from([
                ColumnSpec::fixed("READY", 60),
                ColumnSpec::fixed("STATUS", 100),
                ColumnSpec::fixed("RESTARTS", 80),
                ColumnSpec::flexible("NODE"),
            ]),
            rows: Arc::from([ResourceRow {
                key: ResourceKey::new("default", "api-0"),
                uid: None,
                cells: Arc::from([
                    Arc::from("1/1"),
                    Arc::from("Running"),
                    Arc::from("0"),
                    Arc::from("node-1"),
                ]),
                state: RowState::Healthy,
                created: None,
            }]),
        }
    }

    #[gpui::test]
    fn configured_columns_are_the_ones_rendered(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(&harness, cx, vec![wide_reset("prod", pods())]);

        harness.update(cx, |workspace, _window, _cx| {
            let mut chosen = periscope_config::Columns::default();
            chosen.choose("pods", ["STATUS", "NODE"]);
            workspace.set_columns(chosen);
        });

        harness.read(cx, |workspace| {
            let columns = workspace.visible_columns(0);
            let names: Vec<_> = columns
                .iter()
                .map(|(_, spec)| spec.name.to_string())
                .collect();
            assert_eq!(names, ["STATUS", "NODE"]);

            // The cell index travels with the column: rendering STATUS must
            // read the row's second cell, not its first.
            assert_eq!(
                columns.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
                [1, 3]
            );
        });
    }

    #[gpui::test]
    fn an_unconfigured_kind_still_shows_every_column(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(&harness, cx, vec![wide_reset("prod", pods())]);

        harness.update(cx, |workspace, _window, _cx| {
            let mut chosen = periscope_config::Columns::default();
            chosen.choose("deployments.apps", ["READY"]);
            workspace.set_columns(chosen);
        });

        harness.read(cx, |workspace| {
            assert_eq!(workspace.visible_columns(0).len(), 4);
        });
    }

    /// A discovery event with kinds spread across several sidebar sections.
    fn many_kinds(cluster: &str) -> ClusterEvent {
        let kind = |group: &str, plural: &str, custom: bool| KindInfo {
            id: KindId::new(group, "v1", plural, plural),
            namespaced: true,
            watchable: true,
            custom,
        };

        ClusterEvent::Kinds {
            cluster: cluster.into(),
            kinds: Arc::from([
                kind("", "pods", false),
                kind("apps", "deployments", false),
                kind("", "services", false),
                kind("", "secrets", false),
                kind("", "nodes", false),
                kind("cert-manager.io", "certificates", true),
            ]),
        }
    }

    #[gpui::test]
    fn clicking_a_column_sorts_the_pane_it_was_clicked_in(cx: &mut TestAppContext) {
        use periscope_store::app::SortKey;

        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![reset("prod", pods(), &["c-api", "a-api", "b-api"])],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.sort_by(0, SortKey::Name, cx);
        });

        harness.read(cx, |workspace| {
            let names: Vec<String> = workspace
                .state()
                .rows()
                .iter()
                .map(|row| row.key.name.to_string())
                .collect();
            // A column that was not being sorted by starts ascending.
            assert_eq!(names, ["a-api", "b-api", "c-api"]);
            assert!(!workspace.state().sort().descending);
        });

        harness.update(cx, |workspace, _window, cx| {
            workspace.sort_by(0, SortKey::Name, cx);
        });
        harness.read(cx, |workspace| {
            let names: Vec<String> = workspace
                .state()
                .rows()
                .iter()
                .map(|row| row.key.name.to_string())
                .collect();
            // The same column again reverses it.
            assert_eq!(names, ["c-api", "b-api", "a-api"]);
            assert!(workspace.state().sort().descending);
        });

        harness.update(cx, |workspace, _window, cx| {
            workspace.sort_by(0, SortKey::Name, cx);
        });
        harness.read(cx, |workspace| {
            // And a third click is the way back out to the default order.
            assert_eq!(
                workspace.state().sort(),
                periscope_store::app::Sort::default()
            );
        });
    }

    #[gpui::test]
    fn the_detail_pane_tabs_and_starts_on_yaml(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.read(cx, |workspace| {
            assert_eq!(workspace.detail_tab, DetailTab::Yaml);
        });

        harness.update(cx, |workspace, _window, cx| {
            workspace.show_detail_tab(DetailTab::Events, cx);
        });
        harness.read(cx, |workspace| {
            assert_eq!(workspace.detail_tab, DetailTab::Events);
        });

        // Opening another object goes back to YAML: which tab you were on for
        // the last one says nothing about this one.
        open_pod_detail(&harness, cx, "api-1");
        harness.read(cx, |workspace| {
            assert_eq!(workspace.detail_tab, DetailTab::Yaml);
        });
    }

    #[test]
    fn a_detail_tab_counts_what_it_holds() {
        assert_eq!(DetailTab::Yaml.label(3, 1), "YAML");
        assert_eq!(DetailTab::Events.label(3, 1), "Events (3)");
        assert_eq!(DetailTab::Related.label(3, 1), "Related (1)");
        // Nothing to count, nothing in brackets.
        assert_eq!(DetailTab::Events.label(0, 0), "Events");
        assert_eq!(DetailTab::Related.label(0, 0), "Related");
    }

    #[gpui::test]
    fn the_namespace_picker_lists_what_is_loaded_and_applies_a_choice(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![ClusterEvent::ResourceReset {
                cluster: "prod".into(),
                kind: pods(),
                columns: Arc::from([ColumnSpec::fixed("STATUS", 100)]),
                rows: Arc::from([
                    ResourceRow {
                        key: ResourceKey::new("payments", "api-0"),
                        uid: None,
                        cells: Arc::from([Arc::from("Running")]),
                        state: RowState::Healthy,
                        created: None,
                    },
                    ResourceRow {
                        key: ResourceKey::new("kube-system", "coredns-0"),
                        uid: None,
                        cells: Arc::from([Arc::from("Running")]),
                        state: RowState::Healthy,
                        created: None,
                    },
                ]),
            }],
        );
        drain(&rx);

        harness.update(cx, |workspace, _window, _cx| {
            let namespaces: Vec<String> = workspace
                .state_mut()
                .namespaces()
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(namespaces, ["kube-system", "payments"]);
            assert!(!workspace.namespace_menu_open);
        });

        harness.update(cx, |workspace, window, cx| {
            workspace.toggle_namespace_menu(cx);
            assert!(workspace.namespace_menu_open);
            workspace.pick_namespace(Some(Arc::from("payments")), window, cx);
        });

        harness.read(cx, |workspace| {
            // Picking one closes the menu, filters the table, and shows the
            // choice in the field somebody could also have typed into.
            assert!(!workspace.namespace_menu_open);
            assert_eq!(
                workspace.state().filters().namespace.as_deref(),
                Some("payments")
            );
            assert_eq!(workspace.state().rows().len(), 1);
        });

        // And it re-lists from the apiserver with the narrower scope.
        assert!(drain(&rx).iter().any(|command| matches!(
            command,
            ClusterCommand::Watch { namespace: Some(namespace), .. } if &**namespace == "payments"
        )));
    }

    #[gpui::test]
    fn all_namespaces_is_a_choice_not_an_empty_field(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        harness.update(cx, |workspace, window, cx| {
            workspace.pick_namespace(Some(Arc::from("payments")), window, cx);
        });
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.pick_namespace(None, window, cx);
        });

        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().filters().namespace, None);
        });
    }

    #[gpui::test]
    fn escape_closes_the_namespace_picker_before_anything_else(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.toggle_namespace_menu(cx);
        });
        harness.keys(cx, "escape");

        harness.read(cx, |workspace| {
            assert!(!workspace.namespace_menu_open);
            // The detail pane behind it stayed.
            assert!(workspace.state().detail().is_some());
        });
    }

    #[gpui::test]
    fn j_and_k_move_through_the_table_and_enter_opens_a_row(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![reset("prod", pods(), &["api-0", "api-1", "api-2"])],
        );
        drain(&rx);

        harness.keys(cx, "j");
        harness.keys(cx, "j");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 2);

        harness.keys(cx, "k");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 1);

        harness.keys(cx, "enter");
        harness.read(cx, |workspace| {
            let detail = workspace.state().detail().expect("a row opened");
            assert_eq!(&*detail.key().name, "api-1");
        });
        // Opening it asks the cluster for the object, as a click would.
        assert!(matches!(
            drain(&rx).as_slice(),
            [ClusterCommand::FetchObject { .. }]
        ));
    }

    #[gpui::test]
    fn the_arrows_do_the_same_as_j_and_k(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
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
        drain(&rx);

        harness.keys(cx, "down");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 1);
        harness.keys(cx, "up");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 0);
    }

    #[gpui::test]
    fn g_and_shift_g_jump_to_the_ends(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![reset("prod", pods(), &["api-0", "api-1", "api-2", "api-3"])],
        );
        drain(&rx);

        harness.keys(cx, "shift-g");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 3);
        harness.keys(cx, "g");
        assert_eq!(harness.read(cx, |workspace| workspace.state().cursor()), 0);
    }

    #[gpui::test]
    fn the_tables_keys_do_not_fire_while_the_palette_is_open(cx: &mut TestAppContext) {
        // `down` and `enter` belong to both; only one of them may answer.
        let (harness, rx) = workspace(cx);
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
        drain(&rx);

        harness.keys(cx, "cmd-k");
        harness.keys(cx, "down");

        harness.read(cx, |workspace| {
            assert!(workspace.palette_open());
            // The palette moved; the table stayed where it was.
            assert_eq!(workspace.state().cursor(), 0);
        });
    }

    #[gpui::test]
    fn moving_the_cursor_is_not_the_same_as_opening_a_row(cx: &mut TestAppContext) {
        // Fetching an object on every keypress would be a request per row in a
        // list somebody is scrolling through.
        let (harness, rx) = workspace(cx);
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
        drain(&rx);

        harness.keys(cx, "j");
        harness.keys(cx, "k");

        assert!(drain(&rx).is_empty());
        assert!(harness.read(cx, |workspace| workspace.state().detail().is_none()));
    }

    #[gpui::test]
    fn a_letter_bound_to_the_table_is_still_typed_into_a_filter(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
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
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.search_input.update(cx, |input, cx| {
                input.focus(window, cx);
            });
        });
        harness.keys(cx, "j");

        harness.read(cx, |workspace| assert_eq!(workspace.state().cursor(), 0));
        harness.update(cx, |workspace, _window, cx| {
            assert_eq!(workspace.search_input.read(cx).value(), "j");
        });
    }

    #[gpui::test]
    fn the_sidebar_groups_kinds_and_opens_only_workloads(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(&harness, cx, vec![many_kinds("prod")]);

        harness.read(cx, |workspace| {
            let sections = periscope_store::sections(workspace.state().kinds());
            let titles: Vec<String> = sections
                .iter()
                .map(|(category, _)| category.title())
                .collect();

            assert_eq!(
                titles,
                [
                    "WORKLOADS",
                    "NETWORKING",
                    "CONFIGURATION",
                    "CLUSTER",
                    "CERT-MANAGER.IO"
                ]
            );

            // Only workloads is open, or the sections would be a wall again.
            for (category, _) in &sections {
                assert_eq!(
                    workspace.section_is_open(category),
                    category.title() == "WORKLOADS",
                    "{}",
                    category.title()
                );
            }
        });
    }

    #[gpui::test]
    fn a_section_opens_and_closes_when_its_heading_is_clicked(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(&harness, cx, vec![many_kinds("prod")]);

        let storage = periscope_store::Category::Networking;
        harness.update(cx, |workspace, _window, cx| {
            assert!(!workspace.section_is_open(&storage));
            // One click opens a closed section — the bug this guards against is
            // a first click that stores the state it already had.
            workspace.toggle_section(storage.title(), false, cx);
            assert!(workspace.section_is_open(&storage));

            workspace.toggle_section(storage.title(), true, cx);
            assert!(!workspace.section_is_open(&storage));
        });

        // And the same for one that starts open.
        let workloads = periscope_store::Category::Workloads;
        harness.update(cx, |workspace, _window, cx| {
            assert!(workspace.section_is_open(&workloads));
            workspace.toggle_section(workloads.title(), true, cx);
            assert!(!workspace.section_is_open(&workloads));
        });
    }

    #[gpui::test]
    fn selecting_a_kind_in_a_closed_section_still_shows_it(cx: &mut TestAppContext) {
        // Opening `secrets` from the palette must not leave the selection
        // hidden behind a collapsed heading.
        let (harness, _rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(&harness, cx, vec![many_kinds("prod")]);

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(KindId::new("", "v1", "secrets", "secrets"), cx);
        });

        harness.read(cx, |workspace| {
            let configuration = periscope_store::Category::Configuration;
            // The section itself is still closed by choice...
            assert!(!workspace.section_is_open(&configuration));
            // ...and the sidebar renders it open anyway, which is what the
            // render path asserts by holding the selection.
            let holds = periscope_store::sections(workspace.state().kinds())
                .into_iter()
                .find(|(category, _)| category == &configuration)
                .is_some_and(|(_, kinds)| {
                    kinds
                        .iter()
                        .any(|info| Some(&info.id) == workspace.state().kind())
                });
            assert!(
                holds,
                "the selected kind is not in the section it renders in"
            );
        });
    }

    /// A directory that removes itself, for the tests that write state out.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "periscope-workspace-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn state(&self) -> periscope_config::StateFile {
            periscope_config::StateFile::at(self.0.join("state.toml"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[gpui::test]
    fn a_section_left_open_is_still_open_next_time(cx: &mut TestAppContext) {
        let dir = TempDir::new("sections");
        let file = dir.state();

        let (harness, _rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(&harness, cx, vec![many_kinds("prod")]);
        harness.update(cx, |workspace, _window, _cx| {
            workspace.restore(periscope_config::UiState::default(), file.clone());
        });

        let networking = periscope_store::Category::Networking;
        harness.update(cx, |workspace, _window, cx| {
            workspace.toggle_section(networking.title(), false, cx);
        });

        // What the next run would read off disk, restored into the view the way
        // the binary restores it at startup.
        let (remembered, problem) = file.read();
        assert!(problem.is_none(), "{problem:?}");
        harness.update(cx, |workspace, _window, _cx| {
            workspace.restore(remembered, file.clone());
        });

        harness.read(cx, |workspace| {
            assert!(workspace.section_is_open(&networking));
            // And a section nobody touched still follows its own default.
            assert!(workspace.section_is_open(&periscope_store::Category::Workloads));
            assert!(!workspace.section_is_open(&periscope_store::Category::Storage));
        });
    }

    #[gpui::test]
    fn a_window_with_nowhere_to_write_remembers_nothing_and_says_nothing(cx: &mut TestAppContext) {
        // Every test window is this one: without a state file the sidebar still
        // works, and no session on this machine is overwritten by a test.
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), many_kinds("prod")],
        );

        let networking = periscope_store::Category::Networking;
        harness.update(cx, |workspace, _window, cx| {
            workspace.toggle_section(networking.title(), false, cx);
            assert!(workspace.section_is_open(&networking));
        });
    }

    #[gpui::test]
    fn opening_a_kind_puts_it_at_the_top_of_the_recent_list(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), many_kinds("prod")],
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(KindId::new("", "v1", "secrets", "secrets"), cx);
            workspace.select_kind(KindId::new("apps", "v1", "deployments", "deployments"), cx);
        });

        harness.read(cx, |workspace| {
            let recent: Vec<String> = workspace
                .recent_kinds()
                .iter()
                .map(|info| info.id.label())
                .collect();
            // Most recent first, and the kind the pane opened on by itself is
            // not on the list: recents are what somebody asked for.
            assert_eq!(recent, ["deployments.apps", "secrets"]);
        });
    }

    #[gpui::test]
    fn recents_belong_to_the_cluster_they_were_opened_on(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), many_kinds("prod")],
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(
                KindId::new("cert-manager.io", "v1", "certificates", "certificates"),
                cx,
            );
        });

        // A cluster without cert-manager must not be offered a jump that lands
        // nowhere, even though it is the last thing that was opened.
        harness.update(cx, |workspace, _window, cx| {
            workspace.select_cluster(ClusterId::new("staging"), cx);
        });
        apply(&harness, cx, vec![kinds_event("staging")]);

        harness.read(cx, |workspace| assert!(workspace.recent_kinds().is_empty()));
    }

    #[gpui::test]
    fn a_pane_opens_on_the_kind_that_cluster_was_left_on(cx: &mut TestAppContext) {
        let dir = TempDir::new("last-kind");
        let (harness, rx) = workspace(cx);

        let mut remembered = periscope_config::UiState::default();
        remembered.visited("prod", "deployments.apps");
        harness.update(cx, |workspace, _window, _cx| {
            workspace.restore(remembered, dir.state());
        });

        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        // Pods is the default; this cluster was left on deployments.
        assert!(
            drain(&rx).contains(&ClusterCommand::Watch {
                cluster: ClusterId::new("prod"),
                kind: deployments(),
                namespace: None,
                selector: None,
            }),
            "the remembered kind is not what the pane opened on"
        );
    }

    #[gpui::test]
    fn a_remembered_kind_the_cluster_no_longer_serves_falls_back_to_pods(cx: &mut TestAppContext) {
        // Uninstalling an operator must not leave the app opening on a kind the
        // apiserver has never heard of.
        let dir = TempDir::new("gone-kind");
        let (harness, rx) = workspace(cx);

        let mut remembered = periscope_config::UiState::default();
        remembered.visited("prod", "certificates.cert-manager.io");
        harness.update(cx, |workspace, _window, _cx| {
            workspace.restore(remembered, dir.state());
        });

        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );

        assert!(drain(&rx).contains(&ClusterCommand::Watch {
            cluster: ClusterId::new("prod"),
            kind: pods(),
            namespace: None,
            selector: None,
        }));
    }

    #[gpui::test]
    fn coming_back_to_a_cluster_comes_back_to_what_it_was_showing(cx: &mut TestAppContext) {
        let dir = TempDir::new("two-clusters");
        let (harness, _rx) = workspace(cx);
        harness.update(cx, |workspace, _window, _cx| {
            workspace.restore(periscope_config::UiState::default(), dir.state());
        });
        apply(
            &harness,
            cx,
            vec![
                contexts(&["prod", "staging"], "prod"),
                kinds_event("prod"),
                kinds_event("staging"),
            ],
        );

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_kind(deployments(), cx);
            workspace.select_cluster(ClusterId::new("staging"), cx);
        });
        harness.read(cx, |workspace| {
            // Staging has never been pointed anywhere, so it opens on pods.
            assert_eq!(workspace.state().kind(), Some(&pods()));
        });

        harness.update(cx, |workspace, _window, cx| {
            workspace.select_cluster(ClusterId::new("prod"), cx);
        });
        harness.read(cx, |workspace| {
            assert_eq!(workspace.state().kind(), Some(&deployments()));
        });
    }

    #[gpui::test]
    fn half_a_page_moves_further_than_a_row_and_comes_back(cx: &mut TestAppContext) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        let names: Vec<String> = (0..200).map(|index| format!("api-{index:03}")).collect();
        let rows: Vec<&str> = names.iter().map(String::as_str).collect();
        apply(&harness, cx, vec![reset("prod", pods(), &rows)]);
        drain(&rx);
        // The half is half of what fits, which is only known once the list has
        // been laid out.
        harness.draw(cx);

        harness.keys(cx, "ctrl-d");
        let moved = harness.read(cx, |workspace| workspace.state().cursor());
        assert!(moved > 1, "ctrl-d moved {moved} rows");

        harness.keys(cx, "ctrl-u");
        harness.read(cx, |workspace| assert_eq!(workspace.state().cursor(), 0));

        // Moving is not opening, here as everywhere else.
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn ctrl_d_in_a_filter_field_is_not_the_tables(cx: &mut TestAppContext) {
        // It carries a modifier, so `is_typable` says nobody is typing it — and
        // it is still the key that deletes a character in every text field on
        // this machine.
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![reset("prod", pods(), &["api-0", "api-1", "api-2"])],
        );
        drain(&rx);
        harness.draw(cx);

        harness.update(cx, |workspace, window, cx| {
            workspace.search_input.update(cx, |input, cx| {
                input.focus(window, cx);
            });
        });
        harness.keys(cx, "ctrl-d");

        harness.read(cx, |workspace| assert_eq!(workspace.state().cursor(), 0));
    }

    #[gpui::test]
    fn the_kind_filter_narrows_the_sidebar(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        apply(&harness, cx, vec![contexts(&["prod"], "prod")]);
        apply(&harness, cx, vec![many_kinds("prod")]);

        harness.update(cx, |workspace, window, cx| {
            workspace
                .kind_filter_input
                .update(cx, |input, cx| input.set_value("cert", window, cx));
        });

        harness.read(cx, |workspace| {
            let matches: Vec<String> = periscope_store::sections(workspace.state().kinds())
                .into_iter()
                .flat_map(|(_, kinds)| kinds)
                .map(|info| info.id.label())
                .filter(|label| label.contains("cert"))
                .collect();

            assert_eq!(matches, ["certificates.cert-manager.io"]);
        });
        harness.update(cx, |workspace, _window, cx| {
            assert_eq!(workspace.kind_filter(cx), "cert");
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
    fn the_k9s_keys_work_too(cx: &mut TestAppContext) {
        // `:` for the jump prompt and `q` to go back are what the target user's
        // fingers already do.
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, ":");
        assert!(harness.read(cx, |workspace| workspace.palette_open()));

        harness.keys(cx, "escape");
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "q");
        assert!(harness.read(cx, |workspace| workspace.state().detail().is_none()));
    }

    #[gpui::test]
    fn closing_the_palette_gives_focus_back(cx: &mut TestAppContext) {
        // It used to stay on the palette's input, which is no longer on screen.
        // Every key bound only outside text fields then stopped working for the
        // rest of the session, and typing went into an invisible box.
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.keys(cx, "cmd-k");
        harness.keys(cx, "escape");

        harness.keys(cx, "q");
        assert!(
            harness.read(cx, |workspace| workspace.state().detail().is_none()),
            "a typable binding stopped working after the palette was used"
        );
    }

    #[gpui::test]
    fn a_letter_bound_to_a_command_is_still_typed_into_a_filter(cx: &mut TestAppContext) {
        // The risk single-letter bindings carry: `l` tails logs, and the
        // namespace field contains an `l` in almost every namespace anyone has.
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.namespace_input.update(cx, |input, cx| {
                input.focus(window, cx);
            });
        });
        harness.keys(cx, "l");

        harness.read(cx, |workspace| {
            assert!(
                workspace.state().logs().is_none(),
                "a keystroke meant for a text field started a log session"
            );
        });
        harness.update(cx, |workspace, _window, cx| {
            assert_eq!(workspace.namespace_input.read(cx).value(), "l");
        });
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn a_remapped_key_replaces_the_default(cx: &mut TestAppContext) {
        let mut keys = periscope_config::Keys::default();
        keys.bind(periscope_config::Command::Palette, ["cmd-p"]);
        let (harness, rx) = workspace_with_keys(cx, &keys);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        drain(&rx);

        harness.keys(cx, "cmd-p");
        assert!(harness.read(cx, |workspace| workspace.palette_open()));

        harness.keys(cx, "escape");
        // And the default is gone, because remapping replaces rather than adds.
        harness.keys(cx, "cmd-k");
        assert!(!harness.read(cx, |workspace| workspace.palette_open()));
    }

    #[gpui::test]
    fn a_keystroke_that_is_not_a_key_is_reported_rather_than_fatal(cx: &mut TestAppContext) {
        // `KeyBinding::new` panics on a malformed keystroke, and a typo in a
        // config file must not be able to take the app down before it has a
        // window to complain in.
        let mut keys = periscope_config::Keys::default();
        keys.bind(periscope_config::Command::Logs, ["ctrl-shift-nonsense-key"]);

        let problems = cx.update(|cx| {
            gpui_component::init(cx);
            init_with_keys(cx, &keys)
        });

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("logs"), "{}", problems[0]);
        assert!(
            problems[0].contains("ctrl-shift-nonsense-key"),
            "{}",
            problems[0]
        );
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

    /// The YAML of a pod with two containers and one init container.
    fn pod_yaml(name: &str) -> String {
        format!(
            "apiVersion: v1\n\
             kind: Pod\n\
             metadata:\n  \
               name: {name}\n  \
               namespace: default\n\
             spec:\n  \
               initContainers:\n  \
               - name: migrate\n  \
               containers:\n  \
               - name: api\n  \
               - name: envoy\n"
        )
    }

    /// Answers the detail fetch, which is where the container list comes from.
    ///
    /// The frame matters: the selector reads the object at render time, the
    /// same place the YAML editor is filled in.
    fn deliver_pod(harness: &Harness, cx: &mut TestAppContext, name: &str) {
        apply(
            harness,
            cx,
            vec![ClusterEvent::Object {
                cluster: "prod".into(),
                kind: pods(),
                detail: Arc::new(periscope_bridge::ObjectDetail {
                    key: ResourceKey::new("default", name),
                    yaml: Arc::from(pod_yaml(name).as_str()),
                    maskable: false,
                    revealed: true,
                    events: Arc::from([] as [periscope_bridge::EventLine; 0]),
                    owners: Arc::from([] as [periscope_bridge::OwnerRef; 0]),
                }),
            }],
        );
        harness.draw(cx);
    }

    /// A connected cluster with a pod open and its object delivered.
    fn workspace_with_pod(cx: &mut TestAppContext, name: &str) -> (Harness, CommandReceiver) {
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
        open_pod_detail(&harness, cx, name);
        deliver_pod(&harness, cx, name);
        (harness, rx)
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
            workspace.apply_filter(Lines::Logs, "error".to_owned(), cx);
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

    /// Opens a tail on `api-0` and feeds it lines with the given ages, in
    /// seconds before now, in the order they are listed.
    fn tail_with_ages(
        harness: &Harness,
        cx: &mut TestAppContext,
        ages: &[(&str, u64)],
    ) -> SystemTime {
        use periscope_bridge::{LogLine, LogSource};

        apply(
            harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(harness, cx, "api-0");
        harness.keys(cx, "cmd-l");

        let now = SystemTime::now();
        apply(
            harness,
            cx,
            vec![ClusterEvent::LogBatch {
                cluster: "prod".into(),
                lines: ages
                    .iter()
                    .map(|(pod, age)| LogLine {
                        source: LogSource::new(*pod, "api"),
                        timestamp: now.checked_sub(Duration::from_secs(*age)),
                        text: Arc::from(format!("{pod} at -{age}s").as_str()),
                    })
                    .collect(),
            }],
        );
        now
    }

    fn type_time(harness: &Harness, cx: &mut TestAppContext, typed: &str) {
        let typed = typed.to_owned();
        harness.update(cx, |workspace, window, cx| {
            workspace
                .log_time_input
                .update(cx, |input, cx| input.set_value(typed, window, cx));
            workspace.jump_to_time(cx);
        });
    }

    #[gpui::test]
    fn a_relative_time_scrolls_to_five_minutes_ago(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        // Ten minutes back, four minutes back, one minute back. "-5m" lands on
        // the four-minute line: the first one at or after the time asked for.
        tail_with_ages(
            &harness,
            cx,
            &[("api-0", 600), ("api-0", 240), ("api-0", 60)],
        );

        type_time(&harness, cx, "-5m");

        harness.read(cx, |workspace| {
            assert_eq!(workspace.log_anchor, Some(1));
            // Following would scroll straight back to the newest line on the
            // next batch, undoing the jump the user just asked for.
            assert!(!workspace.state().logs().expect("a tail").following);
            let notice = workspace.log_notice.as_ref().expect("a notice");
            assert!(notice.contains("line 2 of 3"), "{notice}");
        });
    }

    #[gpui::test]
    fn a_time_after_everything_held_says_so_rather_than_moving(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        tail_with_ages(&harness, cx, &[("api-0", 600), ("api-0", 300)]);

        type_time(&harness, cx, "-1s");

        harness.read(cx, |workspace| {
            assert_eq!(workspace.log_anchor, None);
            // A jump that found nothing must not pause a tail that was running.
            assert!(workspace.state().logs().expect("a tail").following);
            let notice = workspace.log_notice.as_ref().expect("a notice");
            assert!(notice.contains("No line at or after"), "{notice}");
        });
    }

    #[gpui::test]
    fn a_time_that_is_not_a_time_is_reported_in_the_pane(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        tail_with_ages(&harness, cx, &[("api-0", 60)]);

        type_time(&harness, cx, "half past four");

        harness.read(cx, |workspace| {
            assert_eq!(workspace.log_anchor, None);
            let notice = workspace.log_notice.as_ref().expect("a notice");
            assert!(notice.contains("not a time"), "{notice}");
        });
    }

    #[gpui::test]
    fn ordering_by_timestamp_interleaves_two_pods_backlogs(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace(cx);
        // One pod's backlog, then the other's: what `kubectl logs` gives, and
        // what makes a merged tail hard to read.
        tail_with_ages(
            &harness,
            cx,
            &[
                ("api-0", 300),
                ("api-0", 100),
                ("api-1", 200),
                ("api-1", 50),
            ],
        );

        harness.update(cx, |workspace, _window, cx| workspace.toggle_order(cx));

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().logs().expect("a tail").buffer;
            assert_eq!(buffer.order(), periscope_store::LogOrder::Timestamp);
            let texts: Vec<_> = buffer.visible().map(|line| line.text.to_string()).collect();
            assert_eq!(
                texts,
                [
                    "api-0 at -300s",
                    "api-1 at -200s",
                    "api-0 at -100s",
                    "api-1 at -50s"
                ]
            );
        });

        harness.update(cx, |workspace, _window, cx| workspace.toggle_order(cx));
        assert_eq!(
            harness.read(cx, |workspace| workspace
                .state()
                .logs()
                .expect("a tail")
                .buffer
                .order()),
            periscope_store::LogOrder::Arrival
        );
    }

    #[gpui::test]
    fn wrapping_shows_a_window_of_the_buffer_rather_than_all_of_it(cx: &mut TestAppContext) {
        use periscope_bridge::{LogLine, LogSource};

        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");

        let count = logview::WRAP_LIMIT * 4;
        apply(
            &harness,
            cx,
            vec![ClusterEvent::LogBatch {
                cluster: "prod".into(),
                lines: (0..count)
                    .map(|index| LogLine {
                        source: LogSource::new("api-0", "api"),
                        timestamp: None,
                        text: Arc::from(format!("line-{index}").as_str()),
                    })
                    .collect(),
            }],
        );

        harness.update(cx, |workspace, _window, cx| workspace.toggle_wrap(cx));

        harness.read(cx, |workspace| {
            assert!(workspace.log_wrap);
            let visible = workspace
                .state()
                .logs()
                .expect("a tail")
                .buffer
                .visible_len();
            assert_eq!(visible, count);
            // The whole point of the cap: an unvirtualised body must never be
            // handed the whole buffer, however many lines that is.
            let window = logview::wrap_window(visible, workspace.log_anchor);
            assert_eq!(window, (count - logview::WRAP_LIMIT)..count);
        });
    }

    #[gpui::test]
    fn a_wrapped_line_is_taller_than_one_that_fits(cx: &mut TestAppContext) {
        use periscope_bridge::{LogLine, LogSource};

        let (harness, _rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        harness.keys(cx, "cmd-l");

        apply(
            &harness,
            cx,
            vec![ClusterEvent::LogBatch {
                cluster: "prod".into(),
                lines: vec![
                    LogLine {
                        source: LogSource::new("api-0", "api"),
                        timestamp: None,
                        text: Arc::from("short"),
                    },
                    LogLine {
                        source: LogSource::new("api-0", "api"),
                        timestamp: None,
                        text: Arc::from("word ".repeat(400).as_str()),
                    },
                ]
                .into(),
            }],
        );
        harness.update(cx, |workspace, _window, cx| workspace.toggle_wrap(cx));
        harness.draw(cx);

        harness.read(cx, |workspace| {
            let scroll = &workspace.log_wrap_scroll;
            let short = scroll
                .bounds_for_item(0)
                .expect("the short line was painted");
            let long = scroll
                .bounds_for_item(1)
                .expect("the long line was painted");
            // Wrapping is a layout property, and the only way to know it
            // actually happened — rather than the row scrolling sideways out of
            // sight — is that the row got taller.
            assert!(
                long.size.height > short.size.height * 2.,
                "the long line was {} tall and the short one {}",
                long.size.height,
                short.size.height
            );
        });
    }

    #[gpui::test]
    fn a_confirmed_mutation_goes_to_the_cluster_the_sentence_named(cx: &mut TestAppContext) {
        // The palette is reachable while a proposal is open, and it changes the
        // active cluster. Resolving the cluster again at confirm time meant the
        // dialog could say prod and the delete could land on staging — the one
        // thing ADR-0030 exists to make impossible.
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod", "staging"], "prod"), kinds_event("prod")],
        );
        apply(
            &harness,
            cx,
            vec![
                ClusterEvent::Status {
                    cluster: "prod".into(),
                    state: ConnectionState::Connected,
                },
                ClusterEvent::Status {
                    cluster: "staging".into(),
                    state: ConnectionState::Connected,
                },
            ],
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
            // The user wanders off to another cluster with the dialog open.
            workspace.select_cluster(ClusterId::new("staging"), cx);
        });
        drain(&rx);

        harness.read(cx, |workspace| {
            let sentence = workspace
                .pending
                .as_ref()
                .expect("still pending")
                .confirmation();
            assert!(sentence.contains("prod"), "{sentence}");
            assert!(!sentence.contains("staging"), "{sentence}");
        });

        harness.update(cx, |workspace, _window, cx| workspace.confirm_mutation(cx));

        match drain(&rx).as_slice() {
            [ClusterCommand::Mutate { cluster, .. }] => {
                assert_eq!(
                    cluster,
                    &ClusterId::new("prod"),
                    "sent to the wrong cluster"
                );
            }
            other => panic!("expected one mutation, got {other:?}"),
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
                .confirmation();
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
    fn the_containers_offered_come_from_the_object_the_pane_already_fetched(
        cx: &mut TestAppContext,
    ) {
        let (harness, rx) = workspace(cx);
        apply(
            &harness,
            cx,
            vec![contexts(&["prod"], "prod"), kinds_event("prod")],
        );
        open_pod_detail(&harness, cx, "api-0");
        drain(&rx);

        deliver_pod(&harness, cx, "api-0");

        harness.read(cx, |workspace| {
            let names: Vec<_> = workspace
                .exec_containers
                .iter()
                .map(|container| &*container.name)
                .collect();
            // The pod's own containers first, then the init container, which is
            // the order the apiserver's default makes sense in.
            assert_eq!(names, ["api", "envoy", "migrate"]);
            assert!(workspace.exec_containers[2].init);
            // Nothing is chosen until somebody chooses.
            assert!(workspace.exec_container.is_none());
        });
        // And it cost no request: the object was already here.
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn a_picked_container_is_named_in_the_confirmation_and_in_what_is_sent(
        cx: &mut TestAppContext,
    ) {
        let (harness, rx) = workspace_with_pod(cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace.pick_container(Some(Arc::from("envoy")), cx);
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("ps", window, cx));
            workspace.run_command(cx);
        });

        // Which container a command runs in changes what is being run, so the
        // sentence the user agrees to has to say it.
        harness.read(cx, |workspace| {
            let sentence = workspace
                .pending
                .as_ref()
                .expect("a command is waiting")
                .confirmation();
            assert!(sentence.contains("container envoy"), "{sentence}");
        });

        harness.update(cx, |workspace, _window, cx| workspace.confirm_mutation(cx));

        match drain(&rx).as_slice() {
            [ClusterCommand::Exec { target, .. }] => {
                assert_eq!(target.container.as_deref(), Some("envoy"));
            }
            other => panic!("expected one exec, got {other:?}"),
        }
    }

    #[gpui::test]
    fn no_container_picked_leaves_the_choice_to_the_apiserver(cx: &mut TestAppContext) {
        let (harness, rx) = workspace_with_pod(cx, "api-0");
        drain(&rx);

        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("ps", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });

        match drain(&rx).as_slice() {
            [ClusterCommand::Exec { target, .. }] => assert!(target.container.is_none()),
            other => panic!("expected one exec, got {other:?}"),
        }
    }

    #[gpui::test]
    fn opening_another_pod_forgets_the_container_that_was_picked(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace_with_pod(cx, "api-0");

        harness.update(cx, |workspace, _window, cx| {
            workspace.pick_container(Some(Arc::from("envoy")), cx);
        });
        open_pod_detail(&harness, cx, "api-1");
        // Before the new object has even arrived: Run is on screen throughout,
        // and a name that meant a sidecar in the last pod is a way to run a
        // command in the wrong place in this one.
        harness.read(cx, |workspace| {
            assert!(workspace.exec_container.is_none());
            assert!(workspace.exec_containers.is_empty());
        });

        deliver_pod(&harness, cx, "api-1");
        harness.read(cx, |workspace| assert!(workspace.exec_container.is_none()));
    }

    #[gpui::test]
    fn escape_puts_the_container_list_away_before_anything_else(cx: &mut TestAppContext) {
        let (harness, _rx) = workspace_with_pod(cx, "api-0");

        harness.update(cx, |workspace, _window, cx| {
            workspace.toggle_container_menu(cx);
        });
        // Painted, so the list is an element tree that really builds and not
        // just a bool nobody looked at.
        harness.draw(cx);

        harness.keys(cx, "escape");

        harness.read(cx, |workspace| {
            assert!(!workspace.container_menu_open);
            // The pane the button belongs to stayed.
            assert!(workspace.state().detail().is_some());
        });
    }

    /// A session with three lines of output in it.
    fn command_with_output(cx: &mut TestAppContext) -> (Harness, CommandReceiver) {
        let (harness, rx) = workspace_with_pod(cx, "api-0");
        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("dmesg", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });
        apply(
            &harness,
            cx,
            vec![ClusterEvent::ExecOutput {
                cluster: "prod".into(),
                lines: ["starting", "an error", "done"]
                    .iter()
                    .map(|text| periscope_bridge::LogLine {
                        source: periscope_bridge::LogSource::new("api-0", "stdout"),
                        timestamp: None,
                        text: Arc::from(*text),
                    })
                    .collect(),
            }],
        );
        // The pane with its toolbar is painted, not merely computed.
        harness.draw(cx);
        drain(&rx);
        (harness, rx)
    }

    #[gpui::test]
    fn filtering_command_output_narrows_it_without_re_running_anything(cx: &mut TestAppContext) {
        let (harness, rx) = command_with_output(cx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.apply_filter(Lines::Command, "error".to_owned(), cx);
        });

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().exec().expect("a session").buffer;
            assert_eq!(buffer.visible_len(), 1);
            assert_eq!(buffer.len(), 3);
        });
        // The output is held here; narrowing it is not a reason to run the
        // command again.
        assert!(drain(&rx).is_empty());
    }

    #[gpui::test]
    fn a_filter_that_will_not_compile_says_so_rather_than_showing_nothing(cx: &mut TestAppContext) {
        let (harness, _rx) = command_with_output(cx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.toggle_filter_mode(Lines::Command, true, cx);
            workspace.apply_filter(Lines::Command, "err(".to_owned(), cx);
        });

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().exec().expect("a session").buffer;
            assert!(buffer.filter_error().is_some());
        });
    }

    #[gpui::test]
    fn copying_command_output_takes_what_is_on_screen_and_says_how_much(cx: &mut TestAppContext) {
        let (harness, _rx) = command_with_output(cx);

        harness.update(cx, |workspace, _window, cx| {
            workspace.apply_filter(Lines::Command, "error".to_owned(), cx);
            workspace.copy_lines(Lines::Command, cx);
        });

        harness.read(cx, |workspace| {
            let said = workspace.exec_notice.as_ref().expect("it says what it did");
            // The filtered lines, not the whole buffer: what is copied is what
            // is being read.
            assert!(said.contains("Copied 1 lines"), "{said}");
            // And the log view, which has its own notice, said nothing.
            assert!(workspace.log_notice.is_none());
        });
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("something reached the clipboard");
        assert!(copied.contains("an error"), "{copied}");
        assert!(!copied.contains("starting"), "{copied}");
    }

    #[gpui::test]
    fn closing_the_command_pane_empties_its_filter_box(cx: &mut TestAppContext) {
        let (harness, _rx) = command_with_output(cx);

        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_filter_input
                .update(cx, |input, cx| input.set_value("error", window, cx));
        });
        harness.update(cx, |workspace, window, cx| {
            workspace.close_command(window, cx);
        });

        // A box still holding a pattern nothing is applying is a lie about
        // what the next command's output has been narrowed to.
        harness.update(cx, |workspace, _window, cx| {
            assert!(workspace.state().exec().is_none());
            assert!(workspace.exec_filter_input.read(cx).value().is_empty());
        });
    }

    #[gpui::test]
    fn running_a_second_command_keeps_the_filter_the_first_was_read_through(
        cx: &mut TestAppContext,
    ) {
        let (harness, rx) = command_with_output(cx);

        harness.update(cx, |workspace, window, cx| {
            workspace
                .exec_filter_input
                .update(cx, |input, cx| input.set_value("error", window, cx));
            workspace
                .exec_input
                .update(cx, |input, cx| input.set_value("dmesg", window, cx));
            workspace.run_command(cx);
            workspace.confirm_mutation(cx);
        });
        drain(&rx);

        harness.read(cx, |workspace| {
            let buffer = &workspace.state().exec().expect("a session").buffer;
            assert_eq!(buffer.filter().pattern, "error");
        });
    }

    #[test]
    fn an_export_name_survives_a_command_line_longer_than_a_file_name() {
        let label = format!("default/api-0 $ sh -c {}", "x".repeat(400));
        let name = export_name(&label, 3);

        assert!(name.len() < 120, "{name}");
        assert!(name.starts_with("periscope-default-api-0-"), "{name}");
        assert!(name.ends_with("-3.log"), "{name}");
        // Nothing a path or a shell would read as structure survives.
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains('$'), "{name}");
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

            let sentence = workspace.pending.as_ref().unwrap().confirmation();

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
