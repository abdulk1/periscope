//! Keeping secrets out of anything that is written down.
//!
//! `AGENTS.md` invariant 3 — tokens and cluster hostnames are redacted from log
//! output — had no implementing code at all until a security review found two
//! places already breaking it. It was held by everybody remembering, across
//! sixty-odd `tracing::` calls, and the audit log's own documentation promised
//! something the code did not do.
//!
//! Two things go wrong on their own, so both are handled here rather than at
//! the call sites:
//!
//! * **Hostnames.** An error from `hyper` names the host it could not reach, so
//!   an ordinary failed request carries the control-plane endpoint — which on
//!   EKS embeds the account id and the region — into the log file.
//! * **Credential material.** `kube`'s `AuthExecRun` carries the failed
//!   plugin's entire `std::process::Output` and `Debug`-prints it. A plugin
//!   that writes its `ExecCredential` JSON to stdout and *then* exits non-zero
//!   — an expired session, a throttled API, a wrapper that appends a warning —
//!   puts a live bearer token in that string.
//!
//! What is on screen is not redacted: the person at the keyboard already has
//! the credentials, and the whole point of this project's error handling is
//! that they see what the API actually said. This is only for what persists.

use std::sync::LazyLock;

use regex::Regex;

/// What replaces a host that has been taken out.
const HOST: &str = "<host redacted>";

/// What replaces something long enough to be a credential.
const SECRET: &str = "<redacted>";

/// A URL with an authority, which is where a hostname hides.
static URLS: LazyLock<Regex> = LazyLock::new(|| {
    // `r#""#` because the character class contains a quote: in a plain raw
    // string a `\"` does not escape, so the literal would end there.
    Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s/\\\]\}\)'"]+"#).expect("valid")
});

/// A run long enough to be a token rather than a word.
///
/// Bearer tokens, JWTs, base64 certificate bodies and service-account tokens
/// all land here. Forty is above anything English writes and below the shortest
/// credential Kubernetes issues.
static SECRETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_\-+/=\.]{40,}").expect("valid"));

/// Removes hostnames and anything token-shaped from text that will be stored.
///
/// Deliberately blunt. A redaction that tries to be clever about which long
/// string is a credential will one day decide wrongly, and the cost of that
/// mistake is unbounded while the cost of over-redacting is a slightly less
/// specific log line.
pub fn text(message: &str) -> String {
    let without_hosts = URLS.replace_all(message, HOST);
    SECRETS.replace_all(&without_hosts, SECRET).into_owned()
}

/// Describes an error for a log or the audit trail, with the parts that must
/// not persist removed.
///
/// Auth failures are handled before [`text`] rather than by it: `kube` puts the
/// plugin's raw output in the message, and recognising it afterwards would mean
/// guessing at a `Debug` format. Matching the variant is exact.
pub fn describe(error: &kube::Error) -> String {
    if let kube::Error::Auth(auth) = error {
        return match auth {
            kube::client::AuthError::AuthExecRun { cmd, status, .. } => format!(
                "the credential plugin `{}` failed with status {status}",
                text(cmd)
            ),
            other => text(&crate::errors::describe(other)),
        };
    }

    text(&crate::errors::describe(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostname_does_not_survive() {
        // The one that was actually happening: an EKS endpoint carries the
        // account id and the region.
        let redacted =
            text("error connecting to https://A1B2C3D4E5F6.gr7.us-east-1.eks.amazonaws.com/api/v1");

        assert!(!redacted.contains("eks.amazonaws.com"), "{redacted}");
        assert!(!redacted.contains("A1B2C3D4E5F6"), "{redacted}");
        assert!(redacted.contains(HOST), "{redacted}");
    }

    #[test]
    fn a_bearer_token_does_not_survive() {
        let token = "eyJhbGciOiJSUzI1NiIsImtpZCI6IkxvbmdFbm91Z2hUb0JlQVJlYWxUb2tlbiJ9";
        let redacted = text(&format!("{{\"token\":\"{token}\"}}"));

        assert!(!redacted.contains(token), "{redacted}");
        assert!(redacted.contains(SECRET), "{redacted}");
    }

    #[test]
    fn a_credential_plugins_output_is_never_described() {
        // `kube` puts the plugin's whole stdout and stderr in this variant's
        // message, and a plugin that prints its ExecCredential and then fails
        // has a live token in there.
        let error = kube::Error::Auth(kube::client::AuthError::AuthExecRun {
            cmd: "aws eks get-token".to_owned(),
            status: std::process::Command::new("false")
                .status()
                .expect("running `false` works"),
            out: std::process::Output {
                status: std::process::Command::new("false")
                    .status()
                    .expect("running `false` works"),
                stdout: br#"{"status":{"token":"k8s-aws-v1.aHR0cHM6Ly9zdHMuYW1hem9uYXdz"}}"#
                    .to_vec(),
                stderr: b"session expired".to_vec(),
            },
        });

        let described = describe(&error);
        // The command is named, because that is what the operator has to go and
        // fix. Nothing the command *said* survives — neither the credential on
        // stdout nor the diagnostic on stderr.
        assert!(described.contains("aws eks get-token"), "{described}");
        assert!(!described.contains("k8s-aws-v1"), "{described}");
        assert!(!described.contains("aHR0cHM"), "{described}");
        assert!(!described.contains("session expired"), "{described}");
    }

    #[test]
    fn ordinary_words_are_left_alone() {
        // Over-redaction is cheap but not free: a message nobody can act on is
        // its own kind of failure.
        let message = "pods \"api-0\" not found: NotFound";
        assert_eq!(text(message), message);
    }

    #[test]
    fn the_object_and_the_reason_survive_a_url_being_removed() {
        let redacted = text(
            "failed to delete deployments.apps payments/api at https://10.0.0.1:6443: 403 Forbidden",
        );

        assert!(redacted.contains("payments/api"), "{redacted}");
        assert!(redacted.contains("403 Forbidden"), "{redacted}");
        assert!(!redacted.contains("10.0.0.1"), "{redacted}");
    }
}
