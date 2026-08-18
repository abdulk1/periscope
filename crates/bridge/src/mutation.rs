//! The things Periscope can change, and what it must say before changing them.
//!
//! Phases 0–4 were read-only by construction. This is where that stops, so the
//! types are shaped around the rule that replaces it: **no mutation exists
//! without a sentence naming the cluster, the object and the operation.** That
//! sentence is [`Mutation::confirmation`], and it is built from the same data
//! the request carries — it cannot drift from what will actually happen.

use std::sync::Arc;

use crate::resource::{KindId, ResourceKey};

/// Something that changes a cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// Delete an object.
    Delete {
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
        /// Seconds to allow for graceful shutdown; `None` uses the object's own
        /// default.
        grace_period: Option<u32>,
    },
    /// Set a workload's replica count.
    Scale {
        /// Kind of the workload.
        kind: KindId,
        /// Which workload.
        key: ResourceKey,
        /// The replica count to set.
        replicas: u32,
        /// What it is now, for the confirmation to quote.
        current: Option<u32>,
    },
    /// Roll a workload's pods, the way `kubectl rollout restart` does.
    Restart {
        /// Kind of the workload.
        kind: KindId,
        /// Which workload.
        key: ResourceKey,
    },
    /// Mark a node schedulable or not.
    Cordon {
        /// Which node.
        node: Arc<str>,
        /// `true` to cordon, `false` to uncordon.
        cordon: bool,
    },
    /// Apply an edited object.
    Apply {
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
        /// The edited YAML.
        yaml: Arc<str>,
        /// Ask the apiserver what would happen without changing anything.
        dry_run: bool,
    },
}

impl Mutation {
    /// The kind this acts on.
    pub fn kind(&self) -> KindId {
        match self {
            Self::Delete { kind, .. } | Self::Scale { kind, .. } | Self::Restart { kind, .. } => {
                kind.clone()
            }
            Self::Apply { kind, .. } => kind.clone(),
            Self::Cordon { .. } => KindId::new("", "v1", "Node", "nodes"),
        }
    }

    /// The object this acts on.
    pub fn key(&self) -> ResourceKey {
        match self {
            Self::Delete { key, .. }
            | Self::Scale { key, .. }
            | Self::Restart { key, .. }
            | Self::Apply { key, .. } => key.clone(),
            Self::Cordon { node, .. } => ResourceKey::cluster_scoped(&**node),
        }
    }

    /// The verb, as the audit log records it.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Delete { .. } => "delete",
            Self::Scale { .. } => "scale",
            Self::Restart { .. } => "restart",
            Self::Cordon { cordon: true, .. } => "cordon",
            Self::Cordon { cordon: false, .. } => "uncordon",
            Self::Apply { dry_run: true, .. } => "dry-run",
            Self::Apply { .. } => "apply",
        }
    }

    /// What the operation asked for, for the audit log's detail column.
    pub fn detail(&self) -> String {
        match self {
            Self::Scale { replicas, .. } => format!("replicas={replicas}"),
            Self::Delete {
                grace_period: Some(seconds),
                ..
            } => format!("grace={seconds}s"),
            Self::Apply { yaml, .. } => format!("{} bytes", yaml.len()),
            _ => String::new(),
        }
    }

    /// Whether this can destroy something a user would miss.
    ///
    /// Not the same as "changes something": a dry run changes nothing, and an
    /// uncordon is trivially reversible. Delete, and scaling to zero, are not.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::Delete { .. }
                | Self::Scale { replicas: 0, .. }
                | Self::Cordon { cordon: true, .. }
                | Self::Restart { .. }
        )
    }

    /// Whether this changes anything at all.
    pub fn changes_anything(&self) -> bool {
        !matches!(self, Self::Apply { dry_run: true, .. })
    }

    /// The sentence a confirmation must show.
    ///
    /// It names the cluster first, because the mistake this exists to prevent
    /// is doing the right thing to the wrong cluster.
    pub fn confirmation(&self, cluster: &crate::ClusterId) -> String {
        let key = self.key();
        let target = if key.is_namespaced() {
            format!(
                "{} {} in namespace {}",
                self.kind(),
                key.name,
                key.namespace
            )
        } else {
            format!("{} {}", self.kind(), key.name)
        };

        match self {
            Self::Delete { grace_period, .. } => {
                let grace = match grace_period {
                    Some(0) => " immediately, with no grace period,".to_owned(),
                    Some(seconds) => format!(" with a {seconds}s grace period,"),
                    None => String::new(),
                };
                format!("Delete{grace} {target} on cluster {cluster}?")
            }
            Self::Scale {
                replicas, current, ..
            } => {
                let from = match current {
                    Some(current) => format!(" from {current}"),
                    None => String::new(),
                };
                format!("Scale {target}{from} to {replicas} replicas on cluster {cluster}?")
            }
            Self::Restart { .. } => {
                format!("Restart every pod of {target} on cluster {cluster}?")
            }
            Self::Cordon { cordon: true, .. } => format!(
                "Cordon {target} on cluster {cluster}? No new pods will be scheduled on it."
            ),
            Self::Cordon { cordon: false, .. } => {
                format!("Uncordon {target} on cluster {cluster}?")
            }
            Self::Apply { dry_run: true, .. } => {
                format!("Ask cluster {cluster} what applying {target} would do?")
            }
            Self::Apply { .. } => format!("Apply your edits to {target} on cluster {cluster}?"),
        }
    }
}

