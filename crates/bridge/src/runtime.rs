//! The tokio side of the bridge.
//!
//! `kube-rs` needs a tokio runtime; GPUI has its own executors and owns the main
//! thread. Rather than trying to reconcile them, [`ClusterRuntime`] parks a
//! multi-threaded tokio runtime on a dedicated background thread and talks to
//! the UI only through the channels in [`crate::channel`].

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use futures::future::BoxFuture;

use crate::channel::{
    CommandError, CommandReceiver, CommandSender, EventSink, EventStream, command_channel,
    event_channel,
};
use crate::protocol::ClusterCommand;

/// How long to wait for in-flight cluster work to wind down on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Whatever actually performs cluster work.
///
/// Implemented by the `cluster` crate. Kept as a trait here so the bridge can be
/// tested — and this crate compiled — without kube.
pub trait CommandHandler: Send + Sync + 'static {
    /// Handles one command. Runs on the tokio runtime, so it may block on I/O
    /// via `.await` but must not block the thread.
    fn handle(&self, command: ClusterCommand, events: EventSink) -> BoxFuture<'static, ()>;

    /// Called once after the command channel closes, before the runtime stops.
    fn shutdown(&self, _events: EventSink) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Runtime tuning. Defaults are sized for interactive use, not for benchmarks.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// tokio worker threads. `None` lets tokio pick (one per core).
    pub worker_threads: Option<usize>,
    /// Bound on queued UI -> cluster commands.
    pub command_capacity: usize,
    /// Bound on queued cluster -> UI events. Overflow drops and marks stale.
    pub event_capacity: usize,
    /// Prefix for the runtime's thread names, so stacks are readable.
    pub thread_name: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: None,
            command_capacity: 256,
            // Deep enough to absorb a resync burst between two 16ms flushes.
            event_capacity: 16_384,
            thread_name: "periscope-cluster".into(),
        }
    }
}

/// Why the runtime failed to start.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The tokio runtime could not be built.
    #[error("could not start the cluster runtime: {0}")]
    Build(#[source] std::io::Error),
    /// The runtime thread died before reporting readiness.
    #[error("the cluster runtime thread exited during startup")]
    StartupAborted,
}

/// Owns the tokio runtime for the process lifetime.
///
/// Dropping this shuts the runtime down: the command channel closes, the
/// dispatch loop ends, in-flight work gets [`SHUTDOWN_GRACE`] to finish, and the
/// thread is joined.
#[derive(Debug)]
pub struct ClusterRuntime {
    commands: Option<CommandSender>,
    handle: tokio::runtime::Handle,
    thread: Option<JoinHandle<()>>,
}

impl ClusterRuntime {
    /// Starts the runtime thread and returns it alongside the UI's event stream.
    pub fn start(
        handler: impl CommandHandler,
        config: RuntimeConfig,
    ) -> Result<(Self, EventStream), RuntimeError> {
        let (commands, command_rx) = command_channel(config.command_capacity);
        let (events, stream) = event_channel(config.event_capacity);

        // The thread reports back either the runtime handle or the build error.
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let handler = Arc::new(handler);
        let thread_name = config.thread_name.clone();
        let worker_threads = config.worker_threads;

        let thread = thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.enable_all().thread_name(format!("{thread_name}-w"));
                if let Some(threads) = worker_threads {
                    builder.worker_threads(threads.max(1));
                }

                let runtime = match builder.build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        // Send failure is fine: it only means the caller gave up.
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };

                if ready_tx.send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }

                runtime.block_on(dispatch(handler, command_rx, events));
                runtime.shutdown_timeout(SHUTDOWN_GRACE);
                tracing::debug!("cluster runtime stopped");
            })
            .map_err(RuntimeError::Build)?;

        let handle = match ready_rx.recv() {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => return Err(RuntimeError::Build(error)),
            Err(_) => return Err(RuntimeError::StartupAborted),
        };

        Ok((
            Self {
                commands: Some(commands),
                handle,
                thread: Some(thread),
            },
            stream,
        ))
    }

    /// A clonable handle for sending commands from anywhere, including the UI.
    pub fn commands(&self) -> CommandSender {
        self.commands
            .clone()
            .expect("commands are only taken during shutdown")
    }

    /// Queues a command for the cluster layer.
    pub fn send(&self, command: ClusterCommand) -> Result<(), CommandError> {
        match &self.commands {
            Some(commands) => commands.send(command),
            None => Err(CommandError::Closed),
        }
    }

    /// The tokio handle, for code that needs to spawn onto this runtime directly.
    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }

    /// Shuts the runtime down and waits for the thread. Idempotent.
    pub fn shutdown(&mut self) {
        // Dropping the sender ends the dispatch loop.
        self.commands = None;
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("cluster runtime thread panicked");
        }
    }
}

