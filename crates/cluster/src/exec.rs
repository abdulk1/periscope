//! Running a command in a container.
//!
//! stdout and stderr are read concurrently and batched the same way log lines
//! are, so a command that prints a lot does not flood the bridge. Each line is
//! tagged with the stream it came from, because "was that stdout or stderr" is
//! usually the next question.
//!
//! There is no pseudo-terminal here, and no interactive input: see
//! `docs/DECISIONS.md` ADR-0033 for where that line is drawn and why.
//!
//! Running a command in a container is a write, not a read — `kubectl exec`
//! needs `create` on `pods/exec`, and `rm -rf` is a perfectly ordinary command.
//! So this goes through the same two gates a mutation does: the cluster's
//! [`WritePolicy`], and the audit log.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::api::AttachParams;
use kube::{Api, Client};
use periscope_bridge::{
    ClusterEvent, ClusterId, EventSink, ExecStatus, ExecTarget, LogLine, LogSource,
};
use periscope_config::{AuditEntry, AuditLog, AuditOutcome};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::mpsc;

use crate::mutate::WritePolicy;

/// How long output accumulates before being handed to the UI.
const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Lines that force a flush before the interval is up.
const BATCH_LINES: usize = 256;

/// Depth of the merge channel between the two readers and the batcher.
const MERGE_CAPACITY: usize = 4_096;

/// Checks the policy, records the attempt, and runs the command.
pub async fn run(
    cluster: ClusterId,
    client: Client,
    target: Arc<ExecTarget>,
    policy: &WritePolicy,
    audit: Option<&AuditLog>,
    events: EventSink,
) {
    if !policy.may_mutate(&cluster) {
        // The store should already have refused. Reaching here means a caller
        // skipped that check, which is exactly what this gate is for.
        let reason = format!("{cluster} is read-only; commands are not run there");
        tracing::warn!(%cluster, target = %target.label(), "refused: cluster is read-only");
        record(&cluster, &target, AuditOutcome::Refused, &reason, audit);
        events.send(ClusterEvent::ExecFinished {
            cluster,
            status: ExecStatus::Failed { reason },
        });
        return;
    }

    // Recorded before the command runs, not after: a command that hangs, or one
    // Periscope is killed in the middle of, still has to leave a trace.
    record(&cluster, &target, AuditOutcome::Applied, "", audit);
    stream(cluster, client, target, events).await;
}

/// Writes one line of the audit log for an exec attempt.
fn record(
    cluster: &ClusterId,
    target: &ExecTarget,
    outcome: AuditOutcome,
    reason: &str,
    audit: Option<&AuditLog>,
) {
    let Some(audit) = audit else {
        return;
    };

    let entry = AuditEntry::new(
        cluster.as_str(),
        &*target.namespace,
        "pods",
        &*target.pod,
        "exec",
    )
    .detail(target.command_line())
    .outcome(outcome, reason);

    if let Err(error) = audit.append(&entry) {
        // Never fatal — but never silent either.
        tracing::error!(%cluster, %error, "could not write the audit log");
    }
}

