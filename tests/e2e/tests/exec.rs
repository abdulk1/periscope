//! Running commands in a container, against a real cluster.
//!
//! Needs the `webby` fixture, whose container is busybox and therefore has the
//! handful of commands these tests run, and — for the container selector —
//! `sidecars`, whose pod has two containers and an init container:
//!
//! ```text
//! kubectl apply -f tests/e2e/fixtures/webby.yaml
//! kubectl apply -f tests/e2e/fixtures/sidecars.yaml
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, EventStream, ExecStatus, ExecTarget,
    LogLine, RuntimeConfig,
};
use periscope_cluster::{KubeHandler, WritePolicy};
use periscope_config::{AuditLog, AuditOutcome};
use periscope_e2e::{context, describe, wait_for};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The label the fixture's pod carries.
const FIXTURE: &str = "app=webby";

/// The label the multi-container fixture's pod carries.
const SIDECARS: &str = "app=sidecars";

/// A connected runtime, with a policy and an audit log of the test's choosing.
fn connected_with(
    policy: WritePolicy,
    audit: Option<AuditLog>,
) -> (ClusterRuntime, EventStream, ClusterId) {
    let mut handler = KubeHandler::new().with_policy(policy);
    if let Some(audit) = audit {
        handler = handler.with_audit(audit);
    }

    let (runtime, stream) =
        ClusterRuntime::start(handler, RuntimeConfig::default()).expect("the runtime starts");
    let cluster = context();

    runtime
        .send(ClusterCommand::Connect {
            cluster: cluster.clone(),
        })
        .expect("connect is queued");
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    (runtime, stream, cluster)
}

fn connected() -> (ClusterRuntime, EventStream, ClusterId) {
    connected_with(WritePolicy::permissive(), None)
}

/// Runs a command and collects everything it said, plus how it ended.
fn run(
    runtime: &ClusterRuntime,
    stream: &EventStream,
    cluster: &ClusterId,
    pod: &str,
    command: &str,
) -> (Vec<LogLine>, ExecStatus) {
    let target = ExecTarget::parse("default", pod, None, command).expect("a command to run");
    runtime
        .send(ClusterCommand::Exec {
            cluster: cluster.clone(),
            target: Arc::new(target),
        })
        .expect("the command is queued");

    collect(stream)
}

/// Runs a command in one named container of a pod.
fn run_in(
    runtime: &ClusterRuntime,
    stream: &EventStream,
    cluster: &ClusterId,
    pod: &str,
    container: &str,
    command: &str,
) -> (Vec<LogLine>, ExecStatus) {
    let target = ExecTarget::parse("default", pod, Some(Arc::from(container)), command)
        .expect("a command to run");
    runtime
        .send(ClusterCommand::Exec {
            cluster: cluster.clone(),
            target: Arc::new(target),
        })
        .expect("the command is queued");

    collect(stream)
}

/// Drains until the command ends, keeping its output.
fn collect(stream: &EventStream) -> (Vec<LogLine>, ExecStatus) {
    let deadline = Instant::now() + TIMEOUT;
    let mut lines = Vec::new();

    while Instant::now() < deadline {
        let Some(event) = stream.try_recv() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };

        match event {
            ClusterEvent::ExecOutput { lines: batch, .. } => lines.extend(batch.iter().cloned()),
            ClusterEvent::ExecFinished { status, .. } => return (lines, status),
            _ => {}
        }
    }

    panic!(
        "the command never finished; collected {} lines",
        lines.len()
    );
}

/// The text of every line, joined.
fn text(lines: &[LogLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines one stream produced.
fn from_stream<'a>(lines: &'a [LogLine], stream: &str) -> Vec<&'a LogLine> {
    lines
        .iter()
        .filter(|line| &*line.source.container == stream)
        .collect()
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_command_runs_and_its_output_comes_back() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let (lines, status) = run(&runtime, &stream, &cluster, &pod, "cat /www/index.html");

    assert!(
        text(&lines).contains("periscope-forward-ok"),
        "the file's contents should come back: {:?}",
        text(&lines)
    );
    assert_eq!(
        status,
        ExecStatus::Exited {
            code: Some(0),
            message: String::new(),
        },
        "a command that worked must say so"
    );
    assert!(!status.is_problem());
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_failing_command_reports_its_exit_code_rather_than_looking_successful() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let (lines, status) = run(&runtime, &stream, &cluster, &pod, "cat /no/such/file");

    match &status {
        ExecStatus::Exited { code, .. } => {
            assert_eq!(*code, Some(1), "busybox cat exits 1 on a missing file")
        }
        other => panic!("expected an exit code, got {other:?}"),
    }
    assert!(status.is_problem(), "a non-zero exit needs attention");
    // The reason the command failed came from the command, not from Periscope.
    assert!(
        text(&lines).contains("No such file"),
        "stderr should carry the reason: {:?}",
        text(&lines)
    );
}

