//! Bounded channels with an explicit overflow policy.
//!
//! Both directions are bounded, but they overflow differently:
//!
//! * Events (cluster -> UI) are **dropped and marked stale**. Blocking a
//!   watcher because the UI is busy would stall the cluster layer and, worse,
//!   apply backpressure to the Kubernetes API watch stream.
//! * Commands (UI -> cluster) are **never silently dropped**. A lost command
//!   means a button that did nothing, so a full channel is surfaced as an
//!   error the UI can render.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::protocol::{ClusterCommand, ClusterEvent};

/// What happened to an event handed to [`EventSink::send`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// Queued for the UI.
    Delivered,
    /// The channel was full; the event was discarded and the drop counter bumped.
    Dropped,
    /// The UI is gone. The caller should wind down.
    Closed,
}

impl SendOutcome {
    /// Whether the receiver has hung up.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// Cluster-layer end of the event channel. Cheap to clone, never blocks.
#[derive(Clone, Debug)]
pub struct EventSink {
    tx: flume::Sender<ClusterEvent>,
    dropped: Arc<AtomicUsize>,
}

impl EventSink {
    /// Offers an event to the UI. Returns without blocking in every case.
    pub fn send(&self, event: ClusterEvent) -> SendOutcome {
        match self.tx.try_send(event) {
            Ok(()) => SendOutcome::Delivered,
            Err(flume::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                SendOutcome::Dropped
            }
            Err(flume::TrySendError::Disconnected(_)) => SendOutcome::Closed,
        }
    }

    /// Total events discarded because the channel was full.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// UI end of the event channel.
#[derive(Clone, Debug)]
pub struct EventStream {
    rx: flume::Receiver<ClusterEvent>,
    dropped: Arc<AtomicUsize>,
}

impl EventStream {
    /// Takes the next event without waiting.
    pub fn try_recv(&self) -> Option<ClusterEvent> {
        self.rx.try_recv().ok()
    }

    /// Waits for the next event. Returns `None` once every sink is gone.
    pub async fn recv(&self) -> Option<ClusterEvent> {
        self.rx.recv_async().await.ok()
    }

    /// Takes the accumulated drop count and resets it to zero, so the caller can
    /// emit exactly one stale marker per flush.
    pub fn take_dropped(&self) -> usize {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Events currently queued.
    pub fn len(&self) -> usize {
        self.rx.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

/// Creates the bounded cluster -> UI channel.
pub fn event_channel(capacity: usize) -> (EventSink, EventStream) {
    let (tx, rx) = flume::bounded(capacity);
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        EventSink {
            tx,
            dropped: Arc::clone(&dropped),
        },
        EventStream { rx, dropped },
    )
}

/// Why a command could not be queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// The cluster layer is saturated. Commands are not dropped silently.
    #[error("the cluster runtime is not keeping up; command was not queued")]
    Backpressure,
    /// The cluster runtime has shut down.
    #[error("the cluster runtime has shut down")]
    Closed,
}

/// UI end of the command channel.
#[derive(Clone, Debug)]
pub struct CommandSender {
    tx: flume::Sender<ClusterCommand>,
}

impl CommandSender {
    /// Queues a command. Never blocks the UI thread.
    pub fn send(&self, command: ClusterCommand) -> Result<(), CommandError> {
        match self.tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(flume::TrySendError::Full(_)) => Err(CommandError::Backpressure),
            Err(flume::TrySendError::Disconnected(_)) => Err(CommandError::Closed),
        }
    }

    /// Commands queued but not yet picked up.
    pub fn pending(&self) -> usize {
        self.tx.len()
    }
}

/// Cluster-layer end of the command channel.
#[derive(Clone, Debug)]
pub struct CommandReceiver {
    rx: flume::Receiver<ClusterCommand>,
}

impl CommandReceiver {
    /// Waits for the next command. Returns `None` once the UI end is dropped.
    pub async fn recv(&self) -> Option<ClusterCommand> {
        self.rx.recv_async().await.ok()
    }

    /// Takes the next command without waiting.
    pub fn try_recv(&self) -> Option<ClusterCommand> {
        self.rx.try_recv().ok()
    }
}

/// Creates the bounded UI -> cluster channel.
pub fn command_channel(capacity: usize) -> (CommandSender, CommandReceiver) {
    let (tx, rx) = flume::bounded(capacity);
    (CommandSender { tx }, CommandReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ConnectionState;

    fn status(cluster: &str) -> ClusterEvent {
        ClusterEvent::Status {
            cluster: cluster.into(),
            state: ConnectionState::Connected,
        }
    }

    #[test]
    fn events_drop_instead_of_blocking_when_full() {
        let (sink, stream) = event_channel(2);

        assert_eq!(sink.send(status("a")), SendOutcome::Delivered);
        assert_eq!(sink.send(status("b")), SendOutcome::Delivered);
        // Third send must return immediately rather than stall the cluster layer.
        assert_eq!(sink.send(status("c")), SendOutcome::Dropped);
        assert_eq!(sink.send(status("d")), SendOutcome::Dropped);

        assert_eq!(sink.dropped(), 2);
        assert_eq!(stream.len(), 2);
    }

    #[test]
    fn taking_the_drop_count_resets_it() {
        let (sink, stream) = event_channel(1);
        sink.send(status("a"));
        sink.send(status("b"));

        assert_eq!(stream.take_dropped(), 1);
        assert_eq!(stream.take_dropped(), 0);
    }

    #[test]
    fn a_closed_ui_is_reported_not_counted_as_a_drop() {
        let (sink, stream) = event_channel(4);
        drop(stream);

        let outcome = sink.send(status("a"));
        assert!(outcome.is_closed());
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn commands_report_backpressure_rather_than_vanishing() {
        let (tx, rx) = command_channel(1);
        let ping = ClusterCommand::Ping {
            cluster: "a".into(),
            nonce: 1,
        };

        assert!(tx.send(ping.clone()).is_ok());
        assert_eq!(tx.send(ping.clone()), Err(CommandError::Backpressure));
        assert_eq!(tx.pending(), 1);

        drop(rx);
        assert_eq!(tx.send(ping), Err(CommandError::Closed));
    }

    #[test]
    fn draining_events_preserves_order() {
        let (sink, stream) = event_channel(8);
        sink.send(status("a"));
        sink.send(status("b"));

        let drained: Vec<_> = std::iter::from_fn(|| stream.try_recv()).collect();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].cluster().unwrap().as_str(), "a");
        assert_eq!(drained[1].cluster().unwrap().as_str(), "b");
        assert!(stream.is_empty());
    }
}
