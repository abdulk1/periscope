//! The resource table.
//!
//! Virtualised through `uniform_list`: only the visible rows are built, so a
//! cluster with tens of thousands of objects costs the same per frame as one
//! with twenty. Row data and column definitions are `Arc`s handed to the list
//! closure, which makes capturing them free rather than a copy per render.
//!
//! Nothing here knows what a Pod or a Deployment is. Columns arrive as data
//! from the cluster layer, which is what lets a CRD render like anything else.

use std::sync::Arc;
use std::time::SystemTime;

use gpui::AnimationExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, uniform_list,
};
use gpui_component::{h_flex, v_flex};
use periscope_bridge::{ColumnSpec, ResourceKey, ResourceRow};
use periscope_store::app::{Sort, SortKey};

use crate::style;
pub(crate) use crate::style::ROW_HEIGHT;
use crate::{format, workspace::Workspace};

/// Width of the namespace column.
const NAMESPACE_WIDTH: f32 = 170.;

/// Width of the age column.
///
/// Ages are short (`5m`, `22h`, `3d`) and right-aligned against the edge, so
/// the column needs the width of the longest one and no more.
const AGE_WIDTH: f32 = 56.;

/// Whether a cell holds a number, and should therefore be read on the right.
///
/// `1/1` counts: a ready column is a pair of numbers and lines up as one.
fn is_numeric(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '/' | '.' | '-'))
}

/// One cell, clipped rather than wrapped: wrapping would break the fixed row
/// height `uniform_list` depends on.
fn cell(width: Option<f32>) -> gpui::Div {
    let cell = div().px_2().overflow_hidden().truncate();
    match width {
        Some(width) => cell.w(px(width)).flex_none(),
        None => cell.flex_1().min_w(px(120.)),
    }
}

/// The column headings for a kind.
///
/// Columns arrive paired with their position in the kind's full set, because
/// the user may have chosen a subset and cells are still indexed by the
/// original position.
///
/// Every heading is a button: clicking one sorts by it, clicking again reverses,
/// and a third click puts the natural order back. The one being sorted by wears
/// an arrow.
pub fn header(
    workspace: Entity<Workspace>,
    pane: usize,
    columns: &[(usize, ColumnSpec)],
    namespaced: bool,
    sort: Sort,
    cx: &App,
) -> impl IntoElement {
    let mut row = h_flex()
        .w_full()
        .h(px(ROW_HEIGHT))
        .items_center()
        .flex_none()
        // One hairline under the headings and none between the rows: a table
        // that boxes every cell is read as a grid rather than as a list.
        .border_b_1()
        .border_color(style::hairline(cx))
        .text_xs()
        .text_color(style::text_faint(cx));

    // Matches the marker stripe on every row, so the columns line up.
    row = row.child(div().w(px(style::MARKER)).flex_none());

    /// One column, as the heading row needs to know it.
    struct Heading {
        label: String,
        key: SortKey,
        width: Option<f32>,
        numeric: bool,
    }

    /// One clickable heading.
    fn heading(
        workspace: &Entity<Workspace>,
        pane: usize,
        column: Heading,
        sort: Sort,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let Heading {
            label,
            key,
            width,
            numeric,
        } = column;
        let workspace = workspace.clone();
        let marker = sort.marker(key);
        let sorted = marker.is_some();

        cell(width)
            .id(SharedString::from(format!("heading-{pane}-{label}")))
            .cursor_pointer()
            // The column being sorted by is the only heading that is not faint,
            // so the sort is visible without reading the arrows.
            .when(sorted, |heading| heading.text_color(style::accent(cx)))
            .hover(|heading| heading.text_color(style::text(cx)))
            .when(numeric, gpui::Styled::text_right)
            .on_click(move |_, _, cx| {
                workspace.update(cx, |workspace, cx| workspace.sort_by(pane, key, cx));
            })
            .child(format!("{label}{}", marker.unwrap_or_default()))
    }

    if namespaced {
        row = row.child(heading(
            &workspace,
            pane,
            Heading {
                label: "NAMESPACE".to_owned(),
                key: SortKey::Namespace,
                width: Some(NAMESPACE_WIDTH),
                numeric: false,
            },
            sort,
            cx,
        ));
    }
    row = row.child(heading(
        &workspace,
        pane,
        Heading {
            label: "NAME".to_owned(),
            key: SortKey::Name,
            width: None,
            numeric: false,
        },
        sort,
        cx,
    ));

    for (index, column) in columns {
        row = row.child(heading(
            &workspace,
            pane,
            Heading {
                label: column.name.to_string(),
                key: SortKey::Cell(*index),
                width: column.width.map(|width| width as f32),
                numeric: column.numeric,
            },
            sort,
            cx,
        ));
    }

    row.child(heading(
        &workspace,
        pane,
        Heading {
            label: "AGE".to_owned(),
            key: SortKey::Age,
            width: Some(AGE_WIDTH),
            numeric: true,
        },
        sort,
        cx,
    ))
}