#[test]
#[ignore = "needs the webby fixture"]
fn stderr_is_labelled_separately_from_stdout() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    // One command, both streams: `ls` prints the file it found and complains
    // about the one it did not.
    let (lines, _) = run(
        &runtime,
        &stream,
        &cluster,
        &pod,
        "ls /www/index.html /no/such/file",
    );

    let out = from_stream(&lines, "stdout");
    let err = from_stream(&lines, "stderr");

    assert!(!out.is_empty(), "expected stdout: {:?}", text(&lines));
    assert!(!err.is_empty(), "expected stderr: {:?}", text(&lines));
    assert!(
        out.iter().any(|line| line.text.contains("index.html")),
        "{:?}",
        text(&lines)
    );
    assert!(
        err.iter().any(|line| line.text.contains("No such file")),
        "{:?}",
        text(&lines)
    );
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_command_that_does_not_exist_says_so_instead_of_hanging() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let (_, status) = run(
        &runtime,
        &stream,
        &cluster,
        &pod,
        "periscope-no-such-command",
    );

    assert!(status.is_problem(), "{status:?}");
    let message = status.message();
    assert!(
        message.contains("not found") || message.contains("no such file"),
        "the apiserver's reason should survive: {message}"
    );
}

#[test]
#[ignore = "needs the webby fixture"]
fn output_arrives_before_the_status_that_follows_it() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    // Ten lines, then an exit: none of them may be lost to the race between the
    // last batch and the status.
    let (lines, status) = run(&runtime, &stream, &cluster, &pod, "seq 1 10");

    let out = from_stream(&lines, "stdout");
    assert_eq!(out.len(), 10, "collected: {:?}", text(&lines));
    assert_eq!(&*out[9].text, "10");
    assert!(!status.is_problem());
}

#[test]
#[ignore = "needs the webby fixture"]
fn cancelling_a_command_stops_it() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let target = ExecTarget::parse("default", &pod, None, "sleep 600").expect("a command");
    runtime
        .send(ClusterCommand::Exec {
            cluster: cluster.clone(),
            target: Arc::new(target),
        })
        .expect("the command is queued");

    // Give the exec time to actually attach before cancelling it.
    std::thread::sleep(Duration::from_secs(2));
    runtime
        .send(ClusterCommand::CancelExec {
            cluster: cluster.clone(),
        })
        .expect("the cancel is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::ExecFinished { .. })
    })
    .unwrap_or_else(|seen| panic!("no ending was reported; saw: {}", describe(&seen)));

    let ClusterEvent::ExecFinished { status, .. } = event else {
        unreachable!()
    };
    assert_eq!(status, ExecStatus::Cancelled);
    // Cancelling is a deliberate act, not a failure.
    assert!(!status.is_problem());
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_read_only_cluster_runs_nothing_and_records_the_refusal() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let scratch = periscope_e2e::exec::Scratch::new("audit-exec-readonly");
    let log = AuditLog::at(scratch.path().join("audit.log"));

    let mut policy = WritePolicy::permissive();
    policy.deny(context());
    let (runtime, stream, cluster) = connected_with(policy, Some(log.clone()));

    // A command that would leave a trace if it ran.
    let (lines, status) = run(
        &runtime,
        &stream,
        &cluster,
        &pod,
        "touch /www/periscope-should-not-exist",
    );

    assert!(
        lines.is_empty(),
        "nothing should have run: {:?}",
        text(&lines)
    );
    match &status {
        ExecStatus::Failed { reason } => assert!(reason.contains("read-only"), "{reason}"),
        other => panic!("a read-only cluster must refuse, got {other:?}"),
    }

    // And the file is not there, because the request never went out.
    let (_, check) = run(
        &runtime,
        &stream,
        &cluster,
        &pod,
        "ls /www/periscope-should-not-exist",
    );
    assert!(
        matches!(check, ExecStatus::Failed { .. }),
        "the check itself is refused too, on a read-only cluster: {check:?}"
    );

    let entries = log.read().expect("the audit log is readable");
    assert!(!entries.is_empty(), "a refusal must be recorded");
    assert_eq!(entries[0].action, "exec");
    assert_eq!(entries[0].outcome, AuditOutcome::Refused);
    assert!(entries[0].detail.contains("touch"), "{}", entries[0].detail);
    assert!(!entries[0].outcome.changed_anything());
}

