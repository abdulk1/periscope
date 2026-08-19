//! Grouping kinds into the sections a sidebar shows.
//!
//! A cluster with cert-manager and Argo CD installed serves seventy-seven
//! kinds. As one alphabetical list that is a wall: `certificaterequests` sits
//! between `bindings` and `clusterrolebindings`, and the six kinds anybody
//! actually opens are scattered through it. Every other console — Rancher, the
//! EKS console, Lens — groups them, and they all group them roughly the same
//! way, because the grouping follows what the objects *are*.
//!
//! The rules are here, in the store, rather than in the view: which section a
//! kind belongs to is a fact about the kind, and this way it is testable
//! without a window.

use std::collections::BTreeMap;

use periscope_bridge::{KindId, KindInfo};

/// A section of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Things that run: pods and the controllers that make them.
    Workloads,
    /// Things that route: services, ingresses, network policy.
    Networking,
    /// Things that configure: config maps, secrets, quotas.
    Configuration,
    /// Things that persist: claims, volumes, storage classes.
    Storage,
    /// Things that permit: service accounts, roles, bindings.
    AccessControl,
    /// Things that describe the cluster itself: nodes, namespaces, events.
    Cluster,
    /// One section per custom API group, named after the group.
    Custom(String),
    /// Everything that matched nothing above. Never hidden — a kind the rules
    /// do not know about must still be reachable.
    Other,
}

impl Category {
    /// The heading the sidebar shows.
    pub fn title(&self) -> String {
        match self {
            Self::Workloads => "WORKLOADS".to_owned(),
            Self::Networking => "NETWORKING".to_owned(),
            Self::Configuration => "CONFIGURATION".to_owned(),
            Self::Storage => "STORAGE".to_owned(),
            Self::AccessControl => "ACCESS CONTROL".to_owned(),
            Self::Cluster => "CLUSTER".to_owned(),
            Self::Custom(group) => group.to_uppercase(),
            Self::Other => "OTHER".to_owned(),
        }
    }

    /// Where the section sits. Workloads first, because that is what people
    /// open; custom groups after the built-ins, alphabetically among
    /// themselves; `Other` last, because it is the leftovers.
    fn rank(&self) -> (u8, &str) {
        match self {
            Self::Workloads => (0, ""),
            Self::Networking => (1, ""),
            Self::Configuration => (2, ""),
            Self::Storage => (3, ""),
            Self::AccessControl => (4, ""),
            Self::Cluster => (5, ""),
            Self::Custom(group) => (6, group.as_str()),
            Self::Other => (7, ""),
        }
    }

    /// Whether this section starts open.
    ///
    /// Only workloads. Opening everything would reproduce the wall this exists
    /// to remove, and workloads is where all but a few sessions begin.
    pub fn open_by_default(&self) -> bool {
        matches!(self, Self::Workloads)
    }

    /// Which section a kind belongs to.
    pub fn of(info: &KindInfo) -> Self {
        // A custom resource goes under its own API group, whatever it is called:
        // somebody who installed cert-manager looks for `certificates` under
        // cert-manager.io, not under a guess at what the objects do.
        if info.custom {
            return Self::Custom(info.id.group.to_string());
        }

        Self::of_id(&info.id)
    }

    /// The rules, by API group and then by kind.
    fn of_id(id: &KindId) -> Self {
        match &*id.group {
            "apps" | "batch" | "autoscaling" => return Self::Workloads,
            "networking.k8s.io" | "discovery.k8s.io" | "gateway.networking.k8s.io" => {
                return Self::Networking;
            }
            "storage.k8s.io" => return Self::Storage,
            "rbac.authorization.k8s.io" => return Self::AccessControl,
            "policy" => return Self::Configuration,
            "apiextensions.k8s.io"
            | "apiregistration.k8s.io"
            | "certificates.k8s.io"
            | "coordination.k8s.io"
            | "node.k8s.io"
            | "scheduling.k8s.io" => {
                return Self::Cluster;
            }
            _ => {}
        }

        if !id.is_core() {
            return Self::Other;
        }

        match &*id.plural {
            "pods" | "replicationcontrollers" | "podtemplates" => Self::Workloads,
            "services" | "endpoints" => Self::Networking,
            "configmaps" | "secrets" | "resourcequotas" | "limitranges" => Self::Configuration,
            "persistentvolumeclaims" | "persistentvolumes" => Self::Storage,
            "serviceaccounts" => Self::AccessControl,
            "nodes" | "namespaces" | "events" | "componentstatuses" => Self::Cluster,
            _ => Self::Other,
        }
    }
}

