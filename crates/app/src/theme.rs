//! The look of the window: the colors of both modes and the small elements every view uses.

use gpui_kit::component::{ActiveTheme, Icon, Theme, ThemeMode};
use gpui_kit::{App, Div, Hsla, IntoElement, ParentElement, SharedString, Styled, Window, div, px, rgb};

/// The colors that the component theme has no name for.
pub struct Palette {
    /// Cards and panels, one step above the background.
    pub surface: Hsla,
    /// Blocks of literal text, such as the arguments of a call.
    pub code: Hsla,
    /// The dot and the text of a state that needs the owner.
    pub waiting: Hsla,
    pub done: Hsla,
    pub stopped: Hsla,
}

pub fn palette(cx: &App) -> Palette {
    if cx.theme().is_dark() {
        Palette {
            surface: rgb(0x1d2129).into(),
            code: rgb(0x14171d).into(),
            waiting: rgb(0xf0b849).into(),
            done: rgb(0x5cc98f).into(),
            stopped: rgb(0xf0766e).into(),
        }
    } else {
        Palette {
            surface: rgb(0xffffff).into(),
            code: rgb(0xf3f4f7).into(),
            waiting: rgb(0xb26a00).into(),
            done: rgb(0x1f8a55).into(),
            stopped: rgb(0xc4372e).into(),
        }
    }
}

/// Sets the colors of `mode`. The accent is the blue of signing ink: approving is signing.
pub fn apply(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(mode, window, cx);
    // The mode change loads the library's colors. These edits come after it.
    Theme::update(cx, |theme| {
        theme.font_size = px(14.);
        theme.mono_font_size = px(12.5);
        theme.radius = px(7.);
        theme.radius_lg = px(11.);
        let colors = &mut theme.colors;
        let (background, ink, muted_ink, border, sidebar, subtle, accent, accent_hover, on_accent) = if mode.is_dark() {
            (
                0x15181e, 0xe7e9ee, 0x99a1b0, 0x2c313b, 0x101216, 0x232831, 0x8298ff, 0x97a9ff, 0x0f1320,
            )
        } else {
            (
                0xf5f6f8, 0x1a1f2b, 0x636b7a, 0xdfe2e8, 0xeceef2, 0xe9ebf0, 0x2743c4, 0x1f37a6, 0xffffff,
            )
        };
        colors.background = rgb(background).into();
        colors.foreground = rgb(ink).into();
        colors.muted = rgb(subtle).into();
        colors.muted_foreground = rgb(muted_ink).into();
        colors.border = rgb(border).into();
        colors.input = rgb(border).into();
        colors.sidebar = rgb(sidebar).into();
        colors.sidebar_border = rgb(border).into();
        colors.sidebar_foreground = rgb(ink).into();
        colors.sidebar_accent = rgb(subtle).into();
        colors.primary = rgb(accent).into();
        colors.primary_hover = rgb(accent_hover).into();
        colors.primary_active = rgb(accent_hover).into();
        colors.primary_foreground = rgb(on_accent).into();
        colors.button_primary = rgb(accent).into();
        colors.button_primary_hover = rgb(accent_hover).into();
        colors.button_primary_active = rgb(accent_hover).into();
        colors.button_primary_foreground = rgb(on_accent).into();
        colors.ring = rgb(accent).into();
    });
}

/// The mode the system asks for now.
pub fn system_mode(window: &Window) -> ThemeMode {
    window.appearance().into()
}

/// A card: the frame of one thing the owner reads or decides.
pub fn card(cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(palette(cx).surface)
}

/// Secondary text: smaller and quieter than the text it explains.
pub fn quiet(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A block of literal text. It wraps and it is never cut, so the owner reads all of it.
pub fn code_block(cx: &App) -> Div {
    div()
        .p_3()
        .rounded(cx.theme().radius)
        .bg(palette(cx).code)
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(cx.theme().mono_font_size)
}

/// A notice the owner must see before going on: tinted with `color`, with room for buttons.
pub fn notice(color: Hsla, cx: &App) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.12))
        .text_sm()
}

/// A form field with its name above it.
pub fn field(name: &'static str, input: impl IntoElement, cx: &App) -> Div {
    div().flex().flex_col().gap_1().child(quiet(name, cx)).child(input)
}

/// A state as a colored dot and a word.
pub fn state_mark(text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .text_sm()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(color)
        .child(div().size(px(7.)).rounded_full().bg(color))
        .child(text.into())
}

/// An icon of the bundled set, by file name without the extension.
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(format!("icons/{name}.svg"))
}

/// A time of the server (seconds since 1970) as `YYYY-MM-DD HH:MM` in UTC.
pub fn utc_minute(at: i64) -> String {
    let (year, month, day, hour, minute, _) = bunsin_core::time::utc_parts(at);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::utc_minute;

    #[test]
    fn a_server_time_is_shown_as_a_utc_date_and_minute() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00");
        assert_eq!(utc_minute(1_000), "1970-01-01 00:16");
        // A leap day, and the last minute of a year.
        assert_eq!(utc_minute(1_709_210_096), "2024-02-29 12:34");
        assert_eq!(utc_minute(1_767_225_599), "2025-12-31 23:59");
        assert_eq!(utc_minute(1_767_225_600), "2026-01-01 00:00");
        // A time before 1970 does not panic.
        assert_eq!(utc_minute(-60), "1969-12-31 23:59");
    }
}