/// One rendered row.
fn row(
    entry: &ResourceRow,
    columns: &[(usize, ColumnSpec)],
    namespaced: bool,
    // `under_cursor` is where the keyboard is; `opened` is what the detail pane
    // is showing. Usually the same row, and they must not look identical when
    // they are not.
    under_cursor: bool,
    opened: bool,
    now: SystemTime,
    cx: &App,
) -> gpui::Div {
    let age = entry
        .age(now)
        .map(format::age)
        .unwrap_or_else(|| "—".to_owned());

    let mut line = h_flex().w_full().h(px(ROW_HEIGHT)).items_center().text_sm();

    if namespaced {
        // The namespace repeats down the whole column and is read second, so
        // it is quiet. The name is what the eye is hunting for.
        line = line.child(
            cell(Some(NAMESPACE_WIDTH))
                .text_color(style::text_dim(cx))
                .child(entry.key.namespace.to_string()),
        );
    }
    line = line.child(
        cell(None)
            .text_color(style::text(cx))
            .child(entry.key.name.to_string()),
    );

    for (index, column) in columns {
        let text = entry.cell(*index).to_owned();
        // The first kind-specific column carries the row's state, which is
        // where the eye lands: READY for a pod, STATUS for a node. Keyed to the
        // original position, so hiding it does not move the colour somewhere it
        // means nothing.
        let color = if *index == 0 {
            style::state(entry.state, cx)
        } else {
            style::text_dim(cx)
        };
        line = line.child(
            cell(column.width.map(|width| width as f32))
                .when(
                    column.numeric || is_numeric(&text),
                    gpui::Styled::text_right,
                )
                .text_color(color)
                .child(text),
        );
    }

    line = line.child(
        cell(Some(AGE_WIDTH))
            .text_right()
            .text_color(style::text_faint(cx))
            .child(age),
    );

    div()
        .w_full()
        .when(under_cursor, |row| row.bg(style::selected(cx)))
        .child(
            h_flex()
                .w_full()
                .items_center()
                // A stripe down the left of whatever the detail pane is
                // showing, so it stays findable after the cursor moves on.
                .child(
                    div()
                        .w(px(style::MARKER))
                        .h(px(ROW_HEIGHT))
                        .flex_none()
                        .bg(if opened {
                            style::accent(cx)
                        } else {
                            gpui::transparent_black()
                        }),
                )
                .child(line),
        )
}

/// Everything one table needs to render.
///
/// A struct rather than nine arguments: they are all "what this table is", and
/// half of them are `usize`s and options that would be easy to pass in the
/// wrong order.
///
/// `now` is carried rather than read per row so every age in one frame is
/// measured against the same instant. Clicking a row opens it in the detail
/// pane, which is why the workspace entity comes along.
#[derive(Clone, Debug)]
pub struct View {
    /// The root view, so a click can open what it hit.
    pub workspace: Entity<Workspace>,
    /// Which pane this is, so a click can focus it.
    pub pane: usize,
    /// The rows to render.
    pub rows: Arc<[Arc<ResourceRow>]>,
    /// Columns, each with its position in the kind's full set.
    pub columns: Arc<[(usize, ColumnSpec)]>,
    /// Whether this kind's objects live in namespaces.
    pub namespaced: bool,
    /// The object the detail pane is showing, if any.
    pub opened: Option<ResourceKey>,
    /// Where the keyboard is.
    pub cursor: usize,
    /// Rows that changed recently, and when, so the change can be shown.
    pub changed: Arc<std::collections::BTreeMap<ResourceKey, std::time::Instant>>,
    /// Scroll position, so the cursor can be brought into view.
    pub scroll: gpui::UniformListScrollHandle,
    /// One instant for every age in the frame.
    pub now: SystemTime,
}