impl Drop for ClusterRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reads commands until the UI hangs up, running each handler concurrently.
async fn dispatch(handler: Arc<impl CommandHandler>, commands: CommandReceiver, events: EventSink) {
    let mut tasks = tokio::task::JoinSet::new();

    while let Some(command) = commands.recv().await {
        // Reap finished tasks so a long session does not accumulate handles.
        while tasks.try_join_next().is_some() {}

        let handler = Arc::clone(&handler);
        let events = events.clone();
        tasks.spawn(async move {
            handler.handle(command, events).await;
        });
    }

    handler.shutdown(events).await;
    tasks.shutdown().await;
}

/// A cluster-free handler used to exercise the bridge end to end.
///
/// It answers pings from a real tokio task, which is exactly the shape the kube
/// handler will have, without needing a cluster.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct ClusterHandlerFixture {
    seen: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClusterEvent, ConnectionState};
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    type EchoHandler = ClusterHandlerFixture;

    impl CommandHandler for ClusterHandlerFixture {
        fn handle(&self, command: ClusterCommand, events: EventSink) -> BoxFuture<'static, ()> {
            self.seen.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                match command {
                    ClusterCommand::Ping { cluster, nonce } => {
                        let started = Instant::now();
                        // Prove we are on a real async runtime, not a stub.
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        events.send(ClusterEvent::Pong {
                            cluster,
                            nonce,
                            elapsed: started.elapsed(),
                        });
                    }
                    ClusterCommand::Connect { cluster } => {
                        events.send(ClusterEvent::Status {
                            cluster,
                            state: ConnectionState::Connected,
                        });
                    }
                    ClusterCommand::Disconnect { cluster } => {
                        events.send(ClusterEvent::Status {
                            cluster,
                            state: ConnectionState::Disconnected { reason: None },
                        });
                    }
                    ClusterCommand::ListContexts => {
                        events.send(ClusterEvent::Contexts {
                            contexts: Arc::from([] as [crate::resource::ContextInfo; 0]),
                            current: None,
                        });
                    }
                    // The fixture exists to prove the bridge carries messages,
                    // not to imitate a cluster; the rest is the kube handler's.
                    _ => {}
                }
            })
        }

        fn shutdown(&self, events: EventSink) -> BoxFuture<'static, ()> {
            Box::pin(async move {
                events.send(ClusterEvent::Status {
                    cluster: "runtime".into(),
                    state: ConnectionState::Disconnected { reason: None },
                });
            })
        }
    }

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            worker_threads: Some(2),
            ..RuntimeConfig::default()
        }
    }

    /// Blocking receive with a deadline, so a broken bridge fails rather than hangs.
    fn recv_timeout(stream: &EventStream, timeout: Duration) -> Option<ClusterEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(event) = stream.try_recv() {
                return Some(event);
            }
            thread::sleep(Duration::from_millis(2));
        }
        None
    }

    #[test]
    fn a_command_reaches_the_runtime_and_an_event_comes_back() {
        let handler = EchoHandler::default();
        let seen = Arc::clone(&handler.seen);
        let (runtime, stream) = ClusterRuntime::start(handler, test_config()).unwrap();

        runtime
            .send(ClusterCommand::Ping {
                cluster: "prod".into(),
                nonce: 42,
            })
            .unwrap();

        let event = recv_timeout(&stream, Duration::from_secs(5)).expect("pong within 5s");
        match event {
            ClusterEvent::Pong { cluster, nonce, .. } => {
                assert_eq!(cluster.as_str(), "prod");
                assert_eq!(nonce, 42);
            }
            other => panic!("expected a pong, got {other:?}"),
        }
        assert_eq!(seen.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn commands_are_handled_concurrently() {
        let (runtime, stream) =
            ClusterRuntime::start(EchoHandler::default(), test_config()).unwrap();

        for nonce in 0..32 {
            runtime
                .send(ClusterCommand::Ping {
                    cluster: "prod".into(),
                    nonce,
                })
                .unwrap();
        }

        let mut nonces = Vec::new();
        while nonces.len() < 32 {
            match recv_timeout(&stream, Duration::from_secs(5)) {
                Some(ClusterEvent::Pong { nonce, .. }) => nonces.push(nonce),
                Some(_) => {}
                None => break,
            }
        }
        nonces.sort_unstable();
        assert_eq!(nonces, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn shutdown_runs_the_handler_hook_and_joins_the_thread() {
        let (mut runtime, stream) =
            ClusterRuntime::start(EchoHandler::default(), test_config()).unwrap();

        runtime.shutdown();
        // Idempotent: a second call must not panic or hang.
        runtime.shutdown();

        let event = stream.try_recv().expect("shutdown hook emitted an event");
        assert!(matches!(event, ClusterEvent::Status { .. }));
        assert_eq!(
            runtime.send(ClusterCommand::Disconnect {
                cluster: "prod".into()
            }),
            Err(CommandError::Closed)
        );
    }

    #[test]
    fn the_tokio_handle_is_usable_from_the_caller() {
        let (runtime, _stream) =
            ClusterRuntime::start(EchoHandler::default(), test_config()).unwrap();
        let answer = runtime.handle().block_on(async { 7 * 6 });
        assert_eq!(answer, 42);
    }
}
