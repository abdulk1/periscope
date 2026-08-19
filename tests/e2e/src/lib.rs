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
pub mod proxy;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
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

/// The absolute path of an external tool this harness shells out to.
///
/// Panics naming the tool and the `PATH` that was searched, which is the whole
/// point: `Command::new("kubectl")` asks `PATH` for something to run and then
/// hands it the developer's kubeconfig — `kubectl config view --raw` prints
/// credentials, and a stub plugin is invoked with a cluster's admin key. A
/// directory earlier on `PATH` that anyone can write to therefore gets code
/// execution with all of that in scope.
///
/// Resolving first does not make `PATH` trustworthy, and it is not meant to:
/// this is a harness a developer runs deliberately on their own machine, so
/// nothing here is a boundary. What it buys is that the *silent* half is gone —
/// a relative entry (the classic `.`, or an empty entry, which means the
/// working directory) is never searched, and a missing tool says so instead of
/// letting whatever else answers to the name run. Refusing group- or
/// world-writable directories was considered and rejected: `/usr/local/bin` is
/// group-writable by design on macOS, so the check would fail on the machines
/// this suite is run from and be worked around rather than heeded.
pub fn tool(name: &str) -> PathBuf {
    find_tool(name).unwrap_or_else(|| {
        let path = std::env::var("PATH").unwrap_or_default();
        panic!(
            "`{name}` is not in any absolute entry of PATH, and these tests need it. PATH={path}"
        )
    })
}

/// The absolute path of an external tool, or `None` when it is not installed.
///
/// For the callers that treat a missing tool as a skipped check rather than a
/// failure. See [`tool`] for why bare names are not spawned.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    find_tool_on(&std::env::var_os("PATH")?, name)
}

/// [`find_tool`], against a `PATH` given rather than inherited.
///
/// Split out so the rule that relative entries are skipped can be tested
/// without a test mutating the environment of every other test in the process.
pub fn find_tool_on(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Whether a path is a file this process could run.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
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

/// The name of the first pod matching a label selector.
pub fn first_pod_named(selector: &str) -> Option<String> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::ListParams;

    let selector = selector.to_owned();
    let name = std::cell::RefCell::new(None);
    blocking(async {
        let api: kube::Api<Pod> = kube::Api::namespaced(client().await?, "default");
        let pods = api.list(&ListParams::default().labels(&selector)).await?;
        *name.borrow_mut() = pods
            .items
            .iter()
            .find(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref())
                    == Some("Running")
            })
            .and_then(|pod| pod.metadata.name.clone());
        Ok(())
    })
    .ok()?;

    name.into_inner()
}

/// Environment variable that turns a missing fixture into a failure.
pub const REQUIRE_FIXTURES_VAR: &str = "PERISCOPE_E2E_REQUIRE_FIXTURES";

/// A running pod from the named fixture, or `None` if it is not installed.
///
/// Skipping when a fixture is absent is right on a developer's machine, where
/// `firehose` burns CPU and nobody wants it running all day. It is wrong in CI:
/// a suite that skips itself is indistinguishable from one that passes, and
/// fifteen tests were silently absent from every pull request until this
/// existed. Setting `PERISCOPE_E2E_REQUIRE_FIXTURES` makes the absence a
/// failure, which is what CI does after applying them.
pub fn fixture(name: &str, selector: &str) -> Option<String> {
    if let Some(pod) = first_pod_named(selector) {
        return Some(pod);
    }

    let instructions = format!("kubectl apply -f tests/e2e/fixtures/{name}.yaml");
    require(
        false,
        &format!("{name} (no pod matches `{selector}`)"),
        &instructions,
    );
    None
}

/// Whether a fixture is in place, saying what to do when it is not.
///
/// Returns `present` so a test can `if !require(..) { return; }`. When the
/// fixture is missing it either skips with instructions or, under
/// `PERISCOPE_E2E_REQUIRE_FIXTURES`, fails — the same rule
/// [`fixture`] follows, for the fixtures that are not pods.
pub fn require(present: bool, name: &str, instructions: &str) -> bool {
    if present {
        return true;
    }

    assert!(
        std::env::var_os(REQUIRE_FIXTURES_VAR).is_none(),
        "the {name} fixture is required here but is not installed \
         ({REQUIRE_FIXTURES_VAR} is set). Install it with: {instructions}"
    );

    eprintln!("skipping: the {name} fixture is not installed ({instructions})");
    false
}

/// Applies a manifest through `kubectl`, whatever kind it is.
///
/// [`apply_yaml`] is typed to Deployments; this is for the fixtures that are
/// not, including instances of custom resources whose Rust type does not exist.
pub fn kubectl_apply(manifest: &str) -> bool {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let Some(kubectl) = find_tool("kubectl") else {
        return false;
    };
    let Ok(mut child) = Command::new(kubectl)
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    else {
        return false;
    };

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(manifest.as_bytes());
    }
    child.wait().is_ok_and(|status| status.success())
}

