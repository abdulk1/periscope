//! Periscope's entry point: parse flags, start the cluster runtime, open a
//! window, and connect the two through the bridge.

mod cli;
mod update;

use std::cell::RefCell;
use std::io::IsTerminal as _;
use std::rc::Rc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Global, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    point, px, rgb, size,
};
use gpui_component::Root;
use gpui_component_assets::Assets;
use periscope_bridge::{ClusterRuntime, LogTarget, PumpConfig, RuntimeConfig, spawn_event_pump};
use periscope_cluster::{KubeHandler, WritePolicy};
use periscope_ui::Workspace;

use crate::cli::Cli;

/// Keeps the cluster runtime alive for the process lifetime and shuts it down
/// cleanly when GPUI tears the app down.
struct RuntimeHandle(#[allow(dead_code)] ClusterRuntime);

impl Global for RuntimeHandle {}

/// Keeps the event pump alive; dropping the task would stop the bridge.
struct PumpHandle(#[allow(dead_code)] gpui::Task<()>);

impl Global for PumpHandle {}

/// The message a panic carried, however it was raised.
fn payload_of(payload: &(dyn std::any::Any + Send)) -> String {
    // `panic!("literal")` gives a `&str`; `panic!("{x}")` and `expect` give a
    // `String`; a panic carrying anything else gives neither.
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// Sends panics to the log as well as to stderr.
///
/// Most of this program's work happens on tokio tasks and GPUI's executors,
/// where a panic unwinds one task and is otherwise silent — the window stays
/// up, the watch is simply gone, and the log file that gets attached to the bug
/// report says nothing at all. Every panic now leaves a record.
///
/// The payload is redacted on the way to the file and left intact on stderr: a
/// panic message can carry whatever was being processed, which for this program
/// can be an object's contents, and the file outlives the session.
fn record_panics() {
    let default = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let payload = payload_of(info.payload());
        let at = info
            .location()
            .map(|location| location.to_string())
            .unwrap_or_else(|| "an unknown location".to_owned());

        tracing::error!(
            at,
            thread = std::thread::current().name().unwrap_or("unnamed"),
            payload = periscope_cluster::redact::text(&payload),
            "panicked"
        );

        default(info);
    }));
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Returning the error is what prints it: `Termination` writes it to stderr
    // and exits non-zero, which is right for anyone who typed `scope`. Nobody
    // who double-clicked an icon has a stderr, so they get a window first.
    match start(cli) {
        Ok(()) => Ok(()),
        Err(error) => {
            show_startup_failure(&error);
            Err(error)
        }
    }
}

/// Shows a fatal startup error to somebody who has no terminal to read.
///
/// Everything before `Application::run` — logging, settings, the audit log, the
/// cluster runtime — used to fail by returning out of `main`. From a terminal
/// that is fine. Launched from Finder or a desktop entry it is not: stderr goes
/// nowhere, so a malformed `settings.toml` produced an application that bounced
/// once in the Dock and vanished, with the explanation written to a stream
/// nobody was reading.
///
/// Refusing to start on a malformed settings file is deliberate — starting with
/// permissive defaults when somebody has written a read-only rule is the worst
/// thing this program could do — which makes it all the more important that the
/// refusal is legible.
fn show_startup_failure(error: &anyhow::Error) {
    // A terminal has already been given it, or is about to be.
    if std::io::stderr().is_terminal() {
        return;
    }

    // Not `{error:#}`. That walks the chain and re-appends each source, and
    // these errors already quote their own — `SettingsError::Parse` is
    // "{path} is not valid settings: {source}" — so the alternate form printed
    // the whole TOML diagnostic twice. `describe` is the same walk with the
    // repetition dropped; it was written for `kube`, which nests errors the
    // same way.
    let message = SharedString::from(failure_message(error));

    Application::new().with_assets(Assets).run(move |cx| {
        cx.activate(true);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let opened = cx.open_window(failure_window_options(), |_, cx| {
            cx.new(|_| StartupFailure {
                message: message.clone(),
            })
        });

        // Nothing left to try: the caller returns the error and stderr gets it,
        // wherever stderr happens to go.
        if opened.is_err() {
            cx.quit();
        }
    });
}

/// The text the failure window shows.
fn failure_message(error: &anyhow::Error) -> String {
    periscope_cluster::errors::describe(error.as_ref())
}

