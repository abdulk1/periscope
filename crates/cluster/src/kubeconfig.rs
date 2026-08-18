//! Reading kubeconfig and building clients from it.
//!
//! `kube` already implements the parts that are easy to get wrong — `KUBECONFIG`
//! with several files merged, relative certificate paths, exec credential
//! plugins, OIDC refresh — so this module is a thin, testable layer over it that
//! speaks Periscope's own types.
//!
//! Nothing here writes to disk. Credentials that `kube` obtains live in the
//! client for as long as the connection does and are never persisted.

use kube::config::{KubeConfigOptions, Kubeconfig, KubeconfigError};
use kube::{Client, Config};
use periscope_bridge::{ClusterId, ContextInfo};

use crate::errors::{Failure, classify, describe};

/// What kubeconfig says about the clusters available.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contexts {
    /// Every context, in the order kubeconfig lists them.
    pub contexts: Vec<ContextInfo>,
    /// The `current-context`, when there is one and it names a real context.
    pub current: Option<ClusterId>,
}

/// Why connecting to a context failed.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Kubeconfig could not be read, parsed, or did not contain the context.
    #[error("kubeconfig: {0}")]
    Kubeconfig(#[from] KubeconfigError),
    /// The client could not be constructed — TLS, proxy, or auth setup.
    #[error("{0}")]
    Client(#[from] kube::Error),
}

impl ConnectError {
    /// How the UI should present this, with the full error text preserved.
    pub fn failure(&self) -> Failure {
        match self {
            Self::Client(error) => classify(error),
            // A kubeconfig that cannot be read is a configuration problem, not
            // a rejected credential, even when it is an auth stanza that is
            // malformed. The distinction matters: one is fixed by re-running a
            // login, the other by editing a file.
            Self::Kubeconfig(_) => Failure::Other(describe(self)),
        }
    }
}

/// Where to read kubeconfig from.
///
/// `None` means the standard search: `$KUBECONFIG` (several files, merged), or
/// `~/.kube/config` when it is unset. A path overrides both, which is what
/// `--kubeconfig` sets and what lets the auth paths be tested against a real
/// apiserver without touching the user's own config.
pub type Source = Option<std::path::PathBuf>;

/// Loads and parses kubeconfig.
///
/// This touches the filesystem, so callers on the tokio runtime must run it
/// through `spawn_blocking`.
fn load(source: &Source) -> Result<Kubeconfig, KubeconfigError> {
    match source {
        Some(path) => Kubeconfig::read_from(path),
        None => Kubeconfig::read(),
    }
}

/// Reads the contexts kubeconfig defines.
pub fn read(source: &Source) -> Result<Contexts, KubeconfigError> {
    Ok(contexts_of(&load(source)?))
}

/// Projects a parsed kubeconfig into the context list the UI renders.
fn contexts_of(kubeconfig: &Kubeconfig) -> Contexts {
    let contexts: Vec<ContextInfo> = kubeconfig
        .contexts
        .iter()
        .map(|named| {
            let context = named.context.as_ref();
            ContextInfo {
                name: named.name.as_str().into(),
                cluster: context
                    .map(|c| c.cluster.as_str())
                    .unwrap_or_default()
                    .into(),
                user: context
                    .and_then(|c| c.user.as_deref())
                    .filter(|user| !user.is_empty())
                    .map(Into::into),
                namespace: context
                    .and_then(|c| c.namespace.as_deref())
                    .filter(|namespace| !namespace.is_empty())
                    .map(Into::into),
            }
        })
        .collect();

    // A `current-context` naming a context that does not exist is a broken
    // kubeconfig; treat it as unset rather than offering to connect to nothing.
    let current = kubeconfig
        .current_context
        .as_deref()
        .filter(|name| contexts.iter().any(|context| &*context.name == *name))
        .map(ClusterId::new);

    Contexts { contexts, current }
}

/// Builds a client for one kubeconfig context.
///
/// Running exec credential plugins and OIDC refreshes happens here, which is
/// why this is async and can take seconds on a cold token.
pub async fn connect(context: &ClusterId, source: Source) -> Result<Client, ConnectError> {
    let name = context.to_string();
    let kubeconfig = tokio::task::spawn_blocking(move || load(&source))
        .await
        .map_err(|join| {
            // The only way this fails is a panic inside the read.
            KubeconfigError::ReadConfig(
                std::io::Error::other(join.to_string()),
                std::path::PathBuf::new(),
            )
        })??;

    let options = KubeConfigOptions {
        context: Some(name),
        ..KubeConfigOptions::default()
    };
    let config = Config::from_custom_kubeconfig(kubeconfig, &options).await?;

    tracing::info!(
        cluster = %context,
        server = %config.cluster_url,
        namespace = %config.default_namespace,
        "building client"
    );

    Ok(Client::try_from(config)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod-cluster
    cluster:
      server: https://prod.example:6443
  - name: kind
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: prod
    context:
      cluster: prod-cluster
      user: prod-admin
      namespace: payments
  - name: kind-local
    context:
      cluster: kind
users:
  - name: prod-admin
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: aws
        args: ["eks", "get-token"]
"#;

    fn sample() -> Kubeconfig {
        Kubeconfig::from_yaml(SAMPLE).expect("fixture parses")
    }

    #[test]
    fn contexts_keep_kubeconfig_order() {
        let contexts = contexts_of(&sample());
        let names: Vec<_> = contexts.contexts.iter().map(|c| &*c.name).collect();
        assert_eq!(names, ["prod", "kind-local"]);
    }

    #[test]
    fn a_context_carries_its_cluster_user_and_namespace() {
        let contexts = contexts_of(&sample());
        let prod = &contexts.contexts[0];

        assert_eq!(&*prod.cluster, "prod-cluster");
        assert_eq!(prod.user.as_deref(), Some("prod-admin"));
        assert_eq!(prod.namespace.as_deref(), Some("payments"));
    }

    #[test]
    fn optional_fields_are_none_rather_than_empty_strings() {
        let contexts = contexts_of(&sample());
        let kind = &contexts.contexts[1];

        assert_eq!(kind.user, None);
        assert_eq!(kind.namespace, None);
    }

    #[test]
    fn the_current_context_is_reported() {
        assert_eq!(contexts_of(&sample()).current, Some(ClusterId::new("prod")));
    }

    #[test]
    fn a_current_context_that_does_not_exist_is_treated_as_unset() {
        let mut kubeconfig = sample();
        kubeconfig.current_context = Some("deleted-last-week".to_owned());
        // Offering to connect to a context that is not there would produce an
        // error the user cannot explain; showing nothing selected is honest.
        assert_eq!(contexts_of(&kubeconfig).current, None);
    }

    #[test]
    fn an_empty_kubeconfig_yields_no_contexts_and_no_current() {
        let empty = Kubeconfig::from_yaml("apiVersion: v1\nkind: Config\n").expect("parses");
        assert_eq!(contexts_of(&empty), Contexts::default());
    }
}
