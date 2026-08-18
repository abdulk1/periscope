//! Application state.
//!
//! The store owns everything the UI renders and is the only place that decides
//! what is true. It has no GPUI dependency, so all of it is testable without a
//! window and without a cluster.

pub mod app;
pub mod connections;
pub mod table;

pub use app::{AppState, Detail, Filters};
pub use connections::{Connection, ConnectionRegistry};
pub use table::ResourceTable;
