//! Asking the apiserver what it serves.
//!
//! Everything the cluster knows about — built-in kinds and every CRD installed
//! on it — comes from here. Nothing is hard-coded: a cluster with Argo CD and
//! cert-manager lists their kinds because the apiserver said so, not because
//! this code knows what Argo CD is.

use kube::Client;
use kube::discovery::{Discovery, Scope, verbs};
use periscope_bridge::{KindId, KindInfo};

/// API groups that ship with Kubernetes.
///
/// Used only to sort built-ins above custom resources in the picker. The suffix
/// test catches the `*.k8s.io` groups; these are the ones that predate that
/// convention.
const BUILT_IN_GROUPS: [&str; 5] = ["", "apps", "batch", "policy", "autoscaling"];

/// Whether a group is part of Kubernetes itself.
fn is_built_in(group: &str) -> bool {
    BUILT_IN_GROUPS.contains(&group) || group.ends_with("k8s.io")
}

/// Lists every kind the cluster serves.
///
/// Subresources (`pods/log`, `deployments/scale`) are excluded: they are not
/// listable collections and would be noise in a picker. Kinds that cannot be
/// listed *are* included but marked unwatchable, because a kind the user can
/// see in `kubectl api-resources` and not here looks like a bug.
pub async fn kinds(client: Client) -> Result<Vec<KindInfo>, kube::Error> {
    let discovery = Discovery::new(client).run().await?;
    let mut kinds = Vec::new();

    for group in discovery.groups() {
        for (resource, capabilities) in group.recommended_resources() {
            // `ApiGroup` already excludes subresources from this listing, but
            // be explicit: a slash here would build a nonsensical URL.
            if resource.plural.contains('/') {
                continue;
            }

            let listable = capabilities
                .operations
                .iter()
                .any(|verb| verb == verbs::LIST);
            let watchable = listable
                && capabilities
                    .operations
                    .iter()
                    .any(|verb| verb == verbs::WATCH);

            kinds.push(KindInfo {
                id: KindId::new(
                    &resource.group,
                    &resource.version,
                    &resource.kind,
                    &resource.plural,
                ),
                namespaced: capabilities.scope == Scope::Namespaced,
                watchable,
                custom: !is_built_in(&resource.group),
            });
        }
    }

    kinds.sort_by_key(KindInfo::sort_key);
    kinds.dedup_by(|a, b| a.id == b.id);
    Ok(kinds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubernetes_own_groups_are_not_custom_resources() {
        for group in ["", "apps", "batch", "policy", "autoscaling"] {
            assert!(is_built_in(group), "{group}");
        }
        for group in [
            "networking.k8s.io",
            "rbac.authorization.k8s.io",
            "apiextensions.k8s.io",
        ] {
            assert!(is_built_in(group), "{group}");
        }
    }

    #[test]
    fn third_party_groups_are_custom_resources() {
        for group in ["argoproj.io", "cert-manager.io", "example.com"] {
            assert!(!is_built_in(group), "{group}");
        }
    }

    #[test]
    fn a_group_merely_containing_k8s_io_is_not_treated_as_built_in() {
        // `k8s.io.evil.example.com` ends with the *domain*, not the suffix.
        assert!(!is_built_in("k8s.io.example.com"));
        assert!(is_built_in("flowcontrol.apiserver.k8s.io"));
    }
}
