//! Command-line flags.

use clap::Parser;
use periscope_config::Verbosity;

/// Periscope — a native, GPU-accelerated Kubernetes console.
#[derive(Debug, Parser)]
#[command(name = "scope", version, about, long_about = None)]
pub struct Cli {
    /// Read this kubeconfig file instead of $KUBECONFIG or ~/.kube/config.
    #[arg(long, value_name = "PATH")]
    pub kubeconfig: Option<std::path::PathBuf>,

    /// Log everything, and mirror the log to stderr.
    #[arg(short, long)]
    pub verbose: bool,

    /// Start in the log view, tailing the pods matching this label selector.
    ///
    /// Needs --namespace, because log streams are per-namespace.
    #[arg(long, value_name = "SELECTOR", requires = "namespace")]
    pub tail: Option<String>,

    /// Namespace for --tail.
    #[arg(long, short = 'n', value_name = "NAMESPACE")]
    pub namespace: Option<String>,

    /// Log warnings and errors only.
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Log frame times and watch-event throughput.
    #[arg(long)]
    pub perf: bool,
}

impl Cli {
    /// The log level implied by the flags.
    pub fn verbosity(&self) -> Verbosity {
        match (self.verbose, self.quiet) {
            (true, _) => Verbosity::Verbose,
            (_, true) => Verbosity::Quiet,
            _ => Verbosity::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn flags_are_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn verbosity_follows_the_flags() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().verbosity();
        assert_eq!(parse(&["scope"]), Verbosity::Normal);
        assert_eq!(parse(&["scope", "--verbose"]), Verbosity::Verbose);
        assert_eq!(parse(&["scope", "--quiet"]), Verbosity::Quiet);
    }

    #[test]
    fn verbose_and_quiet_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["scope", "--verbose", "--quiet"]).is_err());
    }

    #[test]
    fn tailing_at_startup_needs_a_namespace() {
        // Without one there is no URL to open: logs are a pod subresource.
        assert!(Cli::try_parse_from(["scope", "--tail", "app=web"]).is_err());

        let cli = Cli::try_parse_from(["scope", "--tail", "app=web", "-n", "prod"]).unwrap();
        assert_eq!(cli.tail.as_deref(), Some("app=web"));
        assert_eq!(cli.namespace.as_deref(), Some("prod"));
    }

    #[test]
    fn kubeconfig_defaults_to_the_standard_search() {
        assert_eq!(Cli::try_parse_from(["scope"]).unwrap().kubeconfig, None);
        assert_eq!(
            Cli::try_parse_from(["scope", "--kubeconfig", "/tmp/kc"])
                .unwrap()
                .kubeconfig
                .as_deref(),
            Some(std::path::Path::new("/tmp/kc"))
        );
    }

    #[test]
    fn perf_is_off_by_default() {
        assert!(!Cli::try_parse_from(["scope"]).unwrap().perf);
        assert!(Cli::try_parse_from(["scope", "--perf"]).unwrap().perf);
    }
}
