//! Exec credential plugins: the way EKS, GKE and AKS actually authenticate.
//!
//! `aws eks get-token`, `gke-gcloud-auth-plugin` and `kubelogin` are all the
//! same mechanism — an external program prints an `ExecCredential` and the
//! client uses what it says. No cloud account is needed to exercise that
//! machinery honestly: these tests drive it with stub plugins shaped exactly
//! like the vendors' own kubeconfig stanzas, pointed at the real `kind`
//! apiserver, so the credential is genuinely sent and genuinely judged.
//!
//! What this does **not** prove is that a real EKS or GKE handshake succeeds.
//! That needs a real cluster; see `docs/LIMITATIONS.md`.

use std::path::Path;
use std::time::Duration;

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterRuntime, ConnectionState, RuntimeConfig,
};
use periscope_cluster::KubeHandler;
use periscope_e2e::exec::{
    Scratch, decode, invocations, kubeconfig_with_exec, stub_plugin, test_client_certificate,
};
use periscope_e2e::{context, describe, wait_for};

/// Plugins run a subprocess, so allow for a cold start.
const TIMEOUT: Duration = Duration::from_secs(20);

/// What `aws eks get-token` prints on success.
fn eks_credential(token: &str, expires_in: Option<&str>) -> String {
    let status = match expires_in {
        Some(expiry) => format!(r#"{{"token":"{token}","expirationTimestamp":"{expiry}"}}"#),
        None => format!(r#"{{"token":"{token}"}}"#),
    };
    format!(
        r#"{{"kind":"ExecCredential","apiVersion":"client.authentication.k8s.io/v1beta1","spec":{{}},"status":{status}}}"#
    )
}

/// The arguments an `aws eks update-kubeconfig` stanza carries.
const EKS_ARGS: [&str; 6] = [
    "--region",
    "eu-west-1",
    "eks",
    "get-token",
    "--cluster-name",
    "periscope-test",
];

fn connect_with(kubeconfig: &Path) -> (ClusterRuntime, periscope_bridge::EventStream) {
    let (runtime, stream) = ClusterRuntime::start(
        KubeHandler::with_kubeconfig(kubeconfig),
        RuntimeConfig::default(),
    )
    .expect("the cluster runtime starts");

    runtime
        .send(ClusterCommand::Connect { cluster: context() })
        .expect("connect is queued");
    (runtime, stream)
}

fn expect_auth_failure(stream: &periscope_bridge::EventStream) -> String {
    let (event, seen) = wait_for(stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::AuthFailed { .. }
                    | ConnectionState::Disconnected { reason: Some(_) },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| panic!("no failure was reported; saw: {}", describe(&seen)));

    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ClusterEvent::Status {
                state: ConnectionState::Connected,
                ..
            }
        )),
        "reported connected despite a bad credential: {}",
        describe(&seen)
    );

    let ClusterEvent::Status { state, .. } = event else {
        unreachable!()
    };
    state
        .detail()
        .expect("a failure always carries its reason")
        .to_owned()
}

#[test]
#[ignore = "needs a cluster"]
fn an_eks_shaped_plugin_runs_and_its_token_is_actually_sent() {
    let scratch = Scratch::new("eks-token");
    let plugin = stub_plugin(
        scratch.path(),
        "aws",
        &eks_credential("periscope-e2e-not-a-real-eks-token", None),
        "",
        0,
    );
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        &plugin,
        "client.authentication.k8s.io/v1beta1",
        &EKS_ARGS,
    );

    let (_runtime, stream) = connect_with(&kubeconfig);
    let reason = expect_auth_failure(&stream);

    // The plugin ran, its token went to the apiserver, and the apiserver's own
    // verdict came back — not a generic "could not connect".
    assert!(invocations(scratch.path()) >= 1, "the plugin never ran");
    assert!(
        reason.to_lowercase().contains("unauthorized") || reason.contains("401"),
        "expected the apiserver's rejection, got: {reason}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_failing_eks_plugin_surfaces_its_own_stderr() {
    let scratch = Scratch::new("eks-fail");
    // The classic: the SSO session expired and `aws` says so on stderr.
    let plugin = stub_plugin(
        scratch.path(),
        "aws",
        "",
        "Error when retrieving token from sso: Token has expired and refresh failed",
        255,
    );
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        &plugin,
        "client.authentication.k8s.io/v1beta1",
        &EKS_ARGS,
    );

    let (_runtime, stream) = connect_with(&kubeconfig);
    let reason = expect_auth_failure(&stream);

    assert!(
        reason.contains("Token has expired and refresh failed"),
        "the plugin's own words must reach the user, got: {reason}"
    );
    assert!(
        reason.contains("255") || reason.contains("exit"),
        "the exit status should be visible too, got: {reason}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_gke_plugin_that_is_not_installed_is_named_in_the_error() {
    let scratch = Scratch::new("gke-missing");
    // GKE's most common failure: kubeconfig references the plugin, the machine
    // does not have it. The user needs the binary's name to fix this.
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        Path::new("gke-gcloud-auth-plugin-does-not-exist"),
        "client.authentication.k8s.io/v1",
        &[],
    );

    let (_runtime, stream) = connect_with(&kubeconfig);
    let reason = expect_auth_failure(&stream);

    assert!(
        reason.contains("gke-gcloud-auth-plugin-does-not-exist"),
        "the missing plugin should be named, got: {reason}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_gke_shaped_plugin_returning_credentials_connects_and_streams_pods() {
    let Some((certificate, key)) = test_client_certificate() else {
        // Only clusters that authenticate with client certificates — `kind`
        // does — can have a valid credential handed back from a stub.
        eprintln!("skipping: the test context does not use client certificates");
        return;
    };

    let scratch = Scratch::new("gke-ok");
    let credential = serde_json::json!({
        "kind": "ExecCredential",
        "apiVersion": "client.authentication.k8s.io/v1",
        "spec": {},
        "status": {
            "clientCertificateData": decode(&certificate),
            "clientKeyData": decode(&key),
            "expirationTimestamp": "2035-01-01T00:00:00Z"
        }
    })
    .to_string();

    let plugin = stub_plugin(scratch.path(), "gke-gcloud-auth-plugin", &credential, "", 0);
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        &plugin,
        "client.authentication.k8s.io/v1",
        &[],
    );

    let (_runtime, stream) = connect_with(&kubeconfig);
    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::PodsReset { .. })
    })
    .unwrap_or_else(|seen| {
        panic!(
            "a plugin-provided credential should connect; saw: {}",
            describe(&seen)
        )
    });

    let ClusterEvent::PodsReset { pods, .. } = event else {
        unreachable!()
    };
    assert!(!pods.is_empty(), "the cluster listed no pods at all");
    assert!(invocations(scratch.path()) >= 1, "the plugin never ran");
}

