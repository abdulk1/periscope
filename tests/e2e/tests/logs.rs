//! Log tailing against a real cluster.
//!
//! These need the `chatty` fixture — a three-replica Deployment whose pods each
//! print a line every 200ms:
//!
//! ```text
//! kubectl apply -f tests/e2e/fixtures/chatty.yaml
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{ClusterCommand, ClusterEvent, LogSourceState, LogTarget};
use periscope_e2e::{connected, describe, wait_for};
use periscope_store::{FilterSpec, LogBuffer};

/// Attaching runs a request per pod; a busy CI box needs room.
///
/// Ninety seconds rather than thirty because of one test in this file. The
/// firehose fixture exists to saturate the machine — that is the whole point of
/// measuring ingest against it — and on a two-core runner it starves the
/// apiserver it shares a node with. Discovery timed out at thirty seconds while
/// the connection sat in `Connecting`, which says nothing about the code and
/// everything about four busybox pods writing as fast as they can.
///
/// This bounds *waiting for a cluster*, never the measurement: the ingest
/// budget times its own ten-second window and is untouched by this.
const TIMEOUT: Duration = Duration::from_secs(90);

/// The label the fixture's pods carry.
const FIXTURE: &str = "app=chatty";

/// The unthrottled fixture's label.
const FIREHOSE: &str = "app=firehose";

/// Connects, waits for discovery, and starts a log session.
fn tailing(
    target: LogTarget,
) -> (
    periscope_bridge::ClusterRuntime,
    periscope_bridge::EventStream,
) {
    let (runtime, stream, cluster) = connected();
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    runtime
        .send(ClusterCommand::StartLogs {
            cluster,
            target: Arc::new(target),
        })
        .expect("the command is queued");
    (runtime, stream)
}

/// A target that skips history, so a test sees live output rather than a
/// thousand lines of backlog from whichever pod answered first.
fn live(target: LogTarget) -> LogTarget {
    LogTarget {
        tail_lines: Some(2),
        ..target
    }
}