pub fn body(view: View) -> impl IntoElement {
    let View {
        workspace,
        pane,
        rows,
        columns,
        namespaced,
        opened,
        cursor,
        changed,
        scroll,
        now,
    } = view;
    let count = rows.len();

    uniform_list(
        SharedString::from(format!("resources-{pane}")),
        count,
        move |range, _window: &mut Window, cx: &mut App| {
            range
                .filter_map(|index| rows.get(index).map(|entry| (index, entry)))
                .map(|(index, entry)| {
                    let key = entry.key.clone();
                    let workspace = workspace.clone();
                    let changed_at = changed.get(&entry.key).copied();

                    row(
                        entry,
                        &columns,
                        namespaced,
                        index == cursor,
                        opened.as_ref() == Some(&entry.key),
                        now,
                        cx,
                    )
                    .id(index)
                    .cursor_pointer()
                    .hover(|row| row.bg(style::hover(cx)))
                    .on_click(move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            // Clicking a row is also how a pane is focused:
                            // the object opens in the pane it was clicked in.
                            workspace.focus_pane(pane, cx);
                            // And it moves the keyboard cursor there, so that
                            // `j` after a click continues from what was
                            // clicked rather than from wherever it last was.
                            workspace.set_cursor(index, cx);
                            workspace.open_object(key.clone(), window, cx);
                        });
                    })
                    .map(|row| flash(row, changed_at, cx))
                })
                .collect()
        },
    )
    .track_scroll(scroll)
    .flex_1()
    .w_full()
}

/// Fades a wash out of a row that just changed.
///
/// A watch stream rewrites rows underneath whoever is reading them, and without
/// this there is nothing at all to say that a pod restarted or a deployment
/// scaled — the number simply differs from the one that was there a moment ago.
/// The remaining time drives the animation's start, so a row that changed
/// half a second ago is already half faded rather than starting over.
fn flash(
    row: gpui::Stateful<gpui::Div>,
    changed_at: Option<std::time::Instant>,
    cx: &App,
) -> gpui::AnyElement {
    // A row that never changed gets no animation at all. Returning a
    // zero-length one instead — which `FLASH - FLASH` quietly produces — makes
    // GPUI divide the elapsed time by zero, and every table in the test suite
    // panicked on the assertion that catches it.
    let Some(remaining) = changed_at
        .map(|at| at.elapsed())
        .and_then(|elapsed| style::FLASH.checked_sub(elapsed))
        .filter(|remaining| *remaining > std::time::Duration::from_millis(16))
    else {
        return row.into_any_element();
    };

    let tint = style::accent(cx);
    let start = 1.0 - remaining.as_secs_f32() / style::FLASH.as_secs_f32();

    row.with_animation(
        "flash",
        gpui::Animation::new(remaining),
        move |row, delta| {
            let fade = 1.0 - (start + delta * (1.0 - start));
            row.bg(tint.opacity(0.28 * fade.clamp(0., 1.)))
        },
    )
    .into_any_element()
}

/// The message shown where the rows would be, when there are none.
pub fn placeholder(message: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .w_full()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(style::text_faint(cx))
        .child(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_that_is_a_count_is_read_down_its_right_edge() {
        // Most kinds arrive from the cluster layer without a declared
        // alignment, so this is what decides it. `1/1` and `0/3` are the ones
        // that matter: a READY column aligned left beside a RESTARTS column
        // aligned right is the sort of thing nobody reports and everybody
        // notices.
        for text in ["3", "1/1", "0/3", "1.5", "10", "-1"] {
            assert!(is_numeric(text), "{text} should read as a number");
        }
    }

    #[test]
    fn anything_that_is_not_a_count_is_read_from_the_left() {
        // An age is not a number for this purpose — `5m` is a word, and the AGE
        // column sets its own alignment — and neither is an empty cell, which
        // aligned right would drag the cell beside it across for no reason.
        for text in ["", "Running", "CrashLoopBackOff", "5m", "<none>", "10Gi"] {
            assert!(!is_numeric(text), "{text} should not read as a number");
        }
    }
}
