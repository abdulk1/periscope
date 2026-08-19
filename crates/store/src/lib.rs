//! Application state.
//!
//! The store owns everything the UI renders and is the only place that decides
//! what is true. It has no GPUI dependency, so all of it is testable without a
//! window and without a cluster.

pub mod app;
pub mod catalog;
pub mod columns;
pub mod connections;
pub mod logs;
pub mod permissions;
pub mod table;

pub use app::{AppState, Detail, ExecSession, Filters};
pub use catalog::{Category, recent, sections};
pub use columns::Layout;
pub use connections::{Connection, ConnectionRegistry};
pub use logs::{FilterSpec, LogBuffer, LogOrder, TimeError, parse_time};
pub use permissions::{Authorized, AuthorizedExec, Permissions, Refusal};
pub use table::ResourceTable;
