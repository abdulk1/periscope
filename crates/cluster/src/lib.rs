//! The cluster layer: everything that talks to Kubernetes.
//!
//! This crate runs entirely on the tokio runtime owned by
//! [`periscope_bridge::ClusterRuntime`] and never touches GPUI. It communicates
//! only by receiving [`periscope_bridge::ClusterCommand`]s and emitting
//! [`periscope_bridge::ClusterEvent`]s.
//!
//! Phase 0 ships the health handler only. kube clients, kubeconfig parsing,
//! watchers and log streams land in Phase 1.
//!
//! # Read-only invariant
//!
//! Until Phase 5 no code path in this crate may mutate cluster state. New
//! commands must be reads.

pub mod health;

pub use health::HealthHandler;
