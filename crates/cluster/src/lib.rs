//! The cluster layer: everything that talks to Kubernetes.
//!
//! This crate runs entirely on the tokio runtime owned by
//! [`periscope_bridge::ClusterRuntime`] and never touches GPUI. It communicates
//! only by receiving [`periscope_bridge::ClusterCommand`]s and emitting
//! [`periscope_bridge::ClusterEvent`]s.
//!
//! # Where writes are allowed
//!
//! Reads — get, list, watch, logs — go straight through. Everything that can
//! change a cluster is confined to [`mutate`] and [`exec`], and both check the
//! [`WritePolicy`] again before sending a request, even though the store
//! already refused: see `docs/DECISIONS.md` ADR-0028. A new command that is not
//! a read belongs in one of those two modules, behind that check.

pub mod columns;
pub mod detail;
pub mod discovery;
pub mod errors;
pub mod exec;
pub mod forward;
pub mod handler;
pub mod kubeconfig;
pub mod logs;
pub mod mutate;
pub mod pods;
pub mod watch;
pub mod yaml;

pub use errors::Failure;
pub use handler::KubeHandler;
pub use kubeconfig::Contexts;
pub use mutate::WritePolicy;
