//! The pod table.
//!
//! Virtualised through `uniform_list`: only the visible rows are built, so a
//! cluster with tens of thousands of pods costs the same per frame as one with
//! twenty. The row data is an `Arc` slice handed to the list closure, which
//! makes capturing it free rather than a copy per render.

use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    App, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use periscope_bridge::PodSnapshot;

use crate::format;

/// Height of one row. Fixed, because `uniform_list` requires uniformity and
/// because a table that reflows while it streams is unreadable.
const ROW_HEIGHT: f32 = 28.;

/// How a status reads at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Working as intended.
    Healthy,
    /// On its way somewhere: scheduling, pulling, initialising, terminating.
    Transient,
    /// Needs attention.
    Failing,
}

/// Classifies a pod's STATUS text.
///
/// Driven by the strings `kubectl` produces, so anything unrecognised is
/// deliberately treated as neutral rather than guessed at.
pub fn severity(status: &str) -> Severity {
    if status.starts_with("Init:") {
        // `Init:Error`, `Init:CrashLoopBackOff` are failures; `Init:0/2` is not.
        return match status.trim_start_matches("Init:") {
            rest if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) => Severity::Transient,
            rest => severity(rest),
        };
    }

    match status {
        "Running" | "Completed" | "Succeeded" => Severity::Healthy,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "SchedulingGated"
        | "NotReady" => Severity::Transient,
        "CrashLoopBackOff"
        | "Error"
        | "Failed"
        | "Evicted"
        | "OOMKilled"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "CreateContainerConfigError"
        | "CreateContainerError"
        | "InvalidImageName"
        | "Unknown" => Severity::Failing,
        other if other.starts_with("Signal:") || other.starts_with("ExitCode:") => {
            Severity::Failing
        }
        _ => Severity::Transient,
    }
}

fn severity_color(status: &str, cx: &App) -> gpui::Hsla {
    match severity(status) {
        Severity::Healthy => cx.theme().success,
        Severity::Transient => cx.theme().warning,
        Severity::Failing => cx.theme().danger,
    }
}

/// One column's width. `None` means "take the remaining space".
type Width = Option<f32>;

/// The columns, in render order.
const COLUMNS: [(&str, Width); 7] = [
    ("NAMESPACE", Some(180.)),
    ("NAME", None),
    ("READY", Some(72.)),
    ("STATUS", Some(180.)),
    ("RESTARTS", Some(90.)),
    ("AGE", Some(80.)),
    ("NODE", Some(200.)),
];

fn cell(width: Width) -> gpui::Div {
    // Names routinely run past their column. Wrapping would break the fixed row
    // height `uniform_list` depends on, so cells clip with an ellipsis instead.
    let cell = div().px_2().overflow_hidden().truncate();
    match width {
        Some(width) => cell.w(px(width)).flex_none(),
        None => cell.flex_1().min_w(px(120.)),
    }
}

/// The column headings.
pub fn header(cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .h(px(ROW_HEIGHT))
        .items_center()
        .flex_none()
        .border_b_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .children(
            COLUMNS
                .iter()
                .map(|(label, width)| cell(*width).child(SharedString::new_static(label))),
        )
}

/// One rendered row.
fn row(pod: &PodSnapshot, now: SystemTime, cx: &App) -> impl IntoElement {
    let age = pod
        .age(now)
        .map(format::age)
        .unwrap_or_else(|| "—".to_owned());
    let restarts = pod.restarts.to_string();
    let ready = format!("{}/{}", pod.ready, pod.containers);

    h_flex()
        .w_full()
        .h(px(ROW_HEIGHT))
        .items_center()
        .text_sm()
        .text_color(cx.theme().foreground)
        .child(
            cell(COLUMNS[0].1)
                .text_color(cx.theme().muted_foreground)
                .child(pod.key.namespace.to_string()),
        )
        .child(cell(COLUMNS[1].1).child(pod.key.name.to_string()))
        .child(
            cell(COLUMNS[2].1)
                .text_color(if pod.is_ready() {
                    cx.theme().foreground
                } else {
                    cx.theme().warning
                })
                .child(ready),
        )
        .child(
            cell(COLUMNS[3].1)
                .text_color(severity_color(&pod.status, cx))
                .child(pod.status.to_string()),
        )
        .child(
            cell(COLUMNS[4].1)
                .text_color(if pod.restarts > 0 {
                    cx.theme().warning
                } else {
                    cx.theme().muted_foreground
                })
                .child(restarts),
        )
        .child(
            cell(COLUMNS[5].1)
                .text_color(cx.theme().muted_foreground)
                .child(age),
        )
        .child(
            cell(COLUMNS[6].1)
                .text_color(cx.theme().muted_foreground)
                .child(pod.node.as_deref().unwrap_or("—").to_owned()),
        )
}

/// The virtualised body of the table.
///
/// `now` is passed in rather than read per row so every age in one frame is
/// measured against the same instant.
pub fn body(rows: Arc<[Arc<PodSnapshot>]>, now: SystemTime) -> impl IntoElement {
    let count = rows.len();

    uniform_list(
        "pods",
        count,
        move |range, _window: &mut Window, cx: &mut App| {
            range
                .filter_map(|index| rows.get(index))
                .map(|pod| {
                    div()
                        .w_full()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(row(pod, now, cx))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_statuses_are_recognised() {
        assert_eq!(severity("Running"), Severity::Healthy);
        assert_eq!(severity("Completed"), Severity::Healthy);
    }

    #[test]
    fn failures_are_recognised() {
        for status in [
            "CrashLoopBackOff",
            "ImagePullBackOff",
            "Evicted",
            "Unknown",
            "Signal:9",
            "ExitCode:137",
        ] {
            assert_eq!(severity(status), Severity::Failing, "{status}");
        }
    }

    #[test]
    fn init_progress_is_transient_but_a_failing_init_container_is_not() {
        assert_eq!(severity("Init:0/2"), Severity::Transient);
        assert_eq!(severity("Init:CrashLoopBackOff"), Severity::Failing);
        assert_eq!(severity("Init:Error"), Severity::Failing);
    }

    #[test]
    fn an_unknown_status_is_not_reported_as_healthy() {
        // Guessing "fine" for a string we do not recognise is the one wrong
        // answer here; a colour that says "look at me" is recoverable.
        assert_eq!(severity("SomethingNewInK8s"), Severity::Transient);
    }
}
