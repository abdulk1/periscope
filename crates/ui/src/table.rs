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

use crate::format;
use crate::workspace::Workspace;

/// Height of one row. Fixed, because `uniform_list` requires uniformity and
/// because a table that reflows while it streams is unreadable.
const ROW_HEIGHT: f32 = 28.;

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
pub fn header(columns: &[ColumnSpec], namespaced: bool, cx: &App) -> impl IntoElement {
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

    if namespaced {
        row = row.child(cell(Some(NAMESPACE_WIDTH)).child("NAMESPACE"));
    }
    row = row.child(cell(None).child("NAME"));

    for column in columns {
        row =
            row.child(cell(column.width.map(|width| width as f32)).child(column.name.to_string()));
    }

    row.child(cell(Some(AGE_WIDTH)).child("AGE"))
}

/// One rendered row.
fn row(
    entry: &ResourceRow,
    columns: &[ColumnSpec],
    namespaced: bool,
    selected: bool,
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

    for (index, column) in columns.iter().enumerate() {
        let text = entry.cell(index).to_owned();
        // The first kind-specific column carries the row's state, which is
        // where the eye lands: READY for a pod, STATUS for a node.
        let color = if index == 0 {
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
        .when(selected, |row| row.bg(cx.theme().accent))
        .child(line)
}

/// The virtualised body of the table.
///
/// `now` is passed in rather than read per row so every age in one frame is
/// measured against the same instant. Clicking a row opens it in the detail
/// pane, which is why the workspace entity comes along.
pub fn body(
    workspace: Entity<Workspace>,
    rows: Arc<[Arc<ResourceRow>]>,
    columns: Arc<[ColumnSpec]>,
    namespaced: bool,
    selected: Option<ResourceKey>,
    now: SystemTime,
) -> impl IntoElement {
    let count = rows.len();

    uniform_list(
        "resources",
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
                        selected.as_ref() == Some(&entry.key),
                        now,
                        cx,
                    )
                    .id(index)
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().accent.opacity(0.5)))
                    .on_click(move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.open_object(key.clone(), window, cx);
                        });
                    })
                })
                .collect()
        },
    )
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
