//! Application state.
//!
//! The store owns everything the UI renders and is the only place that decides
//! what is true. It has no GPUI dependency, so all of it is testable without a
//! window and without a cluster.
//!
//! Phase 0 tracks connection health only; resource caches and indexes arrive in
//! Phase 1.

pub mod connections;

pub use connections::{Connection, ConnectionRegistry};
