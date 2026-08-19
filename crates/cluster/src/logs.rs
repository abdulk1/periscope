//! Tailing logs, from one pod or from fifty at once.
//!
//! The shape of a session:
//!
//! ```text
//!   pod watch  ──► attach a reader per matching pod
//!                        │  (one task each, restarted when a pod is replaced)
//!                        ▼
//!                  merge channel  ──► batcher ──► one LogBatch event per ~50ms
//! ```
//!
//! Batching is the load-bearing part. A busy pod emits thousands of lines a
//! second and the bridge's event channel is sized for watch events; one event
//! per line would fill it and start dropping text. Merging before batching also
//! means the UI never sorts: lines arrive in the order they were read, which is
//! the interleaving `kubectl logs -f` would have produced.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::{AsyncBufReadExt as _, StreamExt as _, TryStreamExt as _};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, LogParams};
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Client, ResourceExt as _};
use periscope_bridge::{
    ClusterEvent, ClusterId, EventSink, LogLine, LogSelector, LogSource, LogSourceState, LogTarget,
};
use tokio::sync::mpsc;

use crate::errors::{classify, describe};

/// How long lines accumulate before being handed to the UI.
///
/// Three times the UI's flush interval: the pump coalesces per frame anyway, so
/// batching harder here costs nothing visible and cuts event volume by an order
/// of magnitude on a busy stream.
const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Lines that force a flush before the interval is up, so a burst arrives
/// promptly rather than in one enormous batch.
const BATCH_LINES: usize = 512;

/// Depth of the merge channel between readers and the batcher.
///
/// Deep enough to absorb a burst from fifty pods; when it does fill, the reader
/// waits rather than dropping, because the cure for a full channel here is
/// backpressure on one container's stream, not lost text.
const MERGE_CAPACITY: usize = 8_192;

/// How long to wait before re-attaching to a pod whose stream ended.
const REATTACH_DELAY: Duration = Duration::from_millis(500);

/// One line as it comes off a reader.
#[derive(Debug)]
enum Message {
    Line(LogLine),
    Source(LogSource, LogSourceState),
}

/// Splits a timestamped log line into its timestamp and its text.
///
/// The apiserver prefixes each line with an RFC3339 timestamp when asked. A
/// line that does not start with one — which happens when a container writes a
/// partial line — keeps its text whole rather than losing its first word.
fn split_timestamp(line: &str) -> (Option<SystemTime>, &str) {
    let Some((prefix, rest)) = line.split_once(' ') else {
        return (None, line);
    };

    match prefix.parse::<k8s_openapi::jiff::Timestamp>() {
        Ok(timestamp) => {
            let seconds = timestamp.as_second();
            let time = if seconds >= 0 {
                SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)
            } else {
                SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
            };
            (Some(time), rest)
        }
        Err(_) => (None, line),
    }
}

/// Which container to read for a pod, given what the target asked for.
///
/// A pod with one container needs no name. A pod with several needs one, and
/// the apiserver's error for "you did not say" is unhelpful, so the first
/// container is chosen and the caller can switch.
fn container_for(pod: &Pod, wanted: Option<&str>) -> Option<String> {
    let spec = pod.spec.as_ref()?;

    if let Some(wanted) = wanted {
        let known = spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .any(|container| container.name == wanted);
        return known.then(|| wanted.to_owned());
    }

    match spec.containers.as_slice() {
        [] => None,
        [only] => Some(only.name.clone()),
        [first, ..] => Some(first.name.clone()),
    }
}

