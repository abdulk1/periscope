//! Application configuration: paths, settings and logging.
//!
//! This crate is deliberately free of both GPUI and kube so it can be used from
//! anywhere, including tests and future headless tooling.

pub mod audit;
pub mod logging;
pub mod paths;
pub mod settings;
pub mod state;
pub mod updates;

pub use audit::{AuditLog, Entry as AuditEntry, Outcome as AuditOutcome};
pub use logging::{LogGuard, Verbosity};
pub use settings::{
    Access, Columns, Command, Keys, Limits, Settings, SettingsError, Span, ThemeChoice,
};
pub use state::{StateFile, UiState, Visit};
pub use updates::{Release, Updates, Version};
