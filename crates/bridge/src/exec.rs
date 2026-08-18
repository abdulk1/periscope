//! Running a command in a container.
//!
//! This is **not** a terminal. There is no pseudo-terminal, no VT parsing, no
//! cursor addressing and no interactive input: a command is run, its output is
//! streamed, and its exit status is reported. `kubectl exec -it` this is not.
//!
//! The spec asked for terminal emulation; `docs/DECISIONS.md` ADR-0033 records
//! why this stops short of it, as a deviation rather than a reading.
//!
//! What it does cover is what people actually reach for during an incident:
//! `ls`, `cat /etc/config`, `env`, `ps`, `nslookup`. Output arrives as
//! [`crate::LogLine`]s so it lands in the same bounded, filterable buffer the
//! log view already has.

use std::sync::Arc;

/// What to run, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecTarget {
    /// Namespace the pod lives in.
    pub namespace: Arc<str>,
    /// Which pod.
    pub pod: Arc<str>,
    /// Which container, or `None` for the pod's default.
    pub container: Option<Arc<str>>,
    /// The command and its arguments, already split.
    pub command: Vec<Arc<str>>,
}

impl ExecTarget {
    /// Builds a target from a command line typed by the user.
    ///
    /// Splitting on whitespace is deliberately simple: this is not a shell, and
    /// pretending otherwise — quoting, globbing, pipes — would be a lie about
    /// what is being run. Anyone who needs a shell asks for one explicitly:
    /// `sh -c "ls | wc -l"` runs a shell in the container, which is honest.
    pub fn parse(
        namespace: impl AsRef<str>,
        pod: impl AsRef<str>,
        container: Option<Arc<str>>,
        command: &str,
    ) -> Option<Self> {
        let command: Vec<Arc<str>> = command.split_whitespace().map(Arc::<str>::from).collect();
        if command.is_empty() {
            return None;
        }

        Some(Self {
            namespace: Arc::from(namespace.as_ref()),
            pod: Arc::from(pod.as_ref()),
            container,
            command,
        })
    }

    /// The command as one string, for the header.
    pub fn command_line(&self) -> String {
        self.command
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The sentence a confirmation must show.
    ///
    /// Running a command is held to the same rule as a mutation, because it can
    /// do anything a mutation can and more: the sentence names the cluster, the
    /// namespace, the pod, the container and the exact command, so there is no
    /// version of "I did not realise which cluster that was".
    pub fn confirmation(&self, cluster: &crate::ClusterId) -> String {
        let container = self
            .container
            .as_deref()
            .map(|container| format!(" container {container} of"))
            .unwrap_or_default();

        format!(
            "Run `{}` in{container} pod {} in namespace {} on cluster {cluster}?",
            self.command_line(),
            self.pod,
            self.namespace
        )
    }

    /// How the session reads in the header.
    pub fn label(&self) -> String {
        let container = self
            .container
            .as_deref()
            .map(|container| format!(" [{container}]"))
            .unwrap_or_default();
        format!(
            "{}/{}{container} $ {}",
            self.namespace,
            self.pod,
            self.command_line()
        )
    }
}

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecStatus {
    /// It finished. A non-zero code is a result, not a failure of Periscope.
    Exited {
        /// The exit code, when the apiserver reported one.
        code: Option<i32>,
        /// What the apiserver said about it.
        message: String,
    },
    /// It could not be started, or the connection broke.
    Failed {
        /// Verbatim underlying reason.
        reason: String,
    },
    /// The user stopped it.
    Cancelled,
}

impl ExecStatus {
    /// Whether this needs the user's attention.
    pub fn is_problem(&self) -> bool {
        match self {
            Self::Exited { code, .. } => code.is_some_and(|code| code != 0),
            Self::Failed { .. } => true,
            Self::Cancelled => false,
        }
    }

    /// The line the UI shows when the command ends.
    pub fn message(&self) -> String {
        match self {
            Self::Exited { code: Some(0), .. } => "exited 0".to_owned(),
            Self::Exited {
                code: Some(code),
                message,
            } if message.is_empty() => format!("exited {code}"),
            Self::Exited {
                code: Some(code),
                message,
            } => format!("exited {code}: {message}"),
            Self::Exited {
                code: None,
                message,
            } if message.is_empty() => "finished".to_owned(),
            Self::Exited {
                code: None,
                message,
            } => message.clone(),
            Self::Failed { reason } => reason.clone(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_line_is_split_into_arguments() {
        let target = ExecTarget::parse("default", "api-0", None, "ls -la /etc").expect("parses");

        assert_eq!(
            target
                .command
                .iter()
                .map(|part| &**part)
                .collect::<Vec<_>>(),
            ["ls", "-la", "/etc"]
        );
        assert_eq!(target.command_line(), "ls -la /etc");
    }

    #[test]
    fn an_empty_command_is_not_a_target() {
        assert!(ExecTarget::parse("default", "api-0", None, "   ").is_none());
    }

    #[test]
    fn splitting_is_not_a_shell_and_does_not_pretend_to_be() {
        // A pipe is passed to the program as an argument, because there is no
        // shell here to interpret it. Anyone who wants one runs `sh -c`.
        let target = ExecTarget::parse("default", "api-0", None, "ls | wc -l").expect("parses");
        assert_eq!(target.command.len(), 4);
        assert_eq!(&*target.command[1], "|");
    }

    #[test]
    fn a_target_describes_itself_for_the_header() {
        let target = ExecTarget::parse("payments", "api-0", Some(Arc::from("sidecar")), "env")
            .expect("parses");

        assert_eq!(target.label(), "payments/api-0 [sidecar] $ env");
    }

    #[test]
    fn a_confirmation_names_the_cluster_the_pod_and_the_exact_command() {
        let sentence = ExecTarget::parse("payments", "api-0", None, "rm -rf /data")
            .expect("parses")
            .confirmation(&crate::ClusterId::new("prod"));

        assert!(sentence.contains("prod"), "{sentence}");
        assert!(sentence.contains("api-0"), "{sentence}");
        assert!(sentence.contains("payments"), "{sentence}");
        assert!(sentence.contains("rm -rf /data"), "{sentence}");
    }

    #[test]
    fn a_confirmation_names_the_container_when_one_was_chosen() {
        // Which container matters: the same command in a sidecar and in the app
        // are different acts.
        let sentence = ExecTarget::parse("payments", "api-0", Some(Arc::from("sidecar")), "env")
            .expect("parses")
            .confirmation(&crate::ClusterId::new("prod"));

        assert!(sentence.contains("sidecar"), "{sentence}");
    }

    #[test]
    fn a_non_zero_exit_is_a_result_worth_flagging() {
        let failed = ExecStatus::Exited {
            code: Some(1),
            message: String::new(),
        };
        assert!(failed.is_problem());
        assert_eq!(failed.message(), "exited 1");

        let fine = ExecStatus::Exited {
            code: Some(0),
            message: "command terminated with exit code 0".to_owned(),
        };
        assert!(!fine.is_problem());
        assert_eq!(fine.message(), "exited 0");
    }

    #[test]
    fn a_failure_to_start_is_a_problem_and_cancelling_is_not() {
        assert!(
            ExecStatus::Failed {
                reason: "container not found".to_owned()
            }
            .is_problem()
        );
        assert!(!ExecStatus::Cancelled.is_problem());
        assert_eq!(ExecStatus::Cancelled.message(), "cancelled");
    }
}
