//! Who may change what.
//!
//! The acceptance criterion is specific: *read-only contexts reject mutations
//! at the store layer*. Not in the UI, where a missing `if` hides a button but
//! leaves the code path open; not only in the cluster layer, which is too far
//! down to tell the user anything useful. Here — where the app decides what is
//! true — and again in the cluster layer before the request goes out
//! (`docs/DECISIONS.md` ADR-0028).
//!
//! The policy itself comes from settings; this module is only the decision and
//! the reason, both of which are testable without a cluster or a window.

use std::collections::BTreeSet;

use periscope_bridge::{ClusterId, ExecTarget, Mutation};

/// Why a mutation was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The cluster is marked read-only in settings.
    ReadOnly {
        /// Which cluster.
        cluster: ClusterId,
    },
    /// The app has not connected to that cluster, so there is nothing to act
    /// on.
    NotConnected {
        /// Which cluster.
        cluster: ClusterId,
    },
}

impl Refusal {
    /// What the user is told, verbatim.
    pub fn reason(&self) -> String {
        match self {
            Self::ReadOnly { cluster } => format!(
                "{cluster} is read-only. Remove it from `read-only` in settings.toml to allow \
                 changes."
            ),
            Self::NotConnected { cluster } => {
                format!("Not connected to {cluster}.")
            }
        }
    }
}

/// Which clusters may be changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Permissions {
    read_only: BTreeSet<ClusterId>,
    read_only_by_default: bool,
    writable: BTreeSet<ClusterId>,
}

impl Permissions {
    /// Permissions that allow every cluster to be changed.
    ///
    /// This is the default because Periscope's *own* default is to have no
    /// opinion; the app is only as dangerous as the credentials it was given,
    /// and pretending otherwise would train people to ignore the setting.
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Permissions built from settings.
    pub fn from_access(access: &periscope_config::Access) -> Self {
        Self {
            read_only: access.read_only.iter().map(ClusterId::new).collect(),
            read_only_by_default: access.read_only_by_default,
            writable: access.writable.iter().map(ClusterId::new).collect(),
        }
    }

    /// Permissions that refuse everything except the named clusters.
    pub fn read_only_except(writable: impl IntoIterator<Item = ClusterId>) -> Self {
        Self {
            read_only: BTreeSet::new(),
            read_only_by_default: true,
            writable: writable.into_iter().collect(),
        }
    }

    /// Marks a cluster read-only.
    pub fn deny(&mut self, cluster: ClusterId) {
        self.writable.remove(&cluster);
        self.read_only.insert(cluster);
    }

    /// Whether a cluster may be changed at all.
    ///
    /// An explicit read-only entry always wins, so a stale `writable` entry
    /// cannot quietly re-arm a cluster somebody deliberately locked.
    pub fn may_mutate(&self, cluster: &ClusterId) -> bool {
        if self.read_only.contains(cluster) {
            return false;
        }
        if self.read_only_by_default {
            return self.writable.contains(cluster);
        }
        true
    }

    /// The clusters that refuse changes, for the UI to mark.
    pub fn read_only_clusters(&self) -> &BTreeSet<ClusterId> {
        &self.read_only
    }

    /// Whether every cluster is read-only unless named.
    pub fn is_read_only_by_default(&self) -> bool {
        self.read_only_by_default
    }
}

/// A mutation that has been authorised.
///
/// There is no public constructor and no public field: the only way to obtain
/// one is [`crate::AppState::authorize`], which means a caller cannot send a
/// mutation it never asked permission for. That is a compile-time property, not
/// a convention someone has to remember.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorized {
    cluster: ClusterId,
    mutation: std::sync::Arc<Mutation>,
}

impl Authorized {
    /// Only this crate may mint one, and only after checking.
    pub(crate) fn new(cluster: ClusterId, mutation: std::sync::Arc<Mutation>) -> Self {
        Self { cluster, mutation }
    }

    /// Which cluster it is aimed at.
    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }

    /// What it does.
    pub fn mutation(&self) -> &std::sync::Arc<Mutation> {
        &self.mutation
    }

    /// Takes it apart to send.
    pub fn into_parts(self) -> (ClusterId, std::sync::Arc<Mutation>) {
        (self.cluster, self.mutation)
    }
}

/// A command that has been authorised to run in a container.
///
/// Separate from [`Authorized`] because a command is not a [`Mutation`], but
/// held to the same rule and for the same reason: running `rm -rf` in a
/// container is a change, so a read-only cluster must refuse it. There is no
/// public constructor, so an unchecked command cannot be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedExec {
    cluster: ClusterId,
    target: std::sync::Arc<ExecTarget>,
}

impl AuthorizedExec {
    /// Only this crate may mint one, and only after checking.
    pub(crate) fn new(cluster: ClusterId, target: std::sync::Arc<ExecTarget>) -> Self {
        Self { cluster, target }
    }

    /// Which cluster it will run in.
    pub fn cluster(&self) -> &ClusterId {
        &self.cluster
    }

    /// What will be run, and where.
    pub fn target(&self) -> &std::sync::Arc<ExecTarget> {
        &self.target
    }

    /// Takes it apart to send.
    pub fn into_parts(self) -> (ClusterId, std::sync::Arc<ExecTarget>) {
        (self.cluster, self.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prod() -> ClusterId {
        ClusterId::new("prod")
    }

    #[test]
    fn the_default_allows_changes() {
        assert!(Permissions::permissive().may_mutate(&prod()));
    }

    #[test]
    fn a_denied_cluster_refuses() {
        let mut permissions = Permissions::permissive();
        permissions.deny(prod());

        assert!(!permissions.may_mutate(&prod()));
        assert!(permissions.may_mutate(&ClusterId::new("staging")));
    }

    #[test]
    fn read_only_by_default_allows_only_what_is_named() {
        let permissions = Permissions::read_only_except([ClusterId::new("scratch")]);

        assert!(permissions.may_mutate(&ClusterId::new("scratch")));
        assert!(!permissions.may_mutate(&prod()));
    }

    #[test]
    fn an_explicit_denial_beats_writable() {
        let mut permissions = Permissions::read_only_except([prod()]);
        permissions.deny(prod());

        assert!(!permissions.may_mutate(&prod()));
    }

    #[test]
    fn permissions_come_from_settings() {
        let mut access = periscope_config::Access::default();
        access.deny("prod");

        let permissions = Permissions::from_access(&access);
        assert!(!permissions.may_mutate(&prod()));
        assert!(permissions.may_mutate(&ClusterId::new("staging")));
        assert!(permissions.read_only_clusters().contains(&prod()));
    }

    #[test]
    fn a_refusal_says_what_to_change_to_allow_it() {
        let reason = Refusal::ReadOnly { cluster: prod() }.reason();

        assert!(reason.contains("prod"), "{reason}");
        assert!(reason.contains("settings.toml"), "{reason}");
    }
}
