//! The opt-in update check.
//!
//! One HTTPS GET, made once at startup, only when `updates.check` is set. It
//! downloads nothing, installs nothing, and sends nothing about the machine it
//! runs on beyond the User-Agent GitHub requires of every caller.
//!
//! It runs on the tokio runtime the cluster layer already owns — `reqwest`
//! needs a tokio reactor and GPUI's executors are not one — and answers through
//! a oneshot the UI awaits. Whether the answer is *news* is decided by
//! [`periscope_config::updates`], which is pure and tested; there is nothing
//! here worth testing but the request itself.

use std::time::Duration;

use futures::channel::oneshot;
use periscope_config::{Release, Updates, Version};

/// How long to wait before giving up.
///
/// Short on purpose: nobody is waiting for this, and a check that hangs must
/// not keep a runtime task alive for the life of the session.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What GitHub wants callers to identify themselves as. It carries the version
/// and nothing else — no machine, no user, no cluster.
fn user_agent() -> String {
    format!("periscope/{}", env!("CARGO_PKG_VERSION"))
}

/// Starts a check, if one was asked for.
///
/// The receiver yields at most one answer: a newer release, or nothing. Errors
/// are logged rather than returned — a failed update check is not something to
/// interrupt anybody about.
pub fn start(
    handle: &tokio::runtime::Handle,
    updates: &Updates,
    current: Version,
) -> Option<oneshot::Receiver<Release>> {
    let endpoint = updates.target()?.to_owned();
    let (sender, receiver) = oneshot::channel();

    handle.spawn(async move {
        match fetch(&endpoint).await {
            Ok(payload) => match periscope_config::updates::newer_than(current, &payload) {
                Ok(Some(release)) => {
                    tracing::info!(version = %release.version, "a newer version is available");
                    let _ = sender.send(release);
                }
                Ok(None) => tracing::debug!(%current, "already up to date"),
                Err(reason) => tracing::warn!(%reason, "could not read the update check's answer"),
            },
            // Being offline, or behind a proxy that refuses this, is not an
            // error the user needs to see.
            Err(reason) => tracing::info!(%reason, "the update check did not complete"),
        }
    });

    Some(receiver)
}

/// Fetches the endpoint, returning its body.
async fn fetch(endpoint: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(TIMEOUT)
        // The endpoint is checked for `https` before this runs; refusing to
        // follow a redirect off it keeps that guarantee through the redirect.
        .https_only(true)
        .build()
        .map_err(|error| error.to_string())?;

    let response = client
        .get(endpoint)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if !response.status().is_success() {
        return Err(format!("{} answered {}", endpoint, response.status()));
    }

    response.text().await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_identifies_the_program_and_nothing_else() {
        let agent = user_agent();

        assert!(agent.starts_with("periscope/"), "{agent}");
        assert!(!agent.contains(' '), "{agent}");
    }

    #[test]
    fn nothing_is_requested_when_the_check_is_off() {
        // The default, and the promise the security posture makes.
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let started = start(runtime.handle(), &Updates::default(), Version::current());

        assert!(started.is_none());
    }

    #[test]
    fn nothing_is_requested_for_a_plain_http_endpoint() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let updates = Updates {
            check: true,
            endpoint: "http://example.invalid/latest".to_owned(),
        };

        assert!(start(runtime.handle(), &updates, Version::current()).is_none());
    }
}
