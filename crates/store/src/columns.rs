//! Which of a kind's columns are shown.
//!
//! Column definitions arrive from the cluster layer as data, one set per kind,
//! and by default all of them are rendered. This is the user's say over that:
//! `[columns]` in `settings.toml` names the ones worth screen space.
//!
//! It lives here rather than in the view because "which columns" is a question
//! about state, and because resolving names to indices is exactly the sort of
//! thing that should be testable without a window.

use periscope_bridge::{ColumnSpec, KindId};

/// The configured column choices, resolved against real column definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout(periscope_config::Columns);

impl Layout {
    /// A layout from settings.
    pub fn new(columns: periscope_config::Columns) -> Self {
        Self(columns)
    }

    /// The columns to render for a kind, as indices into `specs`.
    ///
    /// `None` means "all of them", which is both the default and the answer for
    /// a kind nobody configured — the common case, and the one that must not
    /// allocate.
    ///
    /// Names are matched without regard to case, and a name the kind does not
    /// have is ignored: column sets differ between Kubernetes versions and
    /// between CRDs, and refusing to start over a column that no longer exists
    /// would be a poor trade. If *nothing* matches, the kind is left alone
    /// rather than rendered with no columns at all — an empty table is a bug
    /// report, not a configuration.
    pub fn indices(&self, kind: &KindId, specs: &[ColumnSpec]) -> Option<Vec<usize>> {
        let wanted = self.0.wanted(&kind.label())?;

        let chosen: Vec<usize> = wanted
            .iter()
            .filter_map(|name| {
                specs
                    .iter()
                    .position(|spec| spec.name.eq_ignore_ascii_case(name.trim()))
            })
            .collect();

        (!chosen.is_empty()).then_some(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pods() -> KindId {
        KindId::new("", "v1", "Pod", "pods")
    }

    fn specs() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::fixed("READY", 60),
            ColumnSpec::fixed("STATUS", 100),
            ColumnSpec::fixed("RESTARTS", 80),
            ColumnSpec::flexible("NODE"),
        ]
    }

    fn layout(kind: &str, columns: &[&str]) -> Layout {
        let mut chosen = periscope_config::Columns::default();
        chosen.choose(kind, columns.iter().copied());
        Layout::new(chosen)
    }

    #[test]
    fn an_unconfigured_kind_shows_everything() {
        assert_eq!(Layout::default().indices(&pods(), &specs()), None);
    }

    #[test]
    fn chosen_columns_are_resolved_in_the_order_they_were_written() {
        // Order is the user's, not the kind's: somebody who put STATUS first
        // wants to read it first.
        let layout = layout("pods", &["STATUS", "READY"]);

        assert_eq!(layout.indices(&pods(), &specs()), Some(vec![1, 0]));
    }

    #[test]
    fn names_are_matched_without_regard_to_case_or_padding() {
        let layout = layout("pods", &["status", " Ready "]);

        assert_eq!(layout.indices(&pods(), &specs()), Some(vec![1, 0]));
    }

    #[test]
    fn a_column_this_kind_does_not_have_is_skipped_not_fatal() {
        // Column sets differ between Kubernetes versions and between CRDs.
        let layout = layout("pods", &["STATUS", "IMPOSSIBLE"]);

        assert_eq!(layout.indices(&pods(), &specs()), Some(vec![1]));
    }

    #[test]
    fn nothing_matching_leaves_the_kind_alone() {
        // Rendering a table with no columns at all would look like a bug in the
        // app rather than a mistake in a config file.
        let layout = layout("pods", &["nonsense", "also-nonsense"]);

        assert_eq!(layout.indices(&pods(), &specs()), None);
    }

    #[test]
    fn a_choice_applies_only_to_the_kind_it_names() {
        let layout = layout("deployments.apps", &["READY"]);

        assert_eq!(layout.indices(&pods(), &specs()), None);
    }
}
