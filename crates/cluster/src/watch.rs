//! Watching pods for one cluster.
//!
//! The stream mechanics live in `kube`; what this module owns is the
//! translation into [`ClusterEvent`]s and the connection state machine around
//! it — including the rule that a rejected credential ends the session with a
//! stated reason rather than retrying silently forever.

use std::sync::Arc;

use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::{WatchStreamExt as _, watcher};
use kube::{Api, Client};
use periscope_bridge::{ClusterEvent, ClusterId, ConnectionState, EventSink, PodSnapshot};

use crate::errors::{Failure, attribute_plugin, classify_watch};
use crate::pods;

/// What to do after handling a watch error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterError {
    /// Keep watching; `kube`'s backoff decides when to retry.
    Retry,
    /// Stop. The credential was rejected and retrying would just hammer the
    /// apiserver with a token it has already refused.
    Stop,
}

/// Accumulates watch events into the events the UI understands.
///
/// Kept separate from the stream loop so the whole translation — including the
/// buffering that turns a resync into one atomic replacement — is testable
/// without a cluster.
#[derive(Debug)]
pub struct PodStream {
    cluster: ClusterId,
    /// Objects seen since the last `Init`, held back until `InitDone` so the
    /// UI swaps the table in one go rather than growing it row by row.
    pending: Vec<PodSnapshot>,
    /// The credential plugin this cluster authenticates with, if any.
    credential_plugin: Option<String>,
}

impl PodStream {
    /// A stream translator for one cluster.
    pub fn new(cluster: ClusterId) -> Self {
        Self {
            cluster,
            pending: Vec::new(),
            credential_plugin: None,
        }
    }

    /// Names the credential plugin, so auth failures can say which binary was
    /// involved. `kube` reports a plugin that will not start as "No such file
    /// or directory" and never mentions what it tried to run.
    pub fn with_credential_plugin(mut self, plugin: Option<String>) -> Self {
        self.credential_plugin = plugin;
        self
    }

    /// Translates one watch event, if it produces anything for the UI.
    pub fn apply(&mut self, event: watcher::Event<Pod>) -> Option<ClusterEvent> {
        match event {
            watcher::Event::Init => {
                self.pending.clear();
                None
            }
            watcher::Event::InitApply(pod) => {
                self.pending.push(pods::project(&pod));
                None
            }
            watcher::Event::InitDone => Some(ClusterEvent::PodsReset {
                cluster: self.cluster.clone(),
                pods: Arc::from(std::mem::take(&mut self.pending)),
            }),
            watcher::Event::Apply(pod) => Some(ClusterEvent::PodApplied {
                cluster: self.cluster.clone(),
                pod: Arc::new(pods::project(&pod)),
            }),
            watcher::Event::Delete(pod) => Some(ClusterEvent::PodDeleted {
                cluster: self.cluster.clone(),
                key: pods::project(&pod).key,
            }),
        }
    }

    /// Translates a watch failure into a connection state and a verdict on
    /// whether the session can continue.
    pub fn on_error(&mut self, error: &watcher::Error) -> (ClusterEvent, AfterError) {
        // Whatever we had buffered belongs to a list that will now be redone.
        self.pending.clear();

        let failure = classify_watch(error);
        let (state, after) = match failure {
            Failure::Auth(reason) => (
                ConnectionState::AuthFailed {
                    reason: attribute_plugin(reason, self.credential_plugin.as_deref()),
                },
                AfterError::Stop,
            ),
            Failure::Other(reason) => (ConnectionState::Degraded { reason }, AfterError::Retry),
        };

        (
            ClusterEvent::Status {
                cluster: self.cluster.clone(),
                state,
            },
            after,
        )
    }

    /// The cluster this stream belongs to.
    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }
}