/// Runs the command until it ends or the task is cancelled.
async fn stream(cluster: ClusterId, client: Client, target: Arc<ExecTarget>, events: EventSink) {
    let api: Api<Pod> = Api::namespaced(client, &target.namespace);
    let params = AttachParams {
        container: target.container.as_deref().map(str::to_owned),
        stdout: true,
        stderr: true,
        // No stdin and no TTY: this runs a command, it does not attach to one.
        stdin: false,
        tty: false,
        ..AttachParams::default()
    };

    let command: Vec<String> = target.command.iter().map(|part| part.to_string()).collect();
    let mut process = match api.exec(&target.pod, command, &params).await {
        Ok(process) => process,
        Err(error) => {
            let reason = format!("{}: {}", target.label(), crate::errors::describe(&error));
            tracing::warn!(%cluster, %reason, "exec could not start");
            events.send(ClusterEvent::ExecFinished {
                cluster,
                status: ExecStatus::Failed { reason },
            });
            return;
        }
    };

    // The status future has to be taken before the streams are drained: it is
    // the only place the exit code is reported.
    let status = process.take_status();
    let stdout = process.stdout();
    let stderr = process.stderr();

    let (sender, receiver) = mpsc::channel::<LogLine>(MERGE_CAPACITY);
    let pump = tokio::spawn(pump(cluster.clone(), receiver, events.clone()));

    let mut readers = Vec::new();
    if let Some(stdout) = stdout {
        readers.push(tokio::spawn(read_stream(
            stdout,
            LogSource::new(&*target.pod, "stdout"),
            sender.clone(),
        )));
    }
    if let Some(stderr) = stderr {
        readers.push(tokio::spawn(read_stream(
            stderr,
            LogSource::new(&*target.pod, "stderr"),
            sender.clone(),
        )));
    }
    drop(sender);

    for reader in readers {
        let _ = reader.await;
    }
    // The pump ends when the last sender is dropped. Waiting for it is what
    // guarantees the final lines are delivered before the status that follows
    // them: a command's last words must not arrive after "exited 1".
    let _ = pump.await;

    let status = match status {
        Some(status) => status.await,
        None => None,
    };

    let status = match status {
        Some(status) => classify(&status),
        // No status object came back: the connection ended without the
        // apiserver saying how the command finished.
        None => match process.join().await {
            Ok(()) => ExecStatus::Exited {
                code: None,
                message: String::new(),
            },
            Err(error) => ExecStatus::Failed {
                reason: format!("{}: {error}", target.label()),
            },
        },
    };

    tracing::info!(
        %cluster,
        target = %target.label(),
        status = status.message(),
        "exec finished"
    );
    events.send(ClusterEvent::ExecFinished { cluster, status });
}

/// Turns the apiserver's `Status` into how the command ended.
///
/// The shapes worth telling apart:
///
/// * **Success** — the command exited 0.
/// * **Failure with an `ExitCode` cause** — the command ran and exited non-zero.
///   The number is never a field of its own; it arrives as a cause, because the
///   apiserver describes a program's exit in the vocabulary of an API error.
/// * **Failure with no exit code** — the command never ran: no such executable,
///   no such container, the connection broke. This is *not* an exit, and
///   calling it one would show "finished" for something that plainly failed.
fn classify(status: &Status) -> ExecStatus {
    let message = status.message.clone().unwrap_or_default();

    if status.status.as_deref() == Some("Success") {
        return ExecStatus::Exited {
            code: Some(0),
            message,
        };
    }

    match exit_code(status) {
        Some(code) => ExecStatus::Exited {
            code: Some(code),
            message,
        },
        None if status.status.as_deref() == Some("Failure") => ExecStatus::Failed {
            reason: if message.is_empty() {
                status
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the command failed".to_owned())
            } else {
                message
            },
        },
        // Neither Success nor Failure: nothing useful was said, so nothing
        // useful is claimed.
        None => ExecStatus::Exited {
            code: None,
            message,
        },
    }
}

/// The exit code the apiserver buried in a cause, if it reported one.
fn exit_code(status: &Status) -> Option<i32> {
    status
        .details
        .as_ref()?
        .causes
        .iter()
        .flatten()
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        .and_then(|cause| cause.message.as_deref()?.parse().ok())
}

