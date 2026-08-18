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
use periscope_bridge::{ClusterRuntime, PumpConfig, RuntimeConfig, spawn_event_pump};
use periscope_cluster::KubeHandler;
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

    let handler = match cli.kubeconfig.clone() {
        Some(path) => KubeHandler::with_kubeconfig(path),
        None => KubeHandler::new(),
    };
    let (runtime, events) = ClusterRuntime::start(handler, RuntimeConfig::default())
        .context("could not start the cluster runtime")?;
    let commands = runtime.commands();

    let perf = cli.perf;
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            cx.set_global(RuntimeHandle(runtime));

            // The window is opened from a task because `open_window` needs to run
            // after the platform has finished launching.
            cx.spawn(async move |cx| {
                let slot: Rc<RefCell<Option<gpui::Entity<Workspace>>>> =
                    Rc::new(RefCell::new(None));
                let captured = Rc::clone(&slot);
                let commands = commands.clone();

                cx.open_window(window_options(), move |window, cx| {
                    let workspace = cx.new(|cx| Workspace::new(commands.clone(), started, cx));
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