/// Watches pods in every namespace until the task is cancelled, the credential
/// is rejected, or the UI goes away.
pub async fn run(
    cluster: ClusterId,
    client: Client,
    credential_plugin: Option<String>,
    events: EventSink,
) {
    let api: Api<Pod> = Api::all(client);
    let mut stream = Box::pin(watcher(api, watcher::Config::default()).default_backoff());
    let mut translator = PodStream::new(cluster.clone()).with_credential_plugin(credential_plugin);
    // Only report a recovery once, rather than on every event after one.
    let mut degraded = false;

    while let Some(next) = stream.next().await {
        match next {
            Ok(event) => {
                let recovered = degraded && matches!(event, watcher::Event::InitDone);
                let Some(event) = translator.apply(event) else {
                    continue;
                };

                if events.send(event).is_closed() {
                    return;
                }

                if recovered {
                    degraded = false;
                    tracing::info!(%cluster, "watch recovered");
                    if events
                        .send(ClusterEvent::Status {
                            cluster: cluster.clone(),
                            state: ConnectionState::Connected,
                        })
                        .is_closed()
                    {
                        return;
                    }
                }
            }
            Err(error) => {
                let (event, after) = translator.on_error(&error);
                degraded = true;
                tracing::warn!(%cluster, %error, ?after, "pod watch failed");

                if events.send(event).is_closed() {
                    return;
                }
                if after == AfterError::Stop {
                    return;
                }
            }
        }
    }

    tracing::info!(%cluster, "pod watch ended");
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::ResourceKey;
    use serde_json::json;

    fn pod(name: &str) -> Pod {
        serde_json::from_value(json!({
            "metadata": { "name": name, "namespace": "default" },
            "spec": { "containers": [{ "name": "app" }] },
            "status": { "phase": "Running" }
        }))
        .expect("fixture is a valid pod")
    }

    fn api_error(code: u16) -> watcher::Error {
        watcher::Error::WatchStartFailed(kube::Error::Api(Box::new(kube::core::Status {
            code,
            message: "token has expired".to_owned(),
            ..kube::core::Status::default()
        })))
    }

    #[test]
    fn an_initial_list_is_held_back_until_it_is_complete() {
        let mut stream = PodStream::new("prod".into());

        assert_eq!(stream.apply(watcher::Event::Init), None);
        assert_eq!(stream.apply(watcher::Event::InitApply(pod("a"))), None);
        assert_eq!(stream.apply(watcher::Event::InitApply(pod("b"))), None);

        // A half-listed table would flash rows in and out; the reset lands once.
        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::PodsReset { cluster, pods }) => {
                assert_eq!(cluster.as_str(), "prod");
                assert_eq!(pods.len(), 2);
            }
            other => panic!("expected a reset, got {other:?}"),
        }
    }

    #[test]
    fn a_second_list_does_not_inherit_the_first_ones_objects() {
        let mut stream = PodStream::new("prod".into());
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("a")));

        // Watch dropped mid-list and restarted.
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("b")));

        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::PodsReset { pods, .. }) => {
                assert_eq!(pods.len(), 1);
                assert_eq!(&*pods[0].key.name, "b");
            }
            other => panic!("expected a reset, got {other:?}"),
        }
    }

    #[test]
    fn updates_and_deletes_map_to_their_own_events() {
        let mut stream = PodStream::new("prod".into());

        match stream.apply(watcher::Event::Apply(pod("a"))) {
            Some(ClusterEvent::PodApplied { pod, .. }) => {
                assert_eq!(pod.key, ResourceKey::new("default", "a"));
            }
            other => panic!("expected an apply, got {other:?}"),
        }

        match stream.apply(watcher::Event::Delete(pod("a"))) {
            Some(ClusterEvent::PodDeleted { key, .. }) => {
                assert_eq!(key, ResourceKey::new("default", "a"));
            }
            other => panic!("expected a delete, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_credential_stops_the_session_and_says_why() {
        let mut stream = PodStream::new("prod".into());
        let (event, after) = stream.on_error(&api_error(401));

        assert_eq!(after, AfterError::Stop);
        match event {
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { reason },
                ..
            } => assert!(reason.contains("token has expired"), "{reason}"),
            other => panic!("expected an auth failure, got {other:?}"),
        }
    }

    #[test]
    fn an_auth_failure_names_the_credential_plugin_that_was_run() {
        // Without this the user sees "No such file or directory" and has no way
        // to know which binary kubeconfig asked for.
        let mut stream = PodStream::new("prod".into())
            .with_credential_plugin(Some("gke-gcloud-auth-plugin".to_owned()));
        let (event, _) = stream.on_error(&watcher::Error::WatchStartFailed(kube::Error::Auth(
            kube::client::AuthError::AuthExecStart(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )),
        )));

        match event {
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { reason },
                ..
            } => assert!(reason.contains("gke-gcloud-auth-plugin"), "{reason}"),
            other => panic!("expected an auth failure, got {other:?}"),
        }
    }

    #[test]
    fn a_plugin_already_named_in_the_error_is_not_repeated() {
        let mut stream =
            PodStream::new("prod".into()).with_credential_plugin(Some("aws".to_owned()));
        // kube names the command itself when the plugin runs and fails; saying
        // it twice reads like two different problems.
        let error =
            watcher::Error::WatchStartFailed(kube::Error::Api(Box::new(kube::core::Status {
                code: 401,
                message: "auth exec command 'aws' failed with status 255".to_owned(),
                ..kube::core::Status::default()
            })));
        let (event, _) = stream.on_error(&error);

        let ClusterEvent::Status {
            state: ConnectionState::AuthFailed { reason },
            ..
        } = event
        else {
            panic!("expected an auth failure")
        };
        assert!(
            !reason.contains("credential plugin:"),
            "the plugin was named twice: {reason}"
        );
    }

    #[test]
    fn a_transient_failure_degrades_but_keeps_watching() {
        let mut stream = PodStream::new("prod".into());
        let (event, after) = stream.on_error(&api_error(500));

        assert_eq!(after, AfterError::Retry);
        assert!(matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Degraded { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_failure_discards_a_partial_list() {
        let mut stream = PodStream::new("prod".into());
        stream.apply(watcher::Event::Init);
        stream.apply(watcher::Event::InitApply(pod("a")));
        stream.on_error(&api_error(500));

        // The retry lists again from scratch; the half-list must not leak into
        // the next reset, or deleted objects would reappear.
        stream.apply(watcher::Event::Init);
        match stream.apply(watcher::Event::InitDone) {
            Some(ClusterEvent::PodsReset { pods, .. }) => assert!(pods.is_empty()),
            other => panic!("expected an empty reset, got {other:?}"),
        }
    }
}
