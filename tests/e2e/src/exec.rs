//! Fixtures for exec credential plugins.
//!
//! EKS (`aws eks get-token`), GKE (`gke-gcloud-auth-plugin`) and AKS
//! (`kubelogin`) all authenticate the same way: kubectl-compatible clients run
//! an external program and read an `ExecCredential` from its stdout. These
//! helpers build kubeconfigs that drive that machinery with stub plugins, so
//! the path can be exercised against a real apiserver without a cloud account.
//!
//! Everything here writes secrets to disk. `kind` issues admin client
//! certificates, and the whole point of these fixtures is to hand that
//! credential back through a plugin — so a stub plugin *is* a copy of the
//! cluster's admin private key, and so is every kubeconfig this module writes.
//! Two rules follow, and both are tested in `tests/harness.rs`: those files are
//! created owner-only rather than chmodded after the fact, in a scratch
//! directory this process created exclusively at mode 0700; and nothing outside
//! this harness's control is pasted into a shell script unquoted.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::{context, tool};

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
///
/// `--minify --context` narrows that to the one context under test. `--raw`
/// alone decodes every credential in the file — production clusters the
/// developer happens to have on the same machine included — and hands them to
/// this process for no reason.
fn kubeconfig_json() -> serde_json::Value {
    let context_name = context().to_string();
    let output = std::process::Command::new(tool("kubectl"))
        .args([
            "config",
            "view",
            "--raw",
            "--minify",
            "--context",
            &context_name,
            "--output",
            "json",
        ])
        .output()
        .expect("kubectl runs; these tests need a cluster anyway");
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

/// Ends the heredoc the credential is printed from.
const HEREDOC: &str = "PERISCOPE_EOF";

/// `value` as one `/bin/sh` word, whatever is in it.
///
/// The script below runs with the developer's kubeconfig in scope, and one of
/// the values interpolated into it is a path under `$TMPDIR` — which this
/// harness does not choose. A single quote in it closed the string and handed
/// the rest of the path to the shell as a command.
fn quoted_for_sh(value: &str) -> String {
    // Ending the string, escaping one quote and starting it again is the only
    // way to get a single quote into a single-quoted `sh` word.
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Writes an executable stub plugin that prints `stdout_json` and, if given,
/// `stderr` before exiting with `code`.
///
/// Every invocation appends a line to `<directory>/invocations`, so a test can
/// prove the plugin ran again rather than a cached token being reused.
///
/// The script is only ever readable by its owner: for the tests that hand back
/// a working credential, this file contains the cluster's admin private key.
///
/// # Panics
///
/// If `stdout_json` contains a line that would end the heredoc early, which
/// would turn the rest of the credential into shell commands.
pub fn stub_plugin(
    directory: &Path,
    name: &str,
    stdout_json: &str,
    stderr: &str,
    code: i32,
) -> PathBuf {
    let path = directory.join(name);
    let invocations = directory.join("invocations");

    assert!(
        !stdout_json.lines().any(|line| line == HEREDOC),
        "the credential contains a line reading `{HEREDOC}`, which would end the \
         heredoc and run the rest of it: {stdout_json}"
    );

    let script = format!(
        "#!/bin/sh\n\
         echo ran >> {invocations}\n\
         {stderr_line}\
         cat <<'{HEREDOC}'\n{stdout_json}\n{HEREDOC}\n\
         exit {code}\n",
        invocations = quoted_for_sh(&invocations.to_string_lossy()),
        stderr_line = if stderr.is_empty() {
            String::new()
        } else {
            format!("echo {} >&2\n", quoted_for_sh(stderr))
        },
    );

    write_owner_only(&path, script.as_bytes(), 0o700).expect("the stub plugin is written");
    path
}

/// Writes a file only its owner can read, creating it with that mode.
///
/// `File::create` asks for 0666 and lets the umask decide, so the usual
/// write-then-chmod leaves a window in which a private key on disk is
/// world-readable — and on a machine with a permissive umask it never closes
/// for the files nobody remembered to chmod at all.
fn write_owner_only(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;

    options.open(path)?.write_all(contents)
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

    write_kubeconfig(directory, "kubeconfig-exec.json", &config)
}

/// Writes a fixture kubeconfig into `directory` and returns its path.
///
/// Every kubeconfig this suite generates is a copy of the test context's
/// credentials, which on `kind` means the cluster's admin certificate and key.
/// `std::fs::write` leaves that at the umask's mercy — 0644 on a stock
/// machine — so all of them go through here instead.
pub fn write_kubeconfig(directory: &Path, name: &str, config: &serde_json::Value) -> PathBuf {
    let path = directory.join(name);
    let json = serde_json::to_vec_pretty(config).expect("a kubeconfig serialises");
    write_owner_only(&path, &json, 0o600).expect("the fixture kubeconfig is written");
    path
}

/// A temporary directory that cleans itself up.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl Scratch {
    /// Creates a private directory under the system temp directory.
    ///
    /// Three things about the name and the mode are deliberate, because what
    /// ends up in here is the cluster's admin key. The name carries a random
    /// component, so a directory on a shared machine cannot be sat on before
    /// the run starts; the directory is created exclusively rather than with
    /// `create_dir_all`, which happily adopts one that already exists — under
    /// any owner, and through a symlink to anywhere; and the mode is 0700 from
    /// the moment it exists.
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "periscope-e2e-{label}-{}-{}",
            std::process::id(),
            random_name()
        ));
        create_private_directory(&path)
            .unwrap_or_else(|error| panic!("scratch directory {}: {error}", path.display()));
        Self(path)
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort, and it has to be: a `SIGKILL` or a hard abort leaves the
        // directory behind whatever this does. That is why the mode, and not
        // the cleanup, is what keeps the key private.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A random component for a scratch directory's name.
///
/// `RandomState` is seeded from the operating system's randomness and bumped on
/// every call, which is the only entropy the standard library exposes. A
/// dependency for one filename was not worth it, and the exclusive `mkdir` in
/// [`create_private_directory`] is what actually makes this safe — the random
/// name only stops somebody camping on it first.
fn random_name() -> String {
    use std::hash::{BuildHasher as _, Hasher as _, RandomState};

    format!("{:016x}", RandomState::new().build_hasher().finish())
}

/// Creates `path` and nothing above it, failing if it is already there.
#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().create(path)
}
