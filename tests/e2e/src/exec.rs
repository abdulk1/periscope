//! Fixtures for exec credential plugins.
//!
//! EKS (`aws eks get-token`), GKE (`gke-gcloud-auth-plugin`) and AKS
//! (`kubelogin`) all authenticate the same way: kubectl-compatible clients run
//! an external program and read an `ExecCredential` from its stdout. These
//! helpers build kubeconfigs that drive that machinery with stub plugins, so
//! the path can be exercised against a real apiserver without a cloud account.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::context;

/// Decodes one of kubeconfig's base64 blobs into the PEM text an
/// `ExecCredential` carries.
pub fn decode(blob: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .expect("kubeconfig blobs are base64");
    String::from_utf8(bytes).expect("certificates are PEM text")
}

/// The test context's kubeconfig, as JSON, straight from `kubectl`.
///
/// Shelling out avoids depending on `secrecy` just to read a key back out of
/// `kube`'s parsed types, and it is exactly what a user would run to inspect
/// their own config.
fn kubeconfig_json() -> serde_json::Value {
    let output = std::process::Command::new("kubectl")
        .args(["config", "view", "--raw", "--output", "json"])
        .output()
        .expect("kubectl is on PATH; these tests need a cluster anyway");
    assert!(
        output.status.success(),
        "kubectl config view failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("kubectl emits JSON")
}

fn named<'a>(list: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    list.as_array()?
        .iter()
        .find(|entry| entry.get("name").and_then(serde_json::Value::as_str) == Some(name))
}

/// The apiserver and CA of the test cluster, as a kubeconfig `clusters` entry.
pub fn test_cluster() -> serde_json::Value {
    let config = kubeconfig_json();
    let context_name = context().to_string();
    let cluster_name = named(&config["contexts"], &context_name)
        .and_then(|entry| entry["context"]["cluster"].as_str())
        .unwrap_or_else(|| panic!("context `{context_name}` is not in kubeconfig"))
        .to_owned();

    named(&config["clusters"], &cluster_name)
        .expect("the context names a cluster that exists")
        .clone()
}

/// The base64 client certificate and key the test context authenticates with.
///
/// `kind` issues admin client certificates, which is what makes it possible to
/// hand a *valid* credential back from a stub plugin and watch the connection
/// succeed rather than only ever testing failures.
pub fn test_client_certificate() -> Option<(String, String)> {
    let config = kubeconfig_json();
    let context_name = context().to_string();
    let user = named(&config["contexts"], &context_name)?["context"]["user"]
        .as_str()?
        .to_owned();
    let auth = named(&config["users"], &user)?;

    Some((
        auth["user"]["client-certificate-data"].as_str()?.to_owned(),
        auth["user"]["client-key-data"].as_str()?.to_owned(),
    ))
}

/// Writes an executable stub plugin that prints `stdout_json` and, if given,
/// `stderr` before exiting with `code`.
///
/// Every invocation appends a line to `<directory>/invocations`, so a test can
/// prove the plugin ran again rather than a cached token being reused.
pub fn stub_plugin(
    directory: &Path,
    name: &str,
    stdout_json: &str,
    stderr: &str,
    code: i32,
) -> PathBuf {
    let path = directory.join(name);
    let invocations = directory.join("invocations");

    let script = format!(
        "#!/bin/sh\n\
         echo ran >> '{invocations}'\n\
         {stderr_line}\
         cat <<'PERISCOPE_EOF'\n{stdout_json}\nPERISCOPE_EOF\n\
         exit {code}\n",
        invocations = invocations.display(),
        stderr_line = if stderr.is_empty() {
            String::new()
        } else {
            format!("echo '{stderr}' >&2\n")
        },
    );

    let mut file = std::fs::File::create(&path).expect("the stub plugin is written");
    file.write_all(script.as_bytes()).expect("script contents");
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub plugin is executable");

    path
}

/// How many times a stub plugin in `directory` has been invoked.
pub fn invocations(directory: &Path) -> usize {
    std::fs::read_to_string(directory.join("invocations"))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

/// Writes a kubeconfig whose user authenticates through `command`.
///
/// `api_version` and `args` mirror what the real vendor plugins are configured
/// with, so the shape under test is the shape users actually have on disk.
pub fn kubeconfig_with_exec(
    directory: &Path,
    command: &Path,
    api_version: &str,
    args: &[&str],
) -> PathBuf {
    let context_name = context().to_string();
    let cluster = test_cluster();
    let cluster_name = cluster
        .get("name")
        .and_then(serde_json::Value::as_str)
        .expect("the cluster entry is named")
        .to_owned();

    let config = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "current-context": context_name,
        "clusters": [cluster],
        "contexts": [{
            "name": context_name,
            "context": { "cluster": cluster_name, "user": "exec-user" }
        }],
        "users": [{
            "name": "exec-user",
            "user": {
                "exec": {
                    "apiVersion": api_version,
                    "command": command.display().to_string(),
                    "args": args,
                    "interactiveMode": "Never"
                }
            }
        }]
    });

    let path = directory.join("kubeconfig-exec.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap())
        .expect("the fixture kubeconfig is written");
    path
}

/// A temporary directory that cleans itself up.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    /// Creates a uniquely named directory under the system temp directory.
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "periscope-e2e-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).expect("scratch directory");
        Self(path)
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