/// Collects log batches until `enough` lines have arrived.
fn collect(
    stream: &periscope_bridge::EventStream,
    timeout: Duration,
    enough: usize,
) -> (LogBuffer, Vec<ClusterEvent>) {
    let mut buffer = LogBuffer::default();
    let mut seen = Vec::new();
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline && buffer.len() < enough {
        match stream.try_recv() {
            Some(ClusterEvent::LogBatch { lines, .. }) => buffer.extend(&lines),
            Some(ClusterEvent::LogSourceChanged { source, state, .. }) => {
                buffer.source_changed(source, state)
            }
            Some(ClusterEvent::LogsFailed { reason, .. }) => buffer.fail(reason),
            Some(other) => seen.push(other),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    (buffer, seen)
}

#[test]
#[ignore = "needs the chatty fixture"]
fn tailing_one_pod_streams_its_output() {
    let Some(pod) = periscope_e2e::fixture("chatty", FIXTURE) else {
        return;
    };
    let (_runtime, stream) = tailing(LogTarget::pod("default", &pod));

    let (buffer, seen) = collect(&stream, TIMEOUT, 5);
    assert!(
        buffer.error().is_none(),
        "the session failed: {:?} (saw {})",
        buffer.error(),
        describe(&seen)
    );
    assert!(buffer.len() >= 5, "only {} lines arrived", buffer.len());

    // Every line carries its source and the timestamp the apiserver stamped.
    let line = buffer.visible().next().unwrap();
    assert_eq!(&*line.source.pod, pod);
    assert_eq!(&*line.source.container, "talker");
    assert!(line.timestamp.is_some(), "lines should carry timestamps");
    assert!(
        line.text.contains("line "),
        "unexpected text: {}",
        line.text
    );
    // The RFC3339 prefix belongs in the timestamp, not in the text.
    assert!(!line.text.starts_with("20"), "{}", line.text);

    assert_eq!(buffer.streaming(), 1);
}

#[test]
#[ignore = "needs the chatty fixture"]
fn a_label_selector_merges_every_matching_pod() {
    if periscope_e2e::fixture("chatty", FIXTURE).is_none() {
        return;
    }

    let (_runtime, stream) = tailing(live(LogTarget::labels("default", FIXTURE)));

    // Enough lines that all three replicas have had time to attach and speak:
    // each prints five a second.
    let (buffer, seen) = collect(&stream, TIMEOUT, 60);
    assert!(
        buffer.error().is_none(),
        "the session failed: {:?} (saw {})",
        buffer.error(),
        describe(&seen)
    );

    let pods: std::collections::BTreeSet<_> = buffer
        .visible()
        .map(|line| line.source.pod.to_string())
        .collect();
    assert!(
        pods.len() >= 3,
        "expected lines from all three replicas, saw {pods:?} in {} lines",
        buffer.len()
    );
    assert!(
        buffer.streaming() >= 3,
        "expected three attached sources, saw {}",
        buffer.streaming()
    );

    // Merged, not concatenated: the stream interleaves the pods rather than
    // showing one pod's history and then the next.
    let sources: Vec<_> = buffer
        .visible()
        .map(|line| line.source.pod.to_string())
        .collect();
    let switches = sources.windows(2).filter(|pair| pair[0] != pair[1]).count();
    assert!(
        switches >= 2,
        "the stream never alternated pods: {switches}"
    );
}

#[test]
#[ignore = "needs the chatty fixture"]
fn filtering_narrows_the_buffer_without_dropping_it() {
    if periscope_e2e::fixture("chatty", FIXTURE).is_none() {
        return;
    }

    let (_runtime, stream) = tailing(live(LogTarget::labels("default", FIXTURE)));
    let (mut buffer, _) = collect(&stream, TIMEOUT, 40);

    let held = buffer.len();
    buffer.set_filter(FilterSpec {
        pattern: r"line \d*[05] ".to_owned(),
        regex: true,
        ..FilterSpec::default()
    });

    assert!(buffer.visible_len() < held, "the filter matched everything");
    assert!(buffer.visible_len() > 0, "the filter matched nothing");
    assert_eq!(buffer.len(), held, "filtering must not discard lines");

    buffer.set_filter(FilterSpec::default());
    assert_eq!(buffer.visible_len(), held);
}

#[test]
#[ignore = "needs the chatty fixture"]
fn a_pod_that_restarts_is_re_attached() {
    if periscope_e2e::fixture("chatty", FIXTURE).is_none() {
        return;
    }

    let (_runtime, stream) = tailing(live(LogTarget::labels("default", FIXTURE)));
    let (before, _) = collect(&stream, TIMEOUT, 20);
    let victim = before
        .visible()
        .next()
        .expect("some line arrived")
        .source
        .pod
        .to_string();

    let killed = Instant::now();
    periscope_e2e::delete_pod("default", &victim).expect("the pod is deleted");

    // The replacement pod is a different name; the session has to notice it and
    // attach without anyone asking.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut buffer = LogBuffer::default();
    let mut attached_new = false;

    while Instant::now() < deadline && !attached_new {
        match stream.try_recv() {
            Some(ClusterEvent::LogBatch { lines, .. }) => {
                buffer.extend(&lines);
                attached_new = buffer
                    .visible()
                    .any(|line| &*line.source.pod != victim.as_str());
            }
            Some(ClusterEvent::LogSourceChanged { source, state, .. }) => {
                if state == LogSourceState::Streaming && &*source.pod != victim.as_str() {
                    // A source that is not the one we killed is either a
                    // surviving replica or the replacement; either way the
                    // session kept working.
                }
                buffer.source_changed(source, state);
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let latency = killed.elapsed();
    println!("delete -> lines from another pod: {latency:?} (pod scheduling dominates this)");
    assert!(
        attached_new,
        "nothing arrived from any pod after the restart"
    );
    let pods: std::collections::BTreeSet<_> = buffer
        .visible()
        .map(|line| line.source.pod.to_string())
        .collect();
    assert!(
        pods.iter().any(|pod| pod != &victim),
        "only the deleted pod's lines arrived: {pods:?}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_container_that_does_not_exist_is_reported_rather_than_silently_empty() {
    let Some(pod) = periscope_e2e::fixture("chatty", FIXTURE) else {
        return;
    };
    let (_runtime, stream) =
        tailing(LogTarget::pod("default", &pod).container(Some(Arc::from("no-such-container"))));

    let (event, seen) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::LogsFailed { .. })
    })
    .unwrap_or_else(|seen| panic!("no failure reported; saw: {}", describe(&seen)));
    let _ = seen;

    let ClusterEvent::LogsFailed { reason, .. } = event else {
        unreachable!()
    };
    assert!(
        reason.contains("no-such-container"),
        "the container should be named: {reason}"
    );
}

/// The Phase 3 budget: 10,000 lines a second, ingested without the buffer
/// growing without bound. Needs the `firehose` fixture, whose pods write as
/// fast as the runtime will take it.
#[test]
#[ignore = "needs the firehose fixture"]
fn a_firehose_is_ingested_at_rate_and_stays_bounded() {
    if periscope_e2e::fixture("firehose", FIREHOSE).is_none() {
        return;
    }

    let (_runtime, stream) = tailing(live(LogTarget::labels("default", FIREHOSE)));

    // A ring far smaller than what will arrive, so the cap is what holds memory
    // down rather than the stream running out.
    let mut buffer = LogBuffer::new(50_000);
    let mut ingested = 0usize;

    // Let the readers attach before timing anything.
    let warmup = Instant::now() + Duration::from_secs(5);
    while Instant::now() < warmup {
        if let Some(ClusterEvent::LogBatch { lines, .. }) = stream.try_recv() {
            buffer.extend(&lines);
        }
    }

    let started = Instant::now();
    let window = Duration::from_secs(10);
    while started.elapsed() < window {
        match stream.try_recv() {
            Some(ClusterEvent::LogBatch { lines, .. }) => {
                ingested += lines.len();
                buffer.extend(&lines);
            }
            Some(ClusterEvent::LogSourceChanged { source, state, .. }) => {
                buffer.source_changed(source, state)
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_micros(200)),
        }
    }

    let rate = ingested as f64 / started.elapsed().as_secs_f64();
    println!(
        "ingested {ingested} lines in {:?} ({rate:.0} lines/s), buffer {} of {} capacity, {} dropped",
        started.elapsed(),
        buffer.len(),
        50_000,
        buffer.dropped()
    );

    assert!(
        rate >= 10_000.0,
        "ingested only {rate:.0} lines/s; the budget is 10,000"
    );
    // Bounded is the point: the ring must hold at its cap however much arrives.
    assert!(buffer.len() <= 50_000, "the ring grew past its capacity");
    assert!(buffer.dropped() > 0, "the ring never had to evict");
}

#[test]
#[ignore = "needs the chatty fixture"]
fn a_high_rate_stream_is_bounded_by_the_ring() {
    if periscope_e2e::fixture("chatty", FIXTURE).is_none() {
        return;
    }

    let (_runtime, stream) = tailing(live(LogTarget::labels("default", FIXTURE)));

    // A ring far smaller than what will arrive, so eviction is guaranteed.
    let mut buffer = LogBuffer::new(20);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && buffer.dropped() < 20 {
        match stream.try_recv() {
            Some(ClusterEvent::LogBatch { lines, .. }) => buffer.extend(&lines),
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    assert!(buffer.dropped() >= 20, "the ring never had to evict");
    assert_eq!(buffer.len(), 20, "the ring grew past its capacity");
}