/// The whole of startup, so that any failure in it has one place to be caught.
fn start(cli: Cli) -> Result<()> {
    let started = Instant::now();

    // Logging first, so a failure anywhere below is recorded.
    let log_guard = periscope_config::logging::init(cli.verbosity(), cli.verbose)
        .context("could not initialise logging")?;
    record_panics();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_dir = %log_guard.directory().display(),
        perf = cli.perf,
        "starting Periscope"
    );

    // Settings decide which clusters may be changed. A malformed file is fatal
    // rather than ignored: starting with permissive defaults when someone has
    // written a read-only rule would be the worst possible failure here.
    let settings = periscope_config::Settings::read().context("could not read settings")?;
    let audit = periscope_config::AuditLog::open().context("could not open the audit log")?;
    // The store refuses before anything is sent, so the cluster layer never
    // sees a refusal; the window needs its own handle to record one.
    let refusals = audit.clone();
    // Remembered UI state, which is the opposite of settings in every way that
    // matters here: the app writes it, and losing it costs a collapsed section
    // rather than a safety rule, so nothing about it is fatal.
    let state_file = periscope_config::StateFile::open()
        .context("could not decide where to remember the sidebar")?;
    let (ui_state, state_problem) = state_file.read();
    if let Some(problem) = state_problem {
        tracing::warn!(%problem, "starting with a fresh sidebar");
    }
    // Logged in full because "the setting had no effect" is otherwise
    // indistinguishable from "the file was never read".
    tracing::info!(
        read_only = settings.access.read_only.len(),
        read_only_by_default = settings.access.read_only_by_default,
        theme = settings.theme.label(),
        idle_timeout_s = settings.limits.idle_timeout.get().as_secs(),
        row_budget = settings.limits.row_budget,
        log_buffer = settings.limits.log_buffer,
        audit = %audit.path().display(),
        state = %state_file.path().display(),
        sections_remembered = ui_state.sections.len(),
        kinds_remembered = ui_state.recent.len(),
        "settings loaded"
    );

    let handler = match cli.kubeconfig.clone() {
        Some(path) => KubeHandler::with_kubeconfig(path),
        None => KubeHandler::new(),
    }
    .with_policy(WritePolicy::from_access(&settings.access))
    .with_audit(audit);
    let (runtime, events) = ClusterRuntime::start(handler, RuntimeConfig::default())
        .context("could not start the cluster runtime")?;
    let commands = runtime.commands();

    // The one network call that is not to a cluster, and only when asked for.
    // It runs on the cluster runtime because `reqwest` needs a tokio reactor,
    // and it answers through a channel the window awaits below.
    if let Some(problem) = settings.updates.problem() {
        tracing::warn!(%problem, "the update check is switched on but will not run");
    }
    let update = update::start(
        runtime.handle(),
        &settings.updates,
        periscope_config::Version::current(),
    );

    let perf = cli.perf;
    // `--tail app=web -n prod` opens the log view on the pods it names as soon
    // as the cluster connects.
    let tail = cli
        .tail
        .as_deref()
        .zip(cli.namespace.as_deref())
        .map(|(selector, namespace)| LogTarget::labels(namespace, selector));
    // The store's copy of the policy; the cluster layer has its own.
    let permissions = periscope_store::Permissions::from_access(&settings.access);
    let theme = settings.theme;
    let limits = settings.limits;
    let keys = settings.keys.clone();
    let columns = settings.columns.clone();

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            // A keystroke in settings.toml that is not a key must not take the
            // app down before there is a window to complain in: the rest of the
            // keymap is bound, and the problems are put on screen below.
            let keymap_problems = periscope_ui::init_with_keys(cx, &keys);
            for problem in &keymap_problems {
                tracing::warn!(%problem, "keymap");
            }
            cx.set_global(RuntimeHandle(runtime));

            // The window is opened from a task because `open_window` needs to run
            // after the platform has finished launching.
            cx.spawn(async move |cx| {
                let slot: Rc<RefCell<Option<gpui::Entity<Workspace>>>> =
                    Rc::new(RefCell::new(None));
                let captured = Rc::clone(&slot);
                let commands = commands.clone();

                cx.open_window(window_options(), move |window, cx| {
                    let workspace = cx.new(|cx| {
                        let mut workspace = Workspace::with_tail(
                            commands.clone(),
                            started,
                            perf,
                            tail.clone(),
                            window,
                            cx,
                        );
                        workspace.set_permissions(permissions.clone());
                        workspace.set_audit(refusals.clone());
                        workspace.set_limits(limits);
                        workspace.set_columns(columns.clone());
                        workspace.restore(ui_state.clone(), state_file.clone());
                        workspace.set_theme(theme, window, cx);
                        if let Some(problem) = keymap_problems.first() {
                            workspace.report(problem.clone());
                        }
                        workspace
                    });
                    *captured.borrow_mut() = Some(workspace.clone());
                    cx.new(|cx| Root::new(workspace, window, cx))
                })?;

                let workspace = slot
                    .borrow_mut()
                    .take()
                    .context("the window closed before it finished opening")?;

                // A newer version, if the check found one and the user asked
                // for it. Never a download and never a prompt: one line in the
                // place every other notice appears.
                if let Some(update) = update {
                    let workspace = workspace.clone();
                    cx.spawn(async move |cx| {
                        let Ok(release) = update.await else {
                            return;
                        };
                        let _ = cx.update(|cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace
                                    .report(release.notice(periscope_config::Version::current()));
                                cx.notify();
                            });
                        });
                    })
                    .detach();
                }

                cx.update(|cx| {
                    let pump = spawn_event_pump(
                        events,
                        PumpConfig::default(),
                        cx,
                        move |batch, stats, cx| {
                            let applied = std::time::Instant::now();
                            let rows = workspace.update(cx, |workspace, cx| {
                                workspace.apply_events(batch, stats, cx);
                                workspace.state().rows().len()
                            });

                            if perf && stats.applied > 0 {
                                tracing::info!(
                                    applied = stats.applied,
                                    collapsed = stats.collapsed,
                                    dropped = stats.dropped,
                                    rows,
                                    apply_us = applied.elapsed().as_micros() as u64,
                                    "flush"
                                );
                            }
                        },
                    );
                    cx.set_global(PumpHandle(pump));
                })?;

                Ok::<_, anyhow::Error>(())
            })
            .detach_and_log_err(cx);

            // Quit when the last window closes: this is a single-window app.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            cx.activate(true);
        });

    tracing::info!("Periscope exited");
    Ok(())
}

