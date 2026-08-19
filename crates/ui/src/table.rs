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

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use periscope_bridge::{ColumnSpec, ResourceKey, ResourceRow, RowState};
use periscope_store::app::{Sort, SortKey};

use crate::format;
use crate::workspace::Workspace;

/// Height of one row. Fixed, because `uniform_list` requires uniformity and
/// because a table that reflows while it streams is unreadable.
pub(crate) const ROW_HEIGHT: f32 = 28.;

/// Width of the namespace column.
const NAMESPACE_WIDTH: f32 = 180.;

/// Width of the age column.
const AGE_WIDTH: f32 = 80.;

/// The colour a row's state reads as.
fn state_color(state: RowState, cx: &App) -> gpui::Hsla {
    match state {
        RowState::Healthy => cx.theme().success,
        RowState::Transient => cx.theme().warning,
        RowState::Failing => cx.theme().danger,
        RowState::Neutral => cx.theme().foreground,
    }
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
        .border_b_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().muted_foreground);

    // Matches the marker stripe on every row, so the columns line up.
    row = row.child(div().w(px(2.)).flex_none());

    /// One clickable heading.
    fn heading(
        workspace: &Entity<Workspace>,
        pane: usize,
        label: String,
        key: SortKey,
        width: Option<f32>,
        sort: Sort,
    ) -> gpui::Stateful<gpui::Div> {
        let workspace = workspace.clone();
        let marker = sort.marker(key).unwrap_or_default();

        cell(width)
            .id(SharedString::from(format!("heading-{pane}-{label}")))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                workspace.update(cx, |workspace, cx| workspace.sort_by(pane, key, cx));
            })
            .child(format!("{label}{marker}"))
    }

    if namespaced {
        row = row.child(heading(
            &workspace,
            pane,
            "NAMESPACE".to_owned(),
            SortKey::Namespace,
            Some(NAMESPACE_WIDTH),
            sort,
        ));
    }
    row = row.child(heading(
        &workspace,
        pane,
        "NAME".to_owned(),
        SortKey::Name,
        None,
        sort,
    ));

    for (index, column) in columns {
        row = row.child(heading(
            &workspace,
            pane,
            column.name.to_string(),
            SortKey::Cell(*index),
            column.width.map(|width| width as f32),
            sort,
        ));
    }

    row.child(heading(
        &workspace,
        pane,
        "AGE".to_owned(),
        SortKey::Age,
        Some(AGE_WIDTH),
        sort,
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

    let mut line = h_flex()
        .w_full()
        .h(px(ROW_HEIGHT))
        .items_center()
        .text_sm()
        .text_color(cx.theme().foreground);

    if namespaced {
        line = line.child(
            cell(Some(NAMESPACE_WIDTH))
                .text_color(cx.theme().muted_foreground)
                .child(entry.key.namespace.to_string()),
        );
    }
    line = line.child(cell(None).child(entry.key.name.to_string()));

    for (index, column) in columns {
        let text = entry.cell(*index).to_owned();
        // The first kind-specific column carries the row's state, which is
        // where the eye lands: READY for a pod, STATUS for a node. Keyed to the
        // original position, so hiding it does not move the colour somewhere it
        // means nothing.
        let color = if *index == 0 {
            state_color(entry.state, cx)
        } else {
            cx.theme().foreground
        };
        line = line.child(
            cell(column.width.map(|width| width as f32))
                .text_color(color)
                .child(text),
        );
    }

    line = line.child(
        cell(Some(AGE_WIDTH))
            .text_color(cx.theme().muted_foreground)
            .child(age),
    );

    div()
        .w_full()
        .border_b_1()
        .border_color(cx.theme().border)
        .when(under_cursor, |row| row.bg(cx.theme().accent))
        .child(
            h_flex()
                .w_full()
                .items_center()
                // A stripe down the left of whatever the detail pane is
                // showing, so it stays findable after the cursor moves on.
                .child(div().w(px(2.)).h(px(ROW_HEIGHT)).flex_none().bg(if opened {
                    cx.theme().primary
                } else {
                    gpui::transparent_black()
                }))
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
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.5)))
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
                })
                .collect()
        },
    )
    .track_scroll(scroll)
    .flex_1()
    .w_full()
}

/// The message shown where the rows would be, when there are none.
pub fn placeholder(message: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .w_full()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(message.into())
}
