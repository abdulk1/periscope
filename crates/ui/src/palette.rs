//! The command palette's matching.
//!
//! Fuzzy matching is `nucleo`'s — the same matcher Zed uses — and what lives
//! here is the vocabulary: what can be jumped to, how each candidate reads, and
//! the ranking rules that keep a 10,000-object cluster answering in time to be
//! typed at.
//!
//! Kept free of GPUI so the ranking can be tested, and benchmarked, without a
//! window.

use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use periscope_bridge::{KindId, KindInfo, ResourceKey, ResourceRow};

/// How many results the palette shows.
///
/// The cap is what makes the palette fast on a large cluster: scoring is linear
/// in the candidates, but nothing sorts or renders more than this.
pub const MAX_RESULTS: usize = 50;

/// Something the palette can jump to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Switch the table to a kind.
    Kind(KindId),
    /// Open an object's detail view.
    Object {
        /// Kind of the object.
        kind: KindId,
        /// Which object.
        key: ResourceKey,
    },
    /// Switch to a cluster.
    Cluster(Arc<str>),
}

/// One candidate, ready to score and render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What selecting this does.
    pub target: Target,
    /// The line the user reads and types against.
    pub label: String,
    /// The dimmer text to its right.
    pub detail: String,
}

/// A scored candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// The candidate.
    pub candidate: Candidate,
    /// `nucleo`'s score; higher is better.
    pub score: u32,
}

/// Builds and ranks palette candidates.
pub struct Palette {
    matcher: Matcher,
    haystacks: Vec<Utf32String>,
    candidates: Vec<Candidate>,
}

