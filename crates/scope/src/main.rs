//! Periscope's entry point: parse flags, start the cluster runtime, open a
//! window, and connect the two through the bridge.

mod cli;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use gpui::{
    App, AppContext as _, Application, Bounds, Global, TitlebarOptions, WindowBounds,
    WindowOptions, point, px, size,
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

fn main() -> Result<()> {
    let started = Instant::now();
    let cli = Cli::parse();

    // Logging first, so a failure anywhere below is recorded.
    let log_guard = periscope_config::logging::init(cli.verbosity(), cli.verbose)
        .context("could not initialise logging")?;
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
                        workspace.set_limits(limits);
                        workspace.set_columns(columns.clone());
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
