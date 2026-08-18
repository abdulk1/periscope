//! The log view.
//!
//! Same virtualisation as the resource table — only the visible lines are
//! built — but the content is different in two ways that matter: lines are
//! monospaced and never wrap, and a merged stream needs its sources
//! distinguishable at a glance, so each pod gets a stable colour.

use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    App, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use periscope_bridge::{LogLine, LogSource, LogSourceState};

/// Height of one log line.
const LINE_HEIGHT: f32 = 18.;

/// Width of the source column in a merged stream.
const SOURCE_WIDTH: f32 = 220.;

/// Width of the timestamp column.
const TIME_WIDTH: f32 = 96.;

/// Colours cycled through for log sources.
///
/// Chosen to stay distinguishable on both themes, and kept few enough that
/// neighbouring pods rarely collide.
const SOURCE_COLOURS: [u32; 8] = [
    0x7aa2f7, 0x9ece6a, 0xe0af68, 0xf7768e, 0xbb9af7, 0x7dcfff, 0xff9e64, 0x73daca,
];

/// The colour a source's lines are tagged with.
///
/// Derived from the name, so a pod keeps its colour for as long as it exists —
/// including across a filter change, which would otherwise reshuffle them.
pub fn source_colour(source: &LogSource) -> gpui::Hsla {
    let hash = source.pod.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as u32)
    });
    let rgb = SOURCE_COLOURS[(hash as usize) % SOURCE_COLOURS.len()];
    gpui::rgb(rgb).into()
}

/// Formats a timestamp as wall-clock time, which is what a reader correlates
/// against. The date is in the exported text; on screen it is noise.
pub fn format_time(time: SystemTime) -> String {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let (hours, minutes, secs) = (seconds / 3_600 % 24, seconds / 60 % 60, seconds % 60);
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

/// The virtualised body of the log view.
pub fn body(
    lines: Arc<[Arc<LogLine>]>,
    show_source: bool,
    scroll: gpui::UniformListScrollHandle,
) -> impl IntoElement {
    let count = lines.len();

    uniform_list(
        "log-lines",
        count,
        move |range, _window: &mut Window, cx: &mut App| {
            range
                .filter_map(|index| lines.get(index))
                .map(|line| {
                    let time = line
                        .timestamp
                        .map(format_time)
                        .unwrap_or_else(|| "--:--:--".to_owned());

                    h_flex()
                        .w_full()
                        .h(px(LINE_HEIGHT))
                        .items_center()
                        .font_family("monospace")
                        .text_xs()
                        .child(
                            div()
                                .w(px(TIME_WIDTH))
                                .flex_none()
                                .px_2()
                                .text_color(cx.theme().muted_foreground)
                                .child(time),
                        )
                        .children(show_source.then(|| {
                            div()
                                .w(px(SOURCE_WIDTH))
                                .flex_none()
                                .px_2()
                                .truncate()
                                .text_color(source_colour(&line.source))
                                .child(line.source.label())
                        }))
                        .child(
                            div()
                                .flex_1()
                                .px_2()
                                .truncate()
                                .text_color(cx.theme().foreground)
                                .child(line.text.to_string()),
                        )
                })
                .collect()
        },
    )
    .track_scroll(scroll)
    .flex_1()
    .w_full()
}

/// The legend: which pods are attached, and which have stopped.
pub fn sources(sources: &[(LogSource, LogSourceState)], cx: &App) -> impl IntoElement {
    let chips: Vec<_> = sources
        .iter()
        .take(12)
        .map(|(source, state)| {
            let (label, dim) = match state {
                LogSourceState::Streaming => (source.label(), false),
                LogSourceState::Ended => (format!("{} (ended)", source.label()), true),
                LogSourceState::Failed { .. } => (format!("{} (failed)", source.label()), true),
            };

            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(source_colour(source).opacity(if dim { 0.4 } else { 1.0 })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(if dim {
                            cx.theme().muted_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .child(label),
                )
        })
        .collect();

    let overflow = (sources.len() > 12).then(|| {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!("+{} more", sources.len() - 12))
    });

    h_flex()
        .w_full()
        .flex_none()
        .gap_3()
        .px_3()
        .py_1()
        .flex_wrap()
        .border_b_1()
        .border_color(cx.theme().border)
        .children(chips)
        .children(overflow)
}

/// The message shown when a session has produced nothing to show.
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
    use std::time::Duration;

    #[test]
    fn a_source_keeps_its_colour() {
        let source = LogSource::new("api-0", "api");
        assert_eq!(source_colour(&source), source_colour(&source));
        // The container does not change the colour: a pod is one colour, even
        // when two of its containers are being read.
        assert_eq!(
            source_colour(&source),
            source_colour(&LogSource::new("api-0", "sidecar"))
        );
    }

    #[test]
    fn different_pods_usually_get_different_colours() {
        let colours: std::collections::HashSet<_> = (0..8)
            .map(|index| {
                format!(
                    "{:?}",
                    source_colour(&LogSource::new(format!("api-{index}"), "api"))
                )
            })
            .collect();
        // Not a guarantee — it is a hash — but a run of eight pods collapsing
        // to one colour would make a merged stream unreadable.
        assert!(colours.len() >= 4, "{colours:?}");
    }

    #[test]
    fn timestamps_render_as_wall_clock_time() {
        let midnight = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        assert_eq!(format_time(midnight), "00:00:00");
        assert_eq!(
            format_time(midnight + Duration::from_secs(3_661)),
            "01:01:01"
        );
    }
}
