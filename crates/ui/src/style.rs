//! The visual vocabulary: one place that decides what things look like.
//!
//! Everything here is derived from the running theme rather than written as a
//! literal colour, so light and dark stay coherent and a theme change moves the
//! whole app at once. What the tokens add on top is *hierarchy*: the theme
//! gives a foreground colour, and this decides that a resource's name is read
//! first, its namespace second and its age last.
//!
//! The rule for using them: no view sets a colour or a spacing by hand. If
//! something needs a value that is not here, it belongs here.

use gpui::{App, Hsla};
use gpui_component::ActiveTheme as _;

/// Height of one table row.
///
/// Dense on purpose. This is a console for scanning thousands of objects, and
/// every pixel of row height is a row that does not fit on the screen; 24
/// leaves a comfortable line for 13px text without the airiness of a settings
/// dialog.
pub const ROW_HEIGHT: f32 = 24.;

/// Height of one row in the sidebar's lists.
pub const ITEM_HEIGHT: f32 = 26.;

/// The corner radius used everywhere except pills.
pub const RADIUS: f32 = 6.;

/// Width of the marker strip down the left of a row.
pub const MARKER: f32 = 2.;

/// The text somebody is actually reading: a resource's name, a log line.
pub fn text(cx: &App) -> Hsla {
    cx.theme().foreground
}

/// Supporting text: the namespace beside a name, a column heading.
///
/// Opacity rather than a second colour, so it stays legible against whatever
/// the row's background happens to be — hovered, selected or plain.
pub fn text_dim(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.62)
}

/// Text that is present but should never compete: ages, counts, hints.
pub fn text_faint(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.40)
}

/// The one colour that means "this is where you are".
///
/// The theme's own `primary` is near-white in dark mode and near-black in
/// light, which is correct for a button and useless as an accent: it left the
/// application without a single hue in it. This is the blue from the app icon,
/// darkened for light backgrounds so it stays legible against white.
pub fn accent(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        gpui::hsla(199. / 360., 0.90, 0.68, 1.0)
    } else {
        gpui::hsla(201. / 360., 0.90, 0.40, 1.0)
    }
}

/// A line between things, when a line is genuinely needed.
///
/// Half-strength: a table that draws a full border under every row turns into
/// a grid, and the eye then reads the grid instead of the rows.
pub fn hairline(cx: &App) -> Hsla {
    cx.theme().border.opacity(0.6)
}

/// The wash under whatever the pointer is over.
pub fn hover(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.06)
}

/// The wash under the row the keyboard is on, or the selected item.
///
/// Tinted with the accent rather than plain grey, so "where I am" and "what the
/// pointer is over" are told apart by hue instead of by two shades of the same
/// grey.
pub fn selected(cx: &App) -> Hsla {
    accent(cx).opacity(if cx.theme().is_dark() { 0.20 } else { 0.14 })
}

/// A surface that sits above the background: a panel, a menu, a toolbar.
pub fn raised(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.04)
}

/// The colour a row's state reads as.
///
/// Colour is reserved for this. Nothing else in a table is coloured, so
/// anything coloured means something is wrong or changing.
pub fn state(state: periscope_bridge::RowState, cx: &App) -> Hsla {
    match state {
        periscope_bridge::RowState::Healthy => cx.theme().success,
        periscope_bridge::RowState::Transient => cx.theme().warning,
        periscope_bridge::RowState::Failing => cx.theme().danger,
        periscope_bridge::RowState::Neutral => text_dim(cx),
    }
}

/// How long a transition takes.
///
/// Short enough that it never delays the answer to a keystroke, long enough to
/// be seen: the point of the movement is to say *what changed*, not to be
/// noticed as an animation.
pub const TRANSITION: std::time::Duration = std::time::Duration::from_millis(140);

/// How long a row stays highlighted after it changes.
///
/// A watch stream rewrites rows underneath a reader with no other signal that
/// anything happened. Long enough to catch the eye on a quiet table, short
/// enough that a busy one does not turn into a light show.
pub const FLASH: std::time::Duration = std::time::Duration::from_millis(900);

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_text_roles_are_ordered_by_prominence(cx: &mut TestAppContext) {
        // The whole point of having three: a name must not read as quietly as
        // the age beside it.
        cx.update(|cx| {
            gpui_component::init(cx);

            assert!(text(cx).a > text_dim(cx).a);
            assert!(text_dim(cx).a > text_faint(cx).a);
        });
    }

    #[gpui::test]
    fn washes_are_subtle_enough_to_read_through(cx: &mut TestAppContext) {
        // A selection that hides the text it selects is a worse selection.
        cx.update(|cx| {
            gpui_component::init(cx);

            assert!(hover(cx).a < 0.2, "{:?}", hover(cx));
            assert!(selected(cx).a < 0.35, "{:?}", selected(cx));
        });
    }

    #[gpui::test]
    fn a_failing_row_does_not_read_as_a_healthy_one(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);

            let healthy = state(periscope_bridge::RowState::Healthy, cx);
            let failing = state(periscope_bridge::RowState::Failing, cx);
            assert_ne!(healthy, failing);
        });
    }
}
