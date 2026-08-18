//! Seeds a cluster with many pods, for the Phase 1 load budget.
//!
//! The pods carry a node selector that matches nothing, so they are recorded by
//! the apiserver and streamed to every watcher but never scheduled and never
//! run. That is what makes 10,000 of them possible on a one-node `kind`
//! cluster, and it keeps the fixture free of images to pull.
//!
//! This is a **test fixture generator and the only thing in this repository
//! that writes to a cluster.** It refuses to touch anything outside the
//! namespace it creates.
//!
//! ```text
//! cargo run -p periscope-e2e --bin seed-pods -- --count 10000
//! cargo run -p periscope-e2e --bin seed-pods -- --delete
//! ```

use anyhow::{Context as _, Result};
use clap::Parser;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::api::{DeleteParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};

/// Seeds a cluster with unschedulable pods for load testing.
#[derive(Debug, Parser)]
#[command(name = "seed-pods")]
struct Cli {
    /// kubeconfig context to seed. Defaults to `$PERISCOPE_E2E_CONTEXT`, then
    /// `kind-periscope`.
    #[arg(long)]
    context: Option<String>,

    /// Namespace to create the pods in. It is created if missing.
    #[arg(long, default_value = "periscope-load")]
    namespace: String,

    /// How many pods to create.
    #[arg(long, default_value_t = 10_000)]
    count: usize,

    /// How many creations to have in flight at once.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,

    /// Delete the namespace and everything in it instead of creating pods.
    #[arg(long)]
    delete: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let context = cli.context.clone().unwrap_or_else(|| {
        std::env::var(periscope_e2e::CONTEXT_VAR).unwrap_or_else(|_| "kind-periscope".to_owned())
    });

    let kubeconfig = Kubeconfig::read().context("reading kubeconfig")?;
    let config = Config::from_custom_kubeconfig(
        kubeconfig,
        &KubeConfigOptions {
            context: Some(context.clone()),
            ..KubeConfigOptions::default()
        },
    )
    .await
    .with_context(|| format!("building a client for context `{context}`"))?;

    // A misdirected fixture run is destructive, so say where it is going.
    println!("context {context} -> {}", config.cluster_url);
    let client = Client::try_from(config)?;

    if cli.delete {
        return delete(client, &cli.namespace).await;
    }
    seed(client, &cli).await
}

async fn delete(client: Client, namespace: &str) -> Result<()> {
    let namespaces: Api<Namespace> = Api::all(client);
    namespaces
        .delete(namespace, &DeleteParams::default())
        .await
        .with_context(|| format!("deleting namespace `{namespace}`"))?;
    println!("deleting namespace {namespace}");
    Ok(())
}

async fn seed(client: Client, cli: &Cli) -> Result<()> {
    ensure_namespace(&client, &cli.namespace).await?;

    let pods: Api<Pod> = Api::namespaced(client, &cli.namespace);
    let started = std::time::Instant::now();

    let created = futures::stream::iter(0..cli.count)
        .map(|index| {
            let pods = pods.clone();
            let namespace = cli.namespace.clone();
            async move {
                let pod = fixture(&namespace, index);
                match pods.create(&PostParams::default(), &pod).await {
                    Ok(_) => 1,
                    // Re-running the seeder should top the namespace up, not fail.
                    Err(kube::Error::Api(status)) if status.code == 409 => 0,
                    Err(error) => {
                        eprintln!("pod {index}: {error}");
                        0
                    }
                }
            }
        })
        .buffer_unordered(cli.concurrency)
        .fold(0_usize, |total, created| async move { total + created })
        .await;

    println!(
        "created {created} pods in {:.1}s ({} requested)",
        started.elapsed().as_secs_f64(),
        cli.count
    );
    Ok(())
}

async fn ensure_namespace(client: &Client, namespace: &str) -> Result<()> {
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let manifest: Namespace = serde_json::from_value(serde_json::json!({
        "metadata": { "name": namespace, "labels": { "app.kubernetes.io/managed-by": "periscope-e2e" } }
    }))?;

    match namespaces.create(&PostParams::default(), &manifest).await {
        Ok(_) | Err(kube::Error::Api(_)) => Ok(()),
        Err(error) => Err(error).context("creating the fixture namespace"),
    }
}

/// A pod that will never be scheduled, and so never runs anything.
fn fixture(namespace: &str, index: usize) -> Pod {
    serde_json::from_value(serde_json::json!({
        "metadata": {
            "name": format!("load-{index:05}"),
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "periscope-e2e" }
        },
        "spec": {
            "nodeSelector": { "periscope.dev/nonexistent": "true" },
            "containers": [{
                "name": "placeholder",
                "image": "registry.k8s.io/pause:3.10"
            }]
        }
    }))
    .expect("the fixture is a valid pod")
}
