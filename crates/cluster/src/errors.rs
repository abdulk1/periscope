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
    /// Anything else: network, TLS, API errors, malformed config.
    Other(String),
}

impl Failure {
    /// The message, whichever kind of failure this is.
    pub fn message(&self) -> &str {
        match self {
            Self::Auth(message) | Self::Other(message) => message,
        }
    }

    /// Whether this is an authentication problem.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth(_))
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

/// Classifies a client error.
pub fn classify(error: &kube::Error) -> Failure {
    let message = describe(error);

    match error {
        // 401 and 403 are the two the apiserver returns for a credential it
        // will not accept. Everything else is a real API error.
        kube::Error::Api(status) if matches!(status.code, 401 | 403) => Failure::Auth(message),
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
        watcher::Error::WatchError(status) if matches!(status.code, 401 | 403) => {
            Failure::Auth(describe(error))
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
    fn a_forbidden_response_is_an_auth_failure_too() {
        assert!(classify(&api_error(403, "pods is forbidden")).is_auth());
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
