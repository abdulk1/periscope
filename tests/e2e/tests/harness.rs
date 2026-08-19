//! What the harness itself promises, proven without a cluster.
//!
//! Every other test in this directory needs a `kind` apiserver and is
//! `#[ignore]`d. These are not: they are about what the fixtures leave on disk
//! and what they hand to a shell. That is where a test harness stops being
//! merely broken and starts being the thing that hands somebody else the
//! cluster's admin key — so the claims are cheap to check and are checked on
//! every `cargo test`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;

use periscope_e2e::exec::{Scratch, invocations, stub_plugin, write_kubeconfig};

/// The permission bits of a path that exists.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Runs a stub plugin the way a kubectl-compatible client would.
fn run(plugin: &Path, directory: &Path) -> Output {
    std::process::Command::new(plugin)
        .current_dir(directory)
        .output()
        .unwrap_or_else(|error| panic!("the stub plugin runs: {error}"))
}

#[test]
fn a_scratch_directory_is_private_to_the_user_who_made_it() {
    let scratch = Scratch::new("modes");
    let mode = mode_of(scratch.path());

    assert_eq!(
        mode & 0o077,
        0,
        "{} is mode {mode:o}; the stub plugins and kubeconfigs written into it \
         are copies of the cluster's admin key",
        scratch.path().display()
    );
}

#[test]
fn a_generated_kubeconfig_is_readable_only_by_its_owner() {
    let scratch = Scratch::new("kubeconfig-mode");
    let path = write_kubeconfig(
        scratch.path(),
        "fixture.json",
        &serde_json::json!({
            "apiVersion": "v1",
            "kind": "Config",
            "users": [{ "name": "u", "user": { "client-key-data": "cGVt" } }]
        }),
    );

    let mode = mode_of(&path);
    assert_eq!(mode & 0o077, 0, "the kubeconfig is mode {mode:o}");
}

#[test]
fn a_stub_plugin_is_readable_only_by_its_owner() {
    let scratch = Scratch::new("plugin-mode");
    // The credential is baked into the script, so the script is the secret.
    let plugin = stub_plugin(scratch.path(), "aws", r#"{"kind":"ExecCredential"}"#, "", 0);

    let mode = mode_of(&plugin);
    assert_eq!(mode & 0o077, 0, "the stub plugin is mode {mode:o}");
    assert_ne!(
        mode & 0o100,
        0,
        "the stub plugin is not executable: {mode:o}"
    );
}

#[test]
fn a_quote_in_the_temp_directory_does_not_inject_a_command_into_a_stub_plugin() {
    let scratch = Scratch::new("sh-injection");
    // What `$TMPDIR` is free to contain. The path was pasted between single
    // quotes unescaped, so this closed the string and the rest ran as commands
    // — in a script the suite then executes with a kubeconfig in scope.
    let hostile = scratch.path().join("t'; touch injected; :'");
    std::fs::create_dir(&hostile).expect("a directory may be named this");

    let plugin = stub_plugin(&hostile, "aws", r#"{"kind":"ExecCredential"}"#, "", 0);
    let output = run(&plugin, &hostile);

    assert!(
        !hostile.join("injected").exists(),
        "the directory's name was executed as a command"
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        invocations(&hostile),
        1,
        "the plugin did not record its own run, so the quoting broke the script"
    );
}

#[test]
fn a_quote_in_a_plugin_message_does_not_inject_a_command() {
    let scratch = Scratch::new("stderr-injection");
    let message = "Token has expired'; touch injected; :'";
    let plugin = stub_plugin(scratch.path(), "aws", "{}", message, 255);

    let output = run(&plugin, scratch.path());

    assert!(
        !scratch.path().join("injected").exists(),
        "the plugin's message was executed as a command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(message),
        "the message must reach the user verbatim, got: {stderr}"
    );
}

#[test]
fn a_stub_plugin_prints_the_credential_it_was_given_unchanged() {
    let scratch = Scratch::new("credential");
    // A real `ExecCredential` carries PEM text: newlines, quotes, `$` and
    // backticks all have to survive the script untouched, which is what the
    // quoted heredoc is for.
    let credential = serde_json::json!({
        "kind": "ExecCredential",
        "status": { "clientKeyData": "-----BEGIN KEY-----\n$HOME `id` 'quoted' \"x\"\n" }
    })
    .to_string();

    let plugin = stub_plugin(scratch.path(), "gke-gcloud-auth-plugin", &credential, "", 0);
    let output = run(&plugin, scratch.path());

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        credential,
        "the credential came back changed"
    );
}

#[test]
fn a_relative_path_entry_is_never_searched_for_a_tool() {
    let scratch = Scratch::new("path-lookup");
    let tool = stub_plugin(scratch.path(), "periscope-fake-tool", "{}", "", 0);

    let absolute = std::env::join_paths([scratch.path()]).expect("a PATH");
    assert_eq!(
        periscope_e2e::find_tool_on(&absolute, "periscope-fake-tool").as_deref(),
        Some(tool.as_path()),
        "an absolute entry holding the tool should find it"
    );

    // The same directory, reached the way a hostile entry does: without a
    // leading slash, so what it names depends on where the process happens to
    // be standing.
    let route = relative_route_to(scratch.path());
    assert!(
        route.join("periscope-fake-tool").exists(),
        "the route must really reach the tool, or this test proves nothing"
    );

    let relative = std::env::join_paths([&route]).expect("a PATH");
    assert_eq!(
        periscope_e2e::find_tool_on(&relative, "periscope-fake-tool"),
        None,
        "a relative PATH entry was searched: {}",
        route.display()
    );
}

/// `path` written as a route from the working directory.
fn relative_route_to(path: &Path) -> PathBuf {
    let working = std::env::current_dir().expect("a working directory");
    let mut route = PathBuf::new();
    for _ in working.components().skip(1) {
        route.push("..");
    }
    route.push(
        path.strip_prefix("/")
            .expect("a temp directory is absolute"),
    );
    route
}