/// What happened to a mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationOutcome {
    /// The apiserver accepted it.
    Applied {
        /// What to tell the user, in one line.
        detail: String,
    },
    /// A dry run came back. Nothing changed.
    DryRun {
        /// What the apiserver said the object would become.
        preview: Arc<str>,
    },
    /// Periscope refused before sending anything.
    Refused {
        /// Why.
        reason: String,
    },
    /// The apiserver rejected it, or the request failed.
    Failed {
        /// Verbatim underlying reason.
        reason: String,
    },
}

impl MutationOutcome {
    /// Whether this needs the user's attention.
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Refused { .. } | Self::Failed { .. })
    }

    /// The line the UI shows.
    pub fn message(&self) -> &str {
        match self {
            Self::Applied { detail } => detail,
            Self::DryRun { .. } => "Dry run: nothing was changed.",
            Self::Refused { reason } | Self::Failed { reason } => reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClusterId;

    fn deployments() -> KindId {
        KindId::new("apps", "v1", "Deployment", "deployments")
    }

    fn api() -> ResourceKey {
        ResourceKey::new("payments", "api")
    }

    fn prod() -> ClusterId {
        ClusterId::new("prod")
    }

    #[test]
    fn every_confirmation_names_the_cluster() {
        // This is the rule the whole phase turns on, so it is asserted for
        // every variant rather than for a sample.
        let mutations = [
            Mutation::Delete {
                kind: deployments(),
                key: api(),
                grace_period: None,
            },
            Mutation::Scale {
                kind: deployments(),
                key: api(),
                replicas: 3,
                current: Some(1),
            },
            Mutation::Restart {
                kind: deployments(),
                key: api(),
            },
            Mutation::Cordon {
                node: Arc::from("worker-0"),
                cordon: true,
            },
            Mutation::Apply {
                kind: deployments(),
                key: api(),
                yaml: Arc::from("kind: Deployment"),
                dry_run: false,
            },
        ];

        for mutation in mutations {
            let sentence = mutation.confirmation(&prod());
            assert!(
                sentence.contains("prod"),
                "{:?} does not name the cluster: {sentence}",
                mutation.verb()
            );
            assert!(
                sentence.ends_with('?') || sentence.contains('?'),
                "{sentence}"
            );
        }
    }

    #[test]
    fn a_confirmation_names_the_object_and_its_namespace() {
        let sentence = Mutation::Delete {
            kind: deployments(),
            key: api(),
            grace_period: None,
        }
        .confirmation(&prod());

        assert_eq!(
            sentence,
            "Delete deployments.apps api in namespace payments on cluster prod?"
        );
    }

    #[test]
    fn a_cluster_scoped_object_has_no_namespace_in_its_sentence() {
        let sentence = Mutation::Cordon {
            node: Arc::from("worker-0"),
            cordon: true,
        }
        .confirmation(&prod());

        assert_eq!(
            sentence,
            "Cordon nodes worker-0 on cluster prod? No new pods will be scheduled on it."
        );
    }

    #[test]
    fn a_forced_delete_says_that_it_is_immediate() {
        let sentence = Mutation::Delete {
            kind: KindId::new("", "v1", "Pod", "pods"),
            key: ResourceKey::new("payments", "api-0"),
            grace_period: Some(0),
        }
        .confirmation(&prod());

        assert!(sentence.contains("immediately"), "{sentence}");
        assert!(sentence.contains("no grace period"), "{sentence}");
    }

    #[test]
    fn a_scale_quotes_what_it_is_changing_from() {
        let sentence = Mutation::Scale {
            kind: deployments(),
            key: api(),
            replicas: 0,
            current: Some(5),
        }
        .confirmation(&prod());

        assert!(sentence.contains("from 5"), "{sentence}");
        assert!(sentence.contains("to 0"), "{sentence}");
    }

    #[test]
    fn destructive_operations_are_marked_as_such() {
        assert!(
            Mutation::Delete {
                kind: deployments(),
                key: api(),
                grace_period: None
            }
            .is_destructive()
        );
        assert!(
            Mutation::Scale {
                kind: deployments(),
                key: api(),
                replicas: 0,
                current: Some(3)
            }
            .is_destructive()
        );
        assert!(
            Mutation::Restart {
                kind: deployments(),
                key: api()
            }
            .is_destructive()
        );
        assert!(
            Mutation::Cordon {
                node: Arc::from("worker-0"),
                cordon: true
            }
            .is_destructive()
        );

        // Scaling up and uncordoning are not destructive; a dry run changes
        // nothing at all.
        assert!(
            !Mutation::Scale {
                kind: deployments(),
                key: api(),
                replicas: 3,
                current: Some(1)
            }
            .is_destructive()
        );
        assert!(
            !Mutation::Cordon {
                node: Arc::from("worker-0"),
                cordon: false
            }
            .is_destructive()
        );

        let dry = Mutation::Apply {
            kind: deployments(),
            key: api(),
            yaml: Arc::from("kind: Deployment"),
            dry_run: true,
        };
        assert!(!dry.changes_anything());
        assert_eq!(dry.verb(), "dry-run");
    }

    #[test]
    fn the_audit_detail_records_what_was_asked_for() {
        assert_eq!(
            Mutation::Scale {
                kind: deployments(),
                key: api(),
                replicas: 4,
                current: None
            }
            .detail(),
            "replicas=4"
        );
        assert_eq!(
            Mutation::Delete {
                kind: deployments(),
                key: api(),
                grace_period: Some(30)
            }
            .detail(),
            "grace=30s"
        );
    }

    #[test]
    fn a_cordon_acts_on_nodes_whatever_the_table_was_showing() {
        let cordon = Mutation::Cordon {
            node: Arc::from("worker-0"),
            cordon: true,
        };
        assert_eq!(cordon.kind(), KindId::new("", "v1", "Node", "nodes"));
        assert!(!cordon.key().is_namespaced());
    }

    #[test]
    fn outcomes_say_whether_they_need_attention() {
        assert!(
            !MutationOutcome::Applied {
                detail: "deleted".into()
            }
            .is_problem()
        );
        assert!(
            !MutationOutcome::DryRun {
                preview: Arc::from("kind: Deployment")
            }
            .is_problem()
        );
        assert!(
            MutationOutcome::Refused {
                reason: "prod is read-only".into()
            }
            .is_problem()
        );
        assert!(
            MutationOutcome::Failed {
                reason: "not found".into()
            }
            .is_problem()
        );
    }
}
