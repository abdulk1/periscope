//! The tokio <-> GPUI bridge.
//!
//! GPUI owns the main thread and runs its own executors; `kube-rs` needs a
//! tokio runtime. This crate keeps them apart and connects them with bounded
//! channels and a coalescing pump:
//!
//! ```text
//!   UI (main thread)                       cluster layer (tokio thread)
//!   ────────────────                       ───────────────────────────
//!   CommandSender  ──── bounded ─────────► CommandReceiver ─► CommandHandler
//!   spawn_event_pump ◄── bounded ───────── EventSink
//!        │  coalesce, flush every 16ms
//!        ▼
//!   one batched mutation per frame
//! ```
//!
//! Design rules this crate enforces:
//!
//! * Both channels are bounded. Events overflow by dropping and marking data
//!   stale; commands overflow by reporting backpressure to the caller. Neither
//!   direction ever blocks the other layer's thread.
//! * The UI is mutated once per flush, never once per watch event.
//! * Nothing here knows about Kubernetes, so all of it is testable without a
//!   cluster.

pub mod channel;
pub mod coalesce;
pub mod link;
pub mod protocol;
pub mod runtime;

pub use channel::{
    CommandError, CommandReceiver, CommandSender, EventSink, EventStream, SendOutcome,
    command_channel, event_channel,
};
pub use coalesce::Coalescer;
pub use link::{
    DEFAULT_FLUSH_INTERVAL, DEFAULT_MAX_BATCH, FlushStats, PumpConfig, spawn_event_pump,
};
pub use protocol::{ClusterCommand, ClusterEvent, ClusterId, ConnectionState, EventKey};
pub use runtime::{ClusterRuntime, CommandHandler, RuntimeConfig, RuntimeError};
