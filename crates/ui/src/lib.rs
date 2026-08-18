//! GPUI views.
//!
//! Everything here runs on the main thread. Views never talk to Kubernetes
//! directly: they send [`periscope_bridge::ClusterCommand`]s and render what the
//! store says, so a slow or broken cluster can never stall a frame.

pub mod format;
pub mod palette;
pub mod perf;
pub mod table;
pub mod theme;
pub mod workspace;

pub use workspace::{BridgeStats, Workspace, init};
