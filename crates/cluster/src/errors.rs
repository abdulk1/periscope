//! Turning kube's errors into something a user can act on.
//!
//! Two rules from the spec drive this module. The audience wants the real API
//! error text, so nothing is summarised away; and an expired credential must
//! never look like an empty table, so authentication failures are separated
//! from every other kind of failure and surfaced as their own state.

use std::error::Error as StdError;

use kube::runtime::watcher;

/// A failure, split by the only distinction the UI treats differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The credentials were rejected, expired, or could not be obtained.
    Auth(String),
    /// The credentials are fine; this identity may not do this.
    ///
    /// Separate from [`Failure::Auth`] because the blast radius is different.
    /// 401 says the apiserver does not know who you are, which is true of
    /// everything you will ask it next. 403 says it knows exactly who you are
    /// and this particular request is not allowed — the usual case being a role
    /// that grants `pods` and not `secrets`. Treating the second as the first
    /// turns one denied kind into a cluster that appears to have logged you out.
    Forbidden(String),
    /// Anything else: network, TLS, API errors, malformed config.
    Other(String),
}

impl Failure {
    /// The message, whichever kind of failure this is.
    ///
    /// For the screen. Anything written to a file goes through
    /// [`Failure::redacted`] instead.
    pub fn message(&self) -> &str {
        match self {
            Self::Auth(message) | Self::Forbidden(message) | Self::Other(message) => message,
        }
    }

    /// The message with hostnames and anything token-shaped removed.
    ///
    /// The person at the keyboard already holds the credentials, so the full
    /// text is theirs to read; a log file outlives the session and gets shared,
    /// so it gets this one.
    pub fn redacted(&self) -> String {
        crate::redact::text(self.message())
    }

    /// Whether this is an authentication problem.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth(_))
    }

    /// Whether this identity is known and simply not permitted.
    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Forbidden(_))
    }
}

/// Renders an error and everything that caused it.
///
/// `thiserror` only prints the outermost message, and for kube that is often
/// the least useful half: "auth error" without the exec plugin's stderr is
/// exactly the kind of message this project refuses to show.
pub fn describe(error: &dyn StdError) -> String {
    let mut message = error.to_string();
    let mut source = error.source();

    while let Some(cause) = source {
        let text = cause.to_string();
        // kube nests errors that already quote their source; do not repeat it.
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }

    message
}

/// Names the credential plugin in a reason that does not already mention it.
///
/// `kube` reports a plugin that will not start as a bare "No such file or
/// directory", which tells the user nothing about which binary kubeconfig asked
/// for. Every auth failure goes through here so the answer is always in the
/// message, wherever the failure was raised.
pub fn attribute_plugin(reason: String, plugin: Option<&str>) -> String {
    match plugin {
        Some(plugin) if !reason.contains(plugin) => {
            format!("{reason} (credential plugin: `{plugin}`)")
        }
        _ => reason,
    }
}

/// Classifies a client error.
pub fn classify(error: &kube::Error) -> Failure {
    let message = describe(error);

    match error {
        // 401 is the apiserver saying it does not know who this is; 403 is it
        // saying it does and the answer is still no. Only the first is a
        // statement about the credential.
        kube::Error::Api(status) if status.code == 401 => Failure::Auth(message),
        kube::Error::Api(status) if status.code == 403 => Failure::Forbidden(message),
        kube::Error::Auth(_) => Failure::Auth(message),
        _ => Failure::Other(message),
    }
}

/// Classifies a watch-stream error.
pub fn classify_watch(error: &watcher::Error) -> Failure {
    match error {
        watcher::Error::InitialListFailed(inner)
        | watcher::Error::WatchStartFailed(inner)
        | watcher::Error::WatchFailed(inner) => classify(inner),
        watcher::Error::WatchError(status) if status.code == 401 => Failure::Auth(describe(error)),
        watcher::Error::WatchError(status) if status.code == 403 => {
            Failure::Forbidden(describe(error))
        }
        _ => Failure::Other(describe(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::Status;

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status {
            code,
            message: message.to_owned(),
            reason: "Forbidden".to_owned(),
            ..Status::default()
        }))
    }

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("the exec plugin exited 255")]
    struct Inner;

    #[test]
    fn a_rejected_credential_is_an_auth_failure() {
        let failure = classify(&api_error(401, "Unauthorized"));
        assert!(failure.is_auth());
        assert!(failure.message().contains("Unauthorized"), "{failure:?}");
    }

    #[test]
    fn a_forbidden_response_is_about_the_request_not_the_credential() {
        // The incident this prevents: a role granting `pods` and not `secrets`
        // opened the Secrets table, the 403 was reported as the credential
        // failing, and every watch on the cluster stopped.
        let failure = classify(&api_error(403, "secrets is forbidden"));

        assert!(failure.is_forbidden());
        assert!(!failure.is_auth(), "403 must not condemn the credential");
        assert!(failure.message().contains("forbidden"), "{failure:?}");
    }

    #[test]
    fn an_ordinary_api_error_is_not_an_auth_failure() {
        let failure = classify(&api_error(500, "internal server error"));
        assert!(!failure.is_auth());
        assert!(failure.message().contains("internal server error"));
    }

    #[test]
    fn a_failed_watch_carries_the_api_error_through() {
        let failure = classify_watch(&watcher::Error::WatchStartFailed(api_error(
            401,
            "token expired",
        )));
        assert!(failure.is_auth());
        assert!(failure.message().contains("token expired"), "{failure:?}");
    }

    #[test]
    fn an_auth_failure_is_attributed_to_the_plugin_that_caused_it() {
        assert_eq!(
            attribute_plugin("unable to run auth exec: No such file".into(), Some("aws")),
            "unable to run auth exec: No such file (credential plugin: `aws`)"
        );
    }

    #[test]
    fn a_reason_that_already_names_the_plugin_is_left_alone() {
        let reason = "auth exec command 'aws' failed".to_owned();
        assert_eq!(attribute_plugin(reason.clone(), Some("aws")), reason);
        assert_eq!(attribute_plugin(reason.clone(), None), reason);
    }

    #[test]
    fn describe_walks_the_whole_cause_chain() {
        // Without this the user would see "outer" and have no idea why.
        assert_eq!(describe(&Outer(Inner)), "outer: the exec plugin exited 255");
    }

    #[test]
    fn describe_does_not_repeat_a_cause_the_outer_message_already_quotes() {
        #[derive(Debug, thiserror::Error)]
        #[error("wrapper: {0}")]
        struct Quoting(#[source] Inner);

        assert_eq!(
            describe(&Quoting(Inner)),
            "wrapper: the exec plugin exited 255"
        );
    }
}