#[test]
#[ignore = "needs the webby fixture"]
fn every_command_is_written_to_the_audit_log() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let scratch = periscope_e2e::exec::Scratch::new("audit-exec");
    let log = AuditLog::at(scratch.path().join("audit.log"));
    let (runtime, stream, cluster) = connected_with(WritePolicy::permissive(), Some(log.clone()));

    let (_, status) = run(&runtime, &stream, &cluster, &pod, "id -u");
    assert!(!status.is_problem(), "{status:?}");

    let entries = log.read().expect("the audit log is readable");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].action, "exec");
    assert_eq!(entries[0].kind, "pods");
    assert_eq!(entries[0].name, pod);
    assert_eq!(entries[0].namespace, "default");
    // The command itself is the point of the record.
    assert_eq!(entries[0].detail, "id -u");
    assert_eq!(entries[0].cluster, cluster.as_str());
}

#[test]
#[ignore = "needs the sidecars fixture"]
fn a_command_runs_in_the_container_it_was_given() {
    let Some(pod) = periscope_e2e::fixture("sidecars", SIDECARS) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    // Each container wrote its own name into its own filesystem, so the file
    // says which one the command actually ran in.
    let (alpha, status) = run_in(&runtime, &stream, &cluster, &pod, "alpha", "cat /identity");
    assert!(!status.is_problem(), "{status:?}");
    assert_eq!(text(&alpha).trim(), "alpha", "{:?}", text(&alpha));

    let (beta, status) = run_in(&runtime, &stream, &cluster, &pod, "beta", "cat /identity");
    assert!(!status.is_problem(), "{status:?}");
    assert_eq!(text(&beta).trim(), "beta", "{:?}", text(&beta));
}

#[test]
#[ignore = "needs the sidecars fixture"]
fn a_container_the_pod_does_not_have_is_refused_by_name() {
    let Some(pod) = periscope_e2e::fixture("sidecars", SIDECARS) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let (_, status) = run_in(&runtime, &stream, &cluster, &pod, "gamma", "cat /identity");

    assert!(status.is_problem(), "{status:?}");
    let message = status.message();
    assert!(
        message.contains("gamma"),
        "the apiserver's reason must name the container: {message}"
    );
}

#[test]
#[ignore = "needs the sidecars fixture"]
fn the_containers_a_selector_can_offer_are_in_the_object_the_detail_pane_fetches() {
    let Some(pod) = periscope_e2e::fixture("sidecars", SIDECARS) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    runtime
        .send(ClusterCommand::FetchObject {
            cluster,
            kind: periscope_bridge::KindId::new("", "v1", "Pod", "pods"),
            key: periscope_bridge::ResourceKey::new("default", &pod),
            reveal: false,
        })
        .expect("fetch is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Object { .. } | ClusterEvent::ObjectFailed { .. }
        )
    })
    .unwrap_or_else(|seen| panic!("no object came back; saw: {}", describe(&seen)));
    let ClusterEvent::Object { detail, .. } = event else {
        panic!("the fetch failed: {event:?}")
    };

    // This is what the UI does with it: the container selector reads the object
    // the pane already has rather than asking the apiserver a second time.
    let object = periscope_cluster::yaml::parse(&detail.yaml).expect("the pane's YAML parses");
    let containers = periscope_cluster::pods::containers(&object);
    let names: Vec<_> = containers
        .iter()
        .map(|container| &*container.name)
        .collect();

    assert_eq!(names, ["alpha", "beta", "migrate"], "{names:?}");
    assert!(containers[2].init, "migrate is an init container");
}

#[test]
#[ignore = "needs a cluster"]
fn running_a_command_in_a_pod_that_does_not_exist_names_it() {
    let (runtime, stream, cluster) = connected();

    let (_, status) = run(&runtime, &stream, &cluster, "periscope-no-such-pod", "ls");

    let message = status.message();
    assert!(status.is_problem(), "{status:?}");
    assert!(
        message.contains("periscope-no-such-pod"),
        "the reason must name the pod: {message}"
    );
}