/// Reads one container's log stream until it ends or the task is cancelled.
async fn read_source(
    api: Api<Pod>,
    pod: String,
    container: String,
    target: Arc<LogTarget>,
    lines: mpsc::Sender<Message>,
) {
    let source = LogSource::new(&pod, &container);
    let params = LogParams {
        container: Some(container.clone()),
        follow: true,
        previous: target.previous,
        tail_lines: target.tail_lines,
        // Timestamps are what make a merged stream readable, and the only way
        // to jump to a point in time later.
        timestamps: true,
        ..LogParams::default()
    };

    loop {
        match api.log_stream(&pod, &params).await {
            Ok(stream) => {
                let _ = lines
                    .send(Message::Source(source.clone(), LogSourceState::Streaming))
                    .await;

                let mut stream = stream.lines();
                loop {
                    match stream.try_next().await {
                        Ok(Some(line)) => {
                            let (timestamp, text) = split_timestamp(&line);
                            if lines
                                .send(Message::Line(LogLine {
                                    source: source.clone(),
                                    timestamp,
                                    text: Arc::from(text),
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            let _ = lines
                                .send(Message::Source(
                                    source.clone(),
                                    LogSourceState::Failed {
                                        reason: error.to_string(),
                                    },
                                ))
                                .await;
                            break;
                        }
                    }
                }

                let _ = lines
                    .send(Message::Source(source.clone(), LogSourceState::Ended))
                    .await;
            }
            Err(error) => {
                let reason = describe(&error);
                tracing::debug!(%pod, %container, %reason, "log stream failed");
                let _ = lines
                    .send(Message::Source(
                        source.clone(),
                        LogSourceState::Failed { reason },
                    ))
                    .await;
            }
        }

        // A container that restarts gets a new stream; `previous` logs are a
        // one-shot read and must not be re-attached, or they would repeat
        // forever.
        if target.previous {
            return;
        }
        tokio::time::sleep(REATTACH_DELAY).await;
    }
}

/// Collects messages, batches lines, and emits them to the UI.
async fn pump(cluster: ClusterId, mut messages: mpsc::Receiver<Message>, events: EventSink) {
    let mut batch: Vec<LogLine> = Vec::with_capacity(BATCH_LINES);
    let mut ticker = tokio::time::interval(BATCH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            message = messages.recv() => match message {
                Some(Message::Line(line)) => {
                    batch.push(line);
                    if batch.len() >= BATCH_LINES
                        && flush(&cluster, &mut batch, &events).is_err()
                    {
                        return;
                    }
                }
                Some(Message::Source(source, state)) => {
                    // Flush first: a source's "ended" must not overtake the last
                    // lines it produced.
                    if flush(&cluster, &mut batch, &events).is_err() {
                        return;
                    }
                    if events
                        .send(ClusterEvent::LogSourceChanged {
                            cluster: cluster.clone(),
                            source,
                            state,
                        })
                        .is_closed()
                    {
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
        .send(ClusterEvent::LogBatch {
            cluster: cluster.clone(),
            lines,
        })
        .is_closed()
    {
        return Err(());
    }
    Ok(())
}

/// Runs a log session until the task is cancelled.
///
/// For a single pod this attaches one reader. For a label selector it watches
/// the pods matching it and keeps a reader per pod, attaching to pods that
/// appear later and re-attaching when one is replaced — which is what makes a
/// tail survive a rollout.
pub async fn run(cluster: ClusterId, client: Client, target: Arc<LogTarget>, events: EventSink) {
    let api: Api<Pod> = Api::namespaced(client, &target.namespace);
    let (sender, receiver) = mpsc::channel(MERGE_CAPACITY);

    let pump = tokio::spawn(pump(cluster.clone(), receiver, events.clone()));
    let mut readers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    match &target.selector {
        LogSelector::Pod(name) => {
            let pod = match api.get(name).await {
                Ok(pod) => pod,
                Err(error) => {
                    let reason = classify(&error).message().to_owned();
                    events.send(ClusterEvent::LogsFailed { cluster, reason });
                    pump.abort();
                    return;
                }
            };

            match container_for(&pod, target.container.as_deref()) {
                Some(container) => {
                    readers.insert(
                        name.to_string(),
                        tokio::spawn(read_source(
                            api.clone(),
                            name.to_string(),
                            container,
                            Arc::clone(&target),
                            sender.clone(),
                        )),
                    );
                }
                None => {
                    events.send(ClusterEvent::LogsFailed {
                        cluster: cluster.clone(),
                        reason: match target.container.as_deref() {
                            Some(container) => {
                                format!("pod {name} has no container named `{container}`")
                            }
                            None => format!("pod {name} has no containers"),
                        },
                    });
                    pump.abort();
                    return;
                }
            }
        }

        LogSelector::Labels(selector) => {
            let config = watcher::Config {
                label_selector: Some(selector.to_string()),
                ..watcher::Config::default()
            };
            let mut pods = Box::pin(watcher(api.clone(), config).default_backoff());

            while let Some(event) = pods.next().await {
                match event {
                    Ok(watcher::Event::Apply(pod) | watcher::Event::InitApply(pod)) => {
                        let name = pod.name_any();
                        // A reader that is still running is already following
                        // this pod; a finished one means the pod was replaced.
                        if readers
                            .get(&name)
                            .is_some_and(|reader| !reader.is_finished())
                        {
                            continue;
                        }
                        let Some(container) = container_for(&pod, target.container.as_deref())
                        else {
                            continue;
                        };

                        tracing::debug!(%cluster, pod = %name, %container, "attaching to logs");
                        readers.insert(
                            name.clone(),
                            tokio::spawn(read_source(
                                api.clone(),
                                name,
                                container,
                                Arc::clone(&target),
                                sender.clone(),
                            )),
                        );
                    }
                    Ok(watcher::Event::Delete(pod)) => {
                        if let Some(reader) = readers.remove(&pod.name_any()) {
                            reader.abort();
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        // One failed listing is not the end of the session: the
                        // readers already attached keep streaming.
                        tracing::warn!(%cluster, %error, "log pod watch failed");
                    }
                }
            }
        }
    }

    // Hold the session open: readers own clones of the sender, and the pump
    // ends when the last one is dropped.
    drop(sender);
    let _ = pump.await;
    for (_, reader) in readers {
        reader.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).expect("fixture is a valid pod")
    }

    #[test]
    fn a_timestamped_line_is_split_into_time_and_text() {
        let (timestamp, text) = split_timestamp("2026-08-18T10:00:00.123456789Z hello world");
        assert_eq!(text, "hello world");
        assert_eq!(
            timestamp,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_047_200))
        );
    }

    #[test]
    fn a_line_without_a_timestamp_keeps_all_of_its_text() {
        // A container writing a partial line must not lose its first word.
        let (timestamp, text) = split_timestamp("hello world");
        assert_eq!(text, "hello world");
        assert_eq!(timestamp, None);

        let (timestamp, text) = split_timestamp("");
        assert_eq!(text, "");
        assert_eq!(timestamp, None);
    }

    #[test]
    fn a_single_container_pod_needs_no_container_name() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] }
        }));
        assert_eq!(container_for(&pod, None).as_deref(), Some("api"));
    }

    #[test]
    fn a_multi_container_pod_defaults_to_the_first() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }, { "name": "sidecar" }] }
        }));
        assert_eq!(container_for(&pod, None).as_deref(), Some("api"));
        assert_eq!(
            container_for(&pod, Some("sidecar")).as_deref(),
            Some("sidecar")
        );
    }

    #[test]
    fn an_init_container_can_be_read_by_name() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": {
                "containers": [{ "name": "api" }],
                "initContainers": [{ "name": "migrate" }]
            }
        }));
        assert_eq!(
            container_for(&pod, Some("migrate")).as_deref(),
            Some("migrate")
        );
    }

    #[test]
    fn a_container_the_pod_does_not_have_is_refused_rather_than_guessed() {
        let pod = pod(json!({
            "metadata": { "name": "api-0", "namespace": "default" },
            "spec": { "containers": [{ "name": "api" }] }
        }));
        // Falling back to `api` here would show the user the wrong container's
        // logs and never say so.
        assert_eq!(container_for(&pod, Some("ghost")), None);
    }
}