impl std::fmt::Debug for Palette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Palette")
            .field("candidates", &self.candidates.len())
            .finish_non_exhaustive()
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    /// An empty palette.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            haystacks: Vec::new(),
            candidates: Vec::new(),
        }
    }

    /// Rebuilds the candidate list.
    ///
    /// Called when the palette opens rather than on every keystroke: the
    /// candidates only change when the cluster does.
    pub fn rebuild(
        &mut self,
        clusters: &[Arc<str>],
        kinds: &[KindInfo],
        kind: Option<&KindId>,
        rows: &[Arc<ResourceRow>],
    ) {
        self.candidates.clear();

        for cluster in clusters {
            self.candidates.push(Candidate {
                target: Target::Cluster(Arc::clone(cluster)),
                label: cluster.to_string(),
                detail: "cluster".to_owned(),
            });
        }

        for info in kinds {
            self.candidates.push(Candidate {
                label: info.id.label(),
                detail: if info.custom {
                    format!("custom resource · {}", info.id.kind)
                } else {
                    info.id.kind.to_string()
                },
                target: Target::Kind(info.id.clone()),
            });
        }

        if let Some(kind) = kind {
            for row in rows {
                self.candidates.push(Candidate {
                    label: row.key.to_string(),
                    detail: kind.label(),
                    target: Target::Object {
                        kind: kind.clone(),
                        key: row.key.clone(),
                    },
                });
            }
        }

        // Scoring wants UTF-32; converting once per rebuild rather than once
        // per keystroke is most of what keeps this fast on a large cluster.
        self.haystacks = self
            .candidates
            .iter()
            .map(|candidate| Utf32String::from(candidate.label.as_str()))
            .collect();
    }

    /// How many candidates are available.
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Whether there is nothing to match against.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// The best matches for a query, best first.
    ///
    /// An empty query returns the first candidates in their natural order —
    /// clusters, then kinds, then objects — which is a useful menu rather than
    /// an arbitrary one.
    pub fn search(&mut self, query: &str) -> Vec<Match> {
        if query.trim().is_empty() {
            return self
                .candidates
                .iter()
                .take(MAX_RESULTS)
                .map(|candidate| Match {
                    candidate: candidate.clone(),
                    score: 0,
                })
                .collect();
        }

        let pattern = Pattern::parse(query.trim(), CaseMatching::Smart, Normalization::Smart);

        let mut matches: Vec<Match> = self
            .candidates
            .iter()
            .zip(self.haystacks.iter())
            .filter_map(|(candidate, haystack)| {
                pattern
                    .score(haystack.slice(..), &mut self.matcher)
                    .map(|score| Match {
                        candidate: candidate.clone(),
                        score,
                    })
            })
            .collect();

        // Ties break towards the shorter label, so `pods` beats
        // `podsecuritypolicies` when both match `pods`.
        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.candidate.label.len().cmp(&b.candidate.label.len()))
                .then_with(|| a.candidate.label.cmp(&b.candidate.label))
        });
        matches.truncate(MAX_RESULTS);
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use periscope_bridge::RowState;

    fn kind(group: &str, kind: &str, plural: &str, custom: bool) -> KindInfo {
        KindInfo {
            id: KindId::new(group, "v1", kind, plural),
            namespaced: true,
            watchable: true,
            custom,
        }
    }

    fn row(namespace: &str, name: &str) -> Arc<ResourceRow> {
        Arc::new(ResourceRow {
            key: ResourceKey::new(namespace, name),
            uid: None,
            cells: Arc::from([] as [Arc<str>; 0]),
            state: RowState::Neutral,
            created: None,
        })
    }

    fn palette() -> Palette {
        let mut palette = Palette::new();
        let kinds = [
            kind("", "Pod", "pods", false),
            kind("apps", "Deployment", "deployments", false),
            kind("policy", "PodSecurityPolicy", "podsecuritypolicies", false),
            kind("argoproj.io", "Application", "applications", true),
        ];
        let rows = [row("default", "api-0"), row("kube-system", "coredns-abc")];

        palette.rebuild(
            &[Arc::from("prod"), Arc::from("staging")],
            &kinds,
            Some(&KindId::new("", "v1", "Pod", "pods")),
            &rows,
        );
        palette
    }

    fn labels(matches: &[Match]) -> Vec<String> {
        matches
            .iter()
            .map(|found| found.candidate.label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_offers_clusters_then_kinds_then_objects() {
        let mut palette = palette();
        let found = labels(&palette.search(""));

        assert_eq!(found[0], "prod");
        assert_eq!(found[1], "staging");
        assert_eq!(found[2], "pods");
        assert!(found.contains(&"default/api-0".to_owned()));
    }

    #[test]
    fn an_exact_kind_name_ranks_above_a_longer_one_containing_it() {
        let mut palette = palette();
        let found = labels(&palette.search("pods"));

        // Both match; `pods` is what the user meant. Kinds outside the core
        // group carry their group, exactly as `kubectl` names them.
        assert_eq!(found[0], "pods");
        assert!(
            found.contains(&"podsecuritypolicies.policy".to_owned()),
            "{found:?}"
        );
    }

    #[test]
    fn objects_are_reachable_by_name() {
        let mut palette = palette();
        let found = palette.search("coredns");

        assert_eq!(found[0].candidate.label, "kube-system/coredns-abc");
        assert!(matches!(found[0].candidate.target, Target::Object { .. }));
    }

    #[test]
    fn objects_are_reachable_by_namespace() {
        let mut palette = palette();
        assert!(
            labels(&palette.search("kube-system"))
                .iter()
                .any(|label| label.contains("coredns"))
        );
    }

    #[test]
    fn matching_is_fuzzy_not_merely_substring() {
        let mut palette = palette();
        // The way people actually type: initials of the words they remember.
        assert!(
            labels(&palette.search("dep")).contains(&"deployments.apps".to_owned()),
            "dep should reach deployments"
        );
        assert!(
            labels(&palette.search("aplctns"))
                .iter()
                .any(|label| label.starts_with("applications")),
            "initials should reach applications"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_returns_nothing() {
        let mut palette = palette();
        assert!(palette.search("zzzzzzzz").is_empty());
    }

    #[test]
    fn custom_resources_say_that_they_are_custom() {
        let mut palette = palette();
        let found = palette.search("applications");
        assert!(found[0].candidate.detail.contains("custom"), "{found:?}");
        assert_eq!(found[0].candidate.label, "applications.argoproj.io");
    }

    #[test]
    fn switching_clusters_is_a_palette_target() {
        let mut palette = palette();
        let found = palette.search("staging");
        assert_eq!(
            found[0].candidate.target,
            Target::Cluster(Arc::from("staging"))
        );
    }

    #[test]
    fn results_are_capped_so_a_huge_cluster_stays_typeable() {
        let mut palette = Palette::new();
        let rows: Vec<_> = (0..10_000)
            .map(|i| row("default", &format!("worker-{i:05}")))
            .collect();
        palette.rebuild(&[], &[], Some(&KindId::new("", "v1", "Pod", "pods")), &rows);

        assert_eq!(palette.len(), 10_000);
        assert_eq!(palette.search("worker").len(), MAX_RESULTS);
        assert_eq!(palette.search("").len(), MAX_RESULTS);
    }

    #[test]
    fn a_ten_thousand_object_search_stays_well_inside_the_budget() {
        // The Phase 2 budget is 50ms on a 10k-object cluster.
        let mut palette = Palette::new();
        let rows: Vec<_> = (0..10_000)
            .map(|i| row("default", &format!("worker-{i:05}")))
            .collect();
        palette.rebuild(&[], &[], Some(&KindId::new("", "v1", "Pod", "pods")), &rows);

        let started = std::time::Instant::now();
        let found = palette.search("wk9");
        let elapsed = started.elapsed();

        assert!(!found.is_empty());
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "searching 10k objects took {elapsed:?}"
        );
    }

    #[test]
    fn rebuilding_replaces_the_previous_candidates() {
        let mut palette = palette();
        palette.rebuild(&[], &[], None, &[]);

        assert!(palette.is_empty());
        assert!(palette.search("pods").is_empty());
    }
}
