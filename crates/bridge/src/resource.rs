//! Cluster data as the UI needs it.
//!
//! These are *projections*, not Kubernetes objects. The cluster layer converts
//! `k8s-openapi` types into these on the tokio side, which keeps `k8s-openapi`
//! out of the store and the UI entirely and means the whole rendering path can
//! be tested without kube. Phase 2 adds the raw object alongside, when detail
//! views need the full YAML.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Identifies an object within a cluster.
///
/// Ordered by namespace then name, which is the order the table renders in, so
/// a `BTreeMap` keyed by this needs no separate sort.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceKey {
    /// Namespace the object lives in.
    pub namespace: Arc<str>,
    /// Object name, unique within the namespace for a given kind.
    pub name: Arc<str>,
}

impl ResourceKey {
    /// Builds a key from anything string-shaped.
    pub fn new(namespace: impl AsRef<str>, name: impl AsRef<str>) -> Self {
        Self {
            namespace: Arc::from(namespace.as_ref()),
            name: Arc::from(name.as_ref()),
        }
    }
}

impl std::fmt::Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// One kubeconfig context, as offered in the cluster picker.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContextInfo {
    /// Context name. This is also the [`crate::ClusterId`].
    pub name: Arc<str>,
    /// The cluster entry the context points at.
    pub cluster: Arc<str>,
    /// The user entry the context authenticates as, when it names one.
    pub user: Option<Arc<str>>,
    /// The context's default namespace, when it sets one.
    pub namespace: Option<Arc<str>>,
}

/// A pod, reduced to the columns the table renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodSnapshot {
    /// Namespace and name.
    pub key: ResourceKey,
    /// `metadata.uid`, which distinguishes a replacement pod from an update to
    /// the same one.
    pub uid: Option<Arc<str>>,
    /// The status column: phase, or the more specific reason when there is one
    /// (`CrashLoopBackOff`, `Init:1/3`, `Terminating`, ...).
    pub status: Arc<str>,
    /// Containers currently ready.
    pub ready: u32,
    /// Containers in the pod.
    pub containers: u32,
    /// Total restarts across containers.
    pub restarts: u32,
    /// Node the pod is scheduled on, once it is scheduled.
    pub node: Option<Arc<str>>,
    /// `metadata.creationTimestamp`, for the age column.
    pub created: Option<SystemTime>,
}

impl PodSnapshot {
    /// Whether every container in the pod is ready.
    pub fn is_ready(&self) -> bool {
        self.containers > 0 && self.ready == self.containers
    }

    /// How long the pod has existed, as of `now`.
    ///
    /// `None` when the object carried no creation timestamp; clocks that
    /// disagree yield [`Duration::ZERO`] rather than a negative age.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        self.created
            .map(|created| now.duration_since(created).unwrap_or(Duration::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod() -> PodSnapshot {
        PodSnapshot {
            key: ResourceKey::new("default", "api-0"),
            uid: None,
            status: Arc::from("Running"),
            ready: 1,
            containers: 2,
            restarts: 0,
            node: None,
            created: None,
        }
    }

    #[test]
    fn keys_sort_by_namespace_then_name() {
        let mut keys = [
            ResourceKey::new("kube-system", "coredns"),
            ResourceKey::new("default", "web"),
            ResourceKey::new("default", "api"),
        ];
        keys.sort();

        let rendered: Vec<_> = keys.iter().map(ResourceKey::to_string).collect();
        assert_eq!(
            rendered,
            ["default/api", "default/web", "kube-system/coredns"]
        );
    }

    #[test]
    fn readiness_needs_every_container() {
        let mut pod = pod();
        assert!(!pod.is_ready());
        pod.ready = 2;
        assert!(pod.is_ready());
    }

    #[test]
    fn a_pod_with_no_containers_is_not_ready() {
        let mut pod = pod();
        pod.ready = 0;
        pod.containers = 0;
        assert!(!pod.is_ready());
    }

    #[test]
    fn age_is_none_without_a_creation_timestamp() {
        assert_eq!(pod().age(SystemTime::now()), None);
    }

    #[test]
    fn a_clock_running_backwards_does_not_produce_a_negative_age() {
        let now = SystemTime::now();
        let mut pod = pod();
        pod.created = Some(now + Duration::from_secs(60));
        assert_eq!(pod.age(now), Some(Duration::ZERO));
    }
}
