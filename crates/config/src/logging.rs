//! Tracing setup: a rotating file log, plus stderr when asked.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::paths;

/// How much to log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verbosity {
    /// Warnings and errors only.
    Quiet,
    /// Ordinary operational logging.
    #[default]
    Normal,
    /// Everything Periscope emits, plus warnings from dependencies.
    Verbose,
}

impl Verbosity {
    /// The default filter directive for this level.
    fn directive(self) -> &'static str {
        match self {
            // Crate-scoped so a chatty dependency cannot drown the log.
            Self::Quiet => "warn",
            Self::Normal => "warn,periscope=info,scope=info",
            Self::Verbose => "warn,periscope=trace,scope=trace",
        }
    }
}

/// Keeps the log writer alive. Dropping it flushes and stops the writer thread,
/// so this must be held for as long as the process logs.
#[derive(Debug)]
pub struct LogGuard {
    _worker: WorkerGuard,
    path: PathBuf,
}

impl LogGuard {
    /// Directory the log files are being written to.
    pub fn directory(&self) -> &PathBuf {
        &self.path
    }
}

/// Installs the global tracing subscriber.
///
/// Logs go to a daily-rotating file. `stderr` additionally mirrors them to the
/// terminal, which is what `--verbose` turns on for people running from a shell.
///
/// `RUST_LOG` overrides the level when set.
pub fn init(verbosity: Verbosity, stderr: bool) -> Result<LogGuard> {
    let dir = paths::log_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create log directory {}", dir.display()))?;

    let appender = tracing_appender::rolling::daily(&dir, "periscope.log");
    let (writer, worker) = tracing_appender::non_blocking(appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(verbosity.directive()));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true);

    let stderr_layer = stderr.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("a tracing subscriber was already installed")?;

    Ok(LogGuard {
        _worker: worker,
        path: dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_directives_are_crate_scoped() {
        assert_eq!(Verbosity::default(), Verbosity::Normal);
        assert!(Verbosity::Normal.directive().contains("periscope=info"));
        assert!(Verbosity::Verbose.directive().contains("periscope=trace"));
        // A quiet run must not surface dependency chatter.
        assert_eq!(Verbosity::Quiet.directive(), "warn");
    }

    #[test]
    fn every_directive_parses_as_a_filter() {
        for verbosity in [Verbosity::Quiet, Verbosity::Normal, Verbosity::Verbose] {
            EnvFilter::try_new(verbosity.directive())
                .unwrap_or_else(|e| panic!("{verbosity:?} directive is invalid: {e}"));
        }
    }
}