/// Whether the cluster serves a kind, by the label `kubectl` prints.
pub fn serves_kind(label: &str) -> bool {
    let Some(kubectl) = find_tool("kubectl") else {
        return false;
    };
    let output = std::process::Command::new(kubectl)
        .args(["api-resources", "--no-headers", "-o", "name"])
        .output();

    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == label),
        Err(_) => false,
    }
}

/// Whether a named object exists.
pub fn object_exists(kind: &str, namespace: &str, name: &str) -> bool {
    find_tool("kubectl").is_some_and(|kubectl| {
        std::process::Command::new(kubectl)
            .args(["get", kind, name, "-n", namespace])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// Deletes a pod, so its replacement can be watched for.
pub fn delete_pod(namespace: &str, name: &str) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::DeleteParams;

    let namespace = namespace.to_owned();
    let name = name.to_owned();
    blocking(async move {
        let api: kube::Api<Pod> = kube::Api::namespaced(client().await?, &namespace);
        api.delete(&name, &DeleteParams::default().grace_period(0))
            .await?;
        Ok(())
    })
}

/// Applies a YAML manifest, for tests that need something to change.
///
/// The e2e crate is allowed to write; the application is not.
pub fn apply_yaml(yaml: &str) -> anyhow::Result<()> {
    use kube::api::{Patch, PatchParams};

    let object: serde_json::Value = serde_saphyr::from_str(yaml)?;
    let name = object["metadata"]["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the manifest has no name"))?
        .to_owned();
    let namespace = object["metadata"]["namespace"]
        .as_str()
        .unwrap_or("default")
        .to_owned();

    blocking(async move {
        let api: kube::Api<k8s_openapi::api::apps::v1::Deployment> =
            kube::Api::namespaced(client().await?, &namespace);
        api.patch(
            &name,
            &PatchParams::apply("periscope-e2e").force(),
            &Patch::Apply(&object),
        )
        .await?;
        Ok(())
    })
}

/// Deletes a deployment, ignoring one that is already gone.
pub fn delete_deployment(namespace: &str, name: &str) -> anyhow::Result<()> {
    use k8s_openapi::api::apps::v1::Deployment;
    use kube::api::DeleteParams;

    let (namespace, name) = (namespace.to_owned(), name.to_owned());
    blocking(async move {
        let api: kube::Api<Deployment> = kube::Api::namespaced(client().await?, &namespace);
        match api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
            Err(error) => Err(error.into()),
        }
    })
}

/// Reads a deployment, for asserting what a mutation did.
fn deployment(namespace: &str, name: &str) -> Option<k8s_openapi::api::apps::v1::Deployment> {
    use k8s_openapi::api::apps::v1::Deployment;

    let (namespace, name) = (namespace.to_owned(), name.to_owned());
    let found = std::cell::RefCell::new(None);
    blocking(async {
        let api: kube::Api<Deployment> = kube::Api::namespaced(client().await?, &namespace);
        *found.borrow_mut() = api.get_opt(&name).await?;
        Ok(())
    })
    .ok()?;
    found.into_inner()
}

/// Whether a deployment exists.
pub fn deployment_exists(namespace: &str, name: &str) -> bool {
    deployment(namespace, name).is_some()
}

/// A deployment's desired replica count.
pub fn deployment_replicas(namespace: &str, name: &str) -> Option<i32> {
    deployment(namespace, name)?.spec?.replicas
}

/// Whether a deployment carries the annotation `kubectl rollout restart` sets.
pub fn deployment_has_restart_annotation(namespace: &str, name: &str) -> bool {
    deployment(namespace, name)
        .and_then(|deployment| deployment.spec)
        .and_then(|spec| spec.template.metadata)
        .and_then(|metadata| metadata.annotations)
        .is_some_and(|annotations| annotations.contains_key("kubectl.kubernetes.io/restartedAt"))
}

/// The first node in the cluster.
pub fn first_node() -> Option<String> {
    use k8s_openapi::api::core::v1::Node;

    let name = std::cell::RefCell::new(None);
    blocking(async {
        let api: kube::Api<Node> = kube::Api::all(client().await?);
        *name.borrow_mut() = api
            .list(&kube::api::ListParams::default())
            .await?
            .items
            .first()
            .and_then(|node| node.metadata.name.clone());
        Ok(())
    })
    .ok()?;
    name.into_inner()
}

/// Whether a node is cordoned.
pub fn node_unschedulable(name: &str) -> Option<bool> {
    use k8s_openapi::api::core::v1::Node;

    let name = name.to_owned();
    let flag = std::cell::RefCell::new(None);
    blocking(async {
        let api: kube::Api<Node> = kube::Api::all(client().await?);
        *flag.borrow_mut() = api
            .get_opt(&name)
            .await?
            .and_then(|node| node.spec)
            .and_then(|spec| spec.unschedulable);
        Ok(())
    })
    .ok()?;
    flag.into_inner()
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
