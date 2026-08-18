//! Shared helpers for the `kind`-based end-to-end tests.
//!
//! These tests talk to a real apiserver, so they are `#[ignore]`d by default and
//! opted into explicitly:
//!
//! ```text
//! kind create cluster --name periscope
//! cargo test -p periscope-e2e -- --ignored
//! ```
//!
//! The context defaults to `kind-periscope` and can be pointed anywhere with
//! `PERISCOPE_E2E_CONTEXT`. Everything here reads; only the fixture generator
//! (`seed-pods`) writes, and it writes only to the cluster it is told to.

pub mod exec;

use std::time::{Duration, Instant};

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, EventStream, KindId, RuntimeConfig,
};
use periscope_cluster::KubeHandler;

/// Environment variable naming the context to test against.
pub const CONTEXT_VAR: &str = "PERISCOPE_E2E_CONTEXT";

/// The context these tests use unless told otherwise.
pub fn context() -> ClusterId {
    ClusterId::new(std::env::var(CONTEXT_VAR).unwrap_or_else(|_| "kind-periscope".to_owned()))
}

/// Starts the cluster runtime with the real kube handler.
pub fn runtime() -> (ClusterRuntime, EventStream) {
    ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default())
        .expect("the cluster runtime starts")
}

/// The kind the tests watch unless they say otherwise.
pub fn pods() -> KindId {
    KindId::new("", "v1", "Pod", "pods")
}

/// Connects to the test context and returns the runtime, stream and cluster id.
pub fn connected() -> (ClusterRuntime, EventStream, ClusterId) {
    let (runtime, stream) = runtime();
    let cluster = context();
    runtime
        .send(ClusterCommand::Connect {
            cluster: cluster.clone(),
        })
        .expect("connect is queued");
    (runtime, stream, cluster)
}

/// Connects, waits for discovery, and starts watching one kind.
///
/// Discovery has to land first: the cluster layer needs to know whether a kind
/// is namespaced before it can build the right URL.
pub fn watching(kind: KindId, timeout: Duration) -> (ClusterRuntime, EventStream, ClusterId) {
    let (runtime, stream, cluster) = connected();

    wait_for(&stream, timeout, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    runtime
        .send(ClusterCommand::Watch {
            cluster: cluster.clone(),
            kind,
            namespace: None,
            selector: None,
        })
        .expect("watch is queued");

    (runtime, stream, cluster)
}

/// Drains events until one matches, or the deadline passes.
///
/// Returns the matching event and everything seen before it, so a failing test
/// can show what did arrive instead.
pub fn wait_for(
    stream: &EventStream,
    timeout: Duration,
    mut wanted: impl FnMut(&ClusterEvent) -> bool,
) -> Result<(ClusterEvent, Vec<ClusterEvent>), Vec<ClusterEvent>> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();

    while Instant::now() < deadline {
        match stream.try_recv() {
            Some(event) if wanted(&event) => return Ok((event, seen)),
            Some(event) => seen.push(event),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }

    Err(seen)
}

/// A client for the test context, for the fixtures the tests create.
///
/// Tests are allowed to write; the application is not. Everything created here
/// carries the `app.kubernetes.io/managed-by: periscope-e2e` label and a node
/// selector that matches nothing, so it is never scheduled and never runs.
pub async fn client() -> anyhow::Result<kube::Client> {
    use kube::config::{KubeConfigOptions, Kubeconfig};

    let kubeconfig = Kubeconfig::read()?;
    let config = kube::Config::from_custom_kubeconfig(
        kubeconfig,
        &KubeConfigOptions {
            context: Some(context().to_string()),
            ..KubeConfigOptions::default()
        },
    )
    .await?;
    Ok(kube::Client::try_from(config)?)
}

/// Creates an unschedulable probe pod in `default`, blocking until it exists.
pub fn create_probe_pod(name: &str) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::PostParams;

    let pod: Pod = serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": name,
            "namespace": "default",
            "labels": { "app.kubernetes.io/managed-by": "periscope-e2e" }
        },
        "spec": {
            "nodeSelector": { "periscope.dev/nonexistent": "true" },
            "containers": [{ "name": "placeholder", "image": "registry.k8s.io/pause:3.10" }]
        }
    }))?;

    blocking(async move {
        let api: kube::Api<Pod> = kube::Api::namespaced(client().await?, "default");
        api.create(&PostParams::default(), &pod).await?;
        Ok(())
    })
}

/// Deletes a probe pod immediately.
pub fn delete_probe_pod(name: &str) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::DeleteParams;

    let name = name.to_owned();
    blocking(async move {
        let api: kube::Api<Pod> = kube::Api::namespaced(client().await?, "default");
        api.delete(&name, &DeleteParams::default().grace_period(0))
            .await?;
        Ok(())
    })
}

/// Runs one future to completion on a throwaway runtime.
///
/// The tests are synchronous — they drive the same bridge the UI does — so the
/// fixtures need their own runtime rather than borrowing the app's.
fn blocking<F>(future: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

/// Summarises events for an assertion message.
pub fn describe(events: &[ClusterEvent]) -> String {
    events
        .iter()
        .map(|event| match event {
            ClusterEvent::Status { cluster, state } => format!("Status({cluster}, {state:?})"),
            ClusterEvent::Kinds { cluster, kinds } => {
                format!("Kinds({cluster}, {} kinds)", kinds.len())
            }
            ClusterEvent::ResourceReset { kind, rows, .. } => {
                format!("ResourceReset({kind}, {} rows)", rows.len())
            }
            ClusterEvent::ResourceApplied { kind, row, .. } => {
                format!("ResourceApplied({kind}, {})", row.key)
            }
            ClusterEvent::ResourceDeleted { kind, key, .. } => {
                format!("ResourceDeleted({kind}, {key})")
            }
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
