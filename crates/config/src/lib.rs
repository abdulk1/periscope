//! Application configuration: paths, settings and logging.
//!
//! This crate is deliberately free of both GPUI and kube so it can be used from
//! anywhere, including tests and future headless tooling.

pub mod logging;
pub mod paths;
pub mod settings;

pub use logging::{LogGuard, Verbosity};
pub use settings::{Settings, ThemeChoice};