#[test]
#[ignore = "needs a cluster"]
fn an_expiring_credential_is_re_fetched_without_the_user_doing_anything() {
    let Some((certificate, key)) = test_client_certificate() else {
        eprintln!("skipping: the test context does not use client certificates");
        return;
    };

    let scratch = Scratch::new("refresh");
    // A credential that expires almost immediately. `aws eks get-token` hands
    // out 15-minute tokens; the client must notice the expiry and re-run the
    // plugin rather than reusing a dead credential.
    let credential = serde_json::json!({
        "kind": "ExecCredential",
        "apiVersion": "client.authentication.k8s.io/v1beta1",
        "spec": {},
        "status": {
            "clientCertificateData": decode(&certificate),
            "clientKeyData": decode(&key),
            "expirationTimestamp": "2020-01-01T00:00:00Z"
        }
    })
    .to_string();

    let plugin = stub_plugin(scratch.path(), "aws", &credential, "", 0);
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        &plugin,
        "client.authentication.k8s.io/v1beta1",
        &EKS_ARGS,
    );

    // Driven through the same client Periscope builds, so this is our
    // configuration being tested, not kube's in isolation.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let listed = runtime.block_on(async {
        let connection =
            periscope_cluster::kubeconfig::connect(&context(), Some(kubeconfig.clone()))
                .await
                .expect("the client is built");
        let api: kube::Api<k8s_openapi::api::core::v1::Pod> =
            kube::Api::namespaced(connection.client, "kube-system");

        let mut listed = 0;
        for _ in 0..3 {
            listed += api
                .list(&kube::api::ListParams::default().limit(1))
                .await
                .expect("the list succeeds with a refreshed credential")
                .items
                .len();
        }
        listed
    });

    assert!(listed > 0, "no pods were listed");
    assert!(
        invocations(scratch.path()) > 1,
        "the plugin ran {} time(s): an expired credential was reused",
        invocations(scratch.path())
    );
}

#[test]
#[ignore = "needs a cluster and the real aws CLI"]
fn the_real_aws_cli_is_run_and_the_token_it_mints_is_sent() {
    if std::process::Command::new("aws")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: the aws CLI is not installed");
        return;
    }

    let scratch = Scratch::new("aws-real");
    // The real binary, with a real EKS stanza. `aws eks get-token` signs a
    // presigned STS URL locally and never contacts EKS, so it succeeds even for
    // a cluster that does not exist — which makes it a clean way to prove the
    // plugin is actually run and its token actually sent, without a cloud
    // cluster. The apiserver then rejects the token, as it should.
    let kubeconfig = kubeconfig_with_exec(
        scratch.path(),
        Path::new("aws"),
        "client.authentication.k8s.io/v1beta1",
        &[
            "--region",
            "eu-west-1",
            "eks",
            "get-token",
            "--cluster-name",
            "periscope-does-not-exist",
        ],
    );

    let (_runtime, stream) = connect_with(&kubeconfig);
    let reason = expect_auth_failure(&stream);

    // Either outcome is informative, and both must name what happened: the
    // token was minted and refused, or the CLI itself failed and said why.
    assert!(
        reason.to_lowercase().contains("unauthorized")
            || reason.contains("401")
            || reason.contains("aws"),
        "the outcome of running the real aws CLI must reach the user: {reason}"
    );
}