/// Kinds arranged into sections, in the order a sidebar renders them.
///
/// Only watchable kinds are included: a kind that cannot be listed cannot be
/// opened, and offering it would be an invitation to a dead end.
pub fn sections(kinds: &[KindInfo]) -> Vec<(Category, Vec<KindInfo>)> {
    let mut grouped: BTreeMap<(u8, String), (Category, Vec<KindInfo>)> = BTreeMap::new();

    for info in kinds.iter().filter(|info| info.watchable) {
        let category = Category::of(info);
        let (rank, name) = category.rank();
        grouped
            .entry((rank, name.to_owned()))
            .or_insert_with(|| (category, Vec::new()))
            .1
            .push(info.clone());
    }

    let mut sections: Vec<(Category, Vec<KindInfo>)> = grouped.into_values().collect();
    for (_, kinds) in &mut sections {
        kinds.sort_by_key(KindInfo::sort_key);
    }
    sections
}

/// How many recently opened kinds the sidebar offers.
///
/// Short on purpose. A recents list long enough to need scrolling is a second
/// copy of the thing sections exist to make navigable.
pub const RECENT_SHOWN: usize = 6;

/// The recently opened kinds a cluster still serves, most recent first.
///
/// Recency is decided elsewhere — this only turns remembered names back into
/// kinds. Everything the cluster no longer serves, or never did, is dropped
/// silently: a cluster's recents are only meaningful on that cluster, and an
/// entry that cannot be opened is worse than a shorter list.
pub fn recent<'a>(kinds: &[KindInfo], visited: impl IntoIterator<Item = &'a str>) -> Vec<KindInfo> {
    visited
        .into_iter()
        .filter_map(|label| {
            kinds
                .iter()
                .find(|info| info.watchable && info.id.label() == label)
        })
        .take(RECENT_SHOWN)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(group: &str, plural: &str) -> KindInfo {
        KindInfo {
            id: KindId::new(group, "v1", plural, plural),
            namespaced: true,
            watchable: true,
            custom: false,
        }
    }

    fn custom(group: &str, plural: &str) -> KindInfo {
        KindInfo {
            custom: true,
            ..kind(group, plural)
        }
    }

    fn category(group: &str, plural: &str) -> Category {
        Category::of(&kind(group, plural))
    }

    #[test]
    fn workloads_are_the_things_that_run() {
        assert_eq!(category("", "pods"), Category::Workloads);
        assert_eq!(category("apps", "deployments"), Category::Workloads);
        assert_eq!(category("apps", "statefulsets"), Category::Workloads);
        assert_eq!(category("batch", "cronjobs"), Category::Workloads);
        // Autoscaling belongs with what it scales, not in a section of its own.
        assert_eq!(
            category("autoscaling", "horizontalpodautoscalers"),
            Category::Workloads
        );
    }

    #[test]
    fn the_other_built_in_sections_hold_what_they_say() {
        assert_eq!(category("", "services"), Category::Networking);
        assert_eq!(
            category("networking.k8s.io", "ingresses"),
            Category::Networking
        );
        assert_eq!(category("", "configmaps"), Category::Configuration);
        assert_eq!(category("", "secrets"), Category::Configuration);
        assert_eq!(
            category("policy", "poddisruptionbudgets"),
            Category::Configuration
        );
        assert_eq!(category("", "persistentvolumes"), Category::Storage);
        assert_eq!(
            category("storage.k8s.io", "storageclasses"),
            Category::Storage
        );
        assert_eq!(category("", "serviceaccounts"), Category::AccessControl);
        assert_eq!(
            category("rbac.authorization.k8s.io", "clusterroles"),
            Category::AccessControl
        );
        assert_eq!(category("", "nodes"), Category::Cluster);
        assert_eq!(category("", "namespaces"), Category::Cluster);
        assert_eq!(category("", "events"), Category::Cluster);
    }

    #[test]
    fn a_custom_resource_goes_under_the_group_that_installed_it() {
        // Not under a guess at what it does: somebody who installed cert-manager
        // looks under cert-manager.io.
        assert_eq!(
            Category::of(&custom("cert-manager.io", "certificates")),
            Category::Custom("cert-manager.io".to_owned())
        );
        assert_eq!(
            Category::of(&custom("argoproj.io", "applications")).title(),
            "ARGOPROJ.IO"
        );
    }

    #[test]
    fn an_unknown_built_in_kind_is_reachable_rather_than_hidden() {
        // A kind the rules have never heard of must still appear somewhere.
        assert_eq!(category("", "bindings"), Category::Other);
        assert_eq!(
            category("flowcontrol.apiserver.k8s.io", "prioritylevels"),
            Category::Other
        );
    }

    #[test]
    fn sections_come_out_in_reading_order() {
        let kinds = vec![
            custom("cert-manager.io", "certificates"),
            kind("", "nodes"),
            kind("apps", "deployments"),
            custom("argoproj.io", "applications"),
            kind("", "services"),
            kind("", "bindings"),
        ];

        let order: Vec<String> = sections(&kinds)
            .into_iter()
            .map(|(category, _)| category.title())
            .collect();

        assert_eq!(
            order,
            [
                "WORKLOADS",
                "NETWORKING",
                "CLUSTER",
                "ARGOPROJ.IO",
                "CERT-MANAGER.IO",
                "OTHER"
            ]
        );
    }

    #[test]
    fn only_workloads_starts_open() {
        assert!(Category::Workloads.open_by_default());
        for category in [
            Category::Networking,
            Category::Storage,
            Category::Cluster,
            Category::Custom("cert-manager.io".to_owned()),
            Category::Other,
        ] {
            assert!(!category.open_by_default(), "{category:?} starts open");
        }
    }

    #[test]
    fn kinds_inside_a_section_are_sorted() {
        let kinds = vec![
            kind("apps", "statefulsets"),
            kind("", "pods"),
            kind("apps", "deployments"),
        ];

        let (_, workloads) = sections(&kinds).into_iter().next().expect("a section");
        let names: Vec<String> = workloads.iter().map(|info| info.id.label()).collect();

        assert_eq!(names, ["deployments.apps", "pods", "statefulsets.apps"]);
    }

    #[test]
    fn recents_come_back_in_the_order_they_were_visited() {
        let kinds = vec![
            kind("", "pods"),
            kind("apps", "deployments"),
            custom("cert-manager.io", "certificates"),
        ];

        let recent = recent(
            &kinds,
            ["certificates.cert-manager.io", "pods", "deployments.apps"],
        );
        let names: Vec<String> = recent.iter().map(|info| info.id.label()).collect();

        assert_eq!(
            names,
            ["certificates.cert-manager.io", "pods", "deployments.apps"]
        );
    }

    #[test]
    fn a_recent_kind_this_cluster_does_not_serve_is_dropped() {
        // Switching to a cluster without cert-manager must not offer a jump
        // that lands nowhere.
        let kinds = vec![kind("", "pods")];

        let recent = recent(&kinds, ["certificates.cert-manager.io", "pods"]);
        let names: Vec<String> = recent.iter().map(|info| info.id.label()).collect();

        assert_eq!(names, ["pods"]);
    }

    #[test]
    fn a_recent_kind_that_cannot_be_watched_is_not_offered_either() {
        let kinds = vec![KindInfo {
            watchable: false,
            ..kind("", "componentstatuses")
        }];

        assert!(recent(&kinds, ["componentstatuses"]).is_empty());
    }

    #[test]
    fn the_recent_list_is_short_however_much_is_remembered() {
        let kinds: Vec<KindInfo> = (0..RECENT_SHOWN * 2)
            .map(|index| kind("", &format!("kind{index}")))
            .collect();
        let labels: Vec<String> = kinds.iter().map(|info| info.id.label()).collect();

        let recent = recent(&kinds, labels.iter().map(String::as_str));
        assert_eq!(recent.len(), RECENT_SHOWN);
    }

    #[test]
    fn a_kind_that_cannot_be_watched_is_not_offered() {
        // Opening it would be a dead end; the palette still finds it.
        let kinds = vec![KindInfo {
            watchable: false,
            ..kind("", "componentstatuses")
        }];

        assert!(sections(&kinds).is_empty());
    }
}
