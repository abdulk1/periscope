//! Asking the apiserver what it serves.
//!
//! Everything the cluster knows about — built-in kinds and every CRD installed
//! on it — comes from here. Nothing is hard-coded: a cluster with Argo CD and
//! cert-manager lists their kinds because the apiserver said so, not because
//! this code knows what Argo CD is.
//!
//! Discovery answers two questions. What kinds are there, which comes from the
//! discovery endpoints; and, for the kinds that come from a
//! CustomResourceDefinition, what columns their authors want printed, which
//! comes from the CRDs themselves.

use std::collections::HashMap;
use std::sync::Arc;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{Api, ListParams};
use kube::discovery::{Discovery, Scope, verbs};
use kube::{Client, ResourceExt as _};
use periscope_bridge::{KindId, KindInfo};

use crate::printer::{self, PrinterColumn};

/// API groups that ship with Kubernetes.
///
/// Used only to sort built-ins above custom resources in the picker. The suffix
/// test catches the `*.k8s.io` groups; these are the ones that predate that
/// convention.
const BUILT_IN_GROUPS: [&str; 5] = ["", "apps", "batch", "policy", "autoscaling"];

/// How many CustomResourceDefinitions are fetched at a time.
///
/// A CRD carries its whole OpenAPI schema, and cert-manager's alone are a
/// megabyte; a cluster with a service mesh and an operator or two has hundreds.
/// Only four fields of each are wanted, so the list is paged and each page is
/// reduced to those fields and dropped, rather than holding every schema on the
/// cluster in memory at once to read a handful of strings out of them.
const CRD_PAGE: u32 = 25;

/// Whether a group is part of Kubernetes itself.
fn is_built_in(group: &str) -> bool {
    BUILT_IN_GROUPS.contains(&group) || group.ends_with("k8s.io")
}

/// What a cluster serves, and how its custom resources want to be printed.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// Every kind the apiserver reported.
    pub kinds: Vec<KindInfo>,
    /// The printer columns declared by the CRDs among them. Kinds that declare
    /// none are absent rather than present and empty.
    printer_columns: HashMap<KindId, Arc<[PrinterColumn]>>,
}

impl Catalog {
    /// The printer columns a kind's CRD declared, empty when it declared none.
    pub fn printer_columns(&self, kind: &KindId) -> Arc<[PrinterColumn]> {
        self.printer_columns
            .get(kind)
            .cloned()
            .unwrap_or_else(|| Arc::from([]))
    }
}

/// Lists every kind the cluster serves, and reads the CRDs among them.
///
/// Subresources (`pods/log`, `deployments/scale`) are excluded: they are not
/// listable collections and would be noise in a picker. Kinds that cannot be
/// listed *are* included but marked unwatchable, because a kind the user can
/// see in `kubectl api-resources` and not here looks like a bug.
///
/// Only the kind listing can fail the whole call. A CRD that cannot be read —
/// RBAC very often forbids `customresourcedefinitions`, and a cluster that
/// serves custom resources it will not describe is perfectly normal — costs
/// that kind its declared columns and nothing else.
pub async fn catalog(client: Client) -> Result<Catalog, kube::Error> {
    let discovery = Discovery::new(client.clone()).run().await?;
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

    let printer_columns = printer_columns(client, &kinds).await;
    Ok(Catalog {
        kinds,
        printer_columns,
    })
}

/// Reads the printer columns declared by the CRDs behind the custom kinds.
///
/// Never fails: a cluster whose CRDs cannot be read still lists everything, on
/// the generic columns it listed on before any of this existed. The reason is
/// logged with the failure text intact, so "why does my CRD not show its own
/// columns" has an answer in the log rather than only in a support thread.
async fn printer_columns(
    client: Client,
    kinds: &[KindInfo],
) -> HashMap<KindId, Arc<[PrinterColumn]>> {
    // The CRD names of the kinds actually on offer, so a CRD for a version the
    // apiserver no longer serves is read and discarded rather than watched for.
    let custom: HashMap<String, Vec<&KindInfo>> =
        kinds
            .iter()
            .filter(|info| info.custom)
            .fold(HashMap::new(), |mut wanted, info| {
                wanted
                    .entry(format!("{}.{}", info.id.plural, info.id.group))
                    .or_default()
                    .push(info);
                wanted
            });

    if custom.is_empty() {
        return HashMap::new();
    }

    let api: Api<CustomResourceDefinition> = Api::all(client);
    let mut columns = HashMap::new();
    let mut params = ListParams::default().limit(CRD_PAGE);

    loop {
        let page = match api.list(&params).await {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(
                    kinds = custom.len(),
                    reason = crate::errors::describe(&error),
                    "could not read CustomResourceDefinitions; \
                     custom resources will list on the generic columns"
                );
                return columns;
            }
        };
        let next = page.metadata.continue_.clone();

        for crd in &page.items {
            let Some(wanted) = custom.get(&crd.name_any()) else {
                continue;
            };

            for info in wanted {
                let declared = printer::declared(crd, &info.id.version);
                let compiled = printer::compile(&info.id, &declared);
                if !compiled.is_empty() {
                    columns.insert(info.id.clone(), compiled);
                }
            }
        }

        match next {
            Some(token) if !token.is_empty() => {
                params = ListParams::default().limit(CRD_PAGE).continue_token(&token);
            }
            _ => break,
        }
    }

    tracing::debug!(
        kinds = columns.len(),
        "custom resources rendering their own printer columns"
    );
    columns
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

    #[test]
    fn a_kind_no_crd_described_has_no_printer_columns() {
        // The path every built-in takes, and the one a CRD takes when reading
        // it was forbidden.
        let catalog = Catalog::default();
        assert!(
            catalog
                .printer_columns(&KindId::new("", "v1", "Pod", "pods"))
                .is_empty()
        );
    }
}