/// The window that says why there is no window.
///
/// Deliberately built from raw GPUI with literal colours: it must render when
/// the theme, the settings and the log file have all failed, so it depends on
/// none of them. It is not themed, and that is the point — this is the one
/// screen that has to work when nothing else did.
struct StartupFailure {
    message: SharedString,
}

impl Render for StartupFailure {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_8()
            .bg(rgb(0x16181d))
            .text_color(rgb(0xe8eaed))
            .child(div().text_lg().child("Periscope could not start"))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(rgb(0xf28b82))
                    .child(self.message.clone()),
            )
            .child(div().text_xs().text_color(rgb(0x9aa0a6)).child(
                "Fix the file named above, or delete it to start with defaults. \
                         Settings are read once, at startup.",
            ))
    }
}

fn failure_window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(160.), px(160.)),
            // Roomy on purpose: a TOML parse error arrives with the offending
            // line and a caret under it, and truncating the one thing that says
            // *where* the problem is would defeat the window.
            size: size(px(860.), px(560.)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some("Periscope".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(120.), px(120.)),
            size: size(px(1_200.), px(800.)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some("Periscope".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_startup_failure_says_it_once() {
        // `{error:#}` walks the chain re-appending every source, and these
        // errors already quote their own — so the window printed the whole TOML
        // diagnostic, caret and all, twice.
        #[derive(Debug, thiserror::Error)]
        #[error("{path} is not valid settings: {source}")]
        struct Parse {
            path: String,
            source: Inner,
        }

        #[derive(Debug, thiserror::Error)]
        #[error("TOML parse error at line 2, column 14")]
        struct Inner;

        let error = anyhow::Error::new(Parse {
            path: "/home/u/settings.toml".to_owned(),
            source: Inner,
        })
        .context("could not read settings");

        let shown = failure_message(&error);

        // The path is the part that makes it actionable, and the parse error is
        // the part that says where.
        assert!(shown.contains("/home/u/settings.toml"), "{shown}");
        assert_eq!(
            shown.matches("TOML parse error").count(),
            1,
            "the source is repeated: {shown}"
        );
    }

    #[test]
    fn a_panic_message_survives_however_it_was_raised() {
        // `panic!("literal")`, `panic!("{x}")` and a payload that is neither.
        assert_eq!(payload_of(&"boom"), "boom");
        assert_eq!(payload_of(&"boom".to_owned()), "boom");
        assert_eq!(payload_of(&7u32), "a panic with no message");
    }

    #[test]
    fn a_panic_does_not_write_a_hostname_into_the_log() {
        // A panic message carries whatever was being handled when it happened,
        // and the log file outlives the session.
        let payload = payload_of(
            &"assertion failed: response from https://A1B2C3.gr7.us-east-1.eks.amazonaws.com"
                .to_owned(),
        );
        let logged = periscope_cluster::redact::text(&payload);

        assert!(!logged.contains("eks.amazonaws.com"), "{logged}");
        assert!(logged.contains("assertion failed"), "{logged}");
    }
}
