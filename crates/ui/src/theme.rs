//! Mapping between Periscope's UI-agnostic theme setting and gpui-component.
//!
//! `periscope-config` must not depend on GPUI, so the translation lives here.

use gpui::{App, Window};
use gpui_component::theme::{Theme, ThemeMode};
use periscope_config::ThemeChoice;

/// Applies a theme choice to the running app.
///
/// `System` re-reads the OS appearance rather than pinning a mode, so the app
/// keeps following the system after the user cycles back to it.
pub fn apply(choice: ThemeChoice, window: Option<&mut Window>, cx: &mut App) {
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(window, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Whether the app is currently rendering dark.
pub fn is_dark(cx: &App) -> bool {
    Theme::global(cx).is_dark()
}