/// Reads one stream line by line into the merge channel.
async fn read_stream(
    stream: impl tokio::io::AsyncRead + Unpin,
    source: LogSource,
    lines: mpsc::Sender<LogLine>,
) {
    let mut reader = BufReader::new(stream).lines();

    // A read error ends this stream and nothing else: the other stream, and the
    // status, still have something to say.
    while let Ok(Some(text)) = reader.next_line().await {
        if lines
            .send(LogLine {
                source: source.clone(),
                // The apiserver does not timestamp exec output the way it
                // timestamps logs, so this is when Periscope read the line.
                timestamp: Some(SystemTime::now()),
                text: Arc::from(text.as_str()),
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Batches output the way the log pump does.
async fn pump(cluster: ClusterId, mut lines: mpsc::Receiver<LogLine>, events: EventSink) {
    let mut batch: Vec<LogLine> = Vec::with_capacity(BATCH_LINES);
    let mut ticker = tokio::time::interval(BATCH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(line) => {
                    batch.push(line);
                    if batch.len() >= BATCH_LINES && flush(&cluster, &mut batch, &events).is_err() {
                        return;
                    }
                }
                None => {
                    let _ = flush(&cluster, &mut batch, &events);
                    return;
                }
            },
            _ = ticker.tick() => {
                if flush(&cluster, &mut batch, &events).is_err() {
                    return;
                }
            }
        }
    }
}

/// Sends whatever has accumulated. `Err` means the UI is gone.
fn flush(cluster: &ClusterId, batch: &mut Vec<LogLine>, events: &EventSink) -> Result<(), ()> {
    if batch.is_empty() {
        return Ok(());
    }

    let lines: Arc<[LogLine]> = Arc::from(std::mem::take(batch));
    if events
        .send(ClusterEvent::ExecOutput {
            cluster: cluster.clone(),
            lines,
        })
        .is_closed()
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(value: serde_json::Value) -> Status {
        serde_json::from_value(value).expect("fixture is a valid status")
    }

    #[test]
    fn a_successful_command_reports_zero() {
        let status = classify(&status(serde_json::json!({ "status": "Success" })));

        assert_eq!(
            status,
            ExecStatus::Exited {
                code: Some(0),
                message: String::new(),
            }
        );
        assert!(!status.is_problem());
    }

    #[test]
    fn a_failing_command_reports_the_code_the_apiserver_buried() {
        // This is the shape the apiserver actually sends: a Failure whose exit
        // code is a cause, not a field.
        let status = classify(&status(serde_json::json!({
            "status": "Failure",
            "message": "command terminated with exit code 2",
            "reason": "NonZeroExitCode",
            "details": {
                "causes": [{ "reason": "ExitCode", "message": "2" }]
            }
        })));

        assert_eq!(
            status,
            ExecStatus::Exited {
                code: Some(2),
                message: "command terminated with exit code 2".to_owned(),
            }
        );
        assert!(status.is_problem());
    }

    #[test]
    fn a_command_that_never_started_is_a_failure_not_a_quiet_exit() {
        // The regression this test exists for: reporting this as `Exited` with
        // no code made a command that never ran look like one that finished,
        // and `is_problem` said no.
        let status = classify(&status(serde_json::json!({
            "status": "Failure",
            "message": "OCI runtime exec failed: exec: \"nope\": executable file not \
                        found in $PATH",
            "reason": "InternalError"
        })));

        assert!(status.is_problem(), "{status:?}");
        assert!(status.message().contains("executable file not found"));
    }

    #[test]
    fn a_failure_with_nothing_to_say_still_says_something() {
        let status = classify(&status(serde_json::json!({
            "status": "Failure",
            "reason": "InternalError"
        })));

        assert!(status.is_problem());
        assert_eq!(status.message(), "InternalError");
    }

    #[test]
    fn a_status_whose_cause_is_not_a_number_does_not_panic() {
        let status = classify(&status(serde_json::json!({
            "status": "Failure",
            "message": "killed",
            "details": { "causes": [{ "reason": "ExitCode", "message": "killed" }] }
        })));

        assert_eq!(
            status,
            ExecStatus::Failed {
                reason: "killed".to_owned()
            }
        );
    }

    #[test]
    fn a_status_that_says_neither_claims_nothing() {
        // Not a failure and not a success: the honest answer is "it finished",
        // with no invented exit code.
        let status = classify(&status(serde_json::json!({})));

        assert_eq!(
            status,
            ExecStatus::Exited {
                code: None,
                message: String::new(),
            }
        );
        assert_eq!(status.message(), "finished");
    }
}
