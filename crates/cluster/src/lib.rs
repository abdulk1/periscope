//! The cluster layer: everything that talks to Kubernetes.
//!
//! This crate runs entirely on the tokio runtime owned by
//! [`periscope_bridge::ClusterRuntime`] and never touches GPUI. It communicates
//! only by receiving [`periscope_bridge::ClusterCommand`]s and emitting
//! [`periscope_bridge::ClusterEvent`]s.
//!
//! # Read-only invariant
//!
//! Until Phase 5 no code path in this crate may mutate cluster state. Every
//! request it makes is a get, list or watch. New commands must be reads.

pub mod columns;
pub mod detail;
pub mod discovery;
pub mod errors;
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
