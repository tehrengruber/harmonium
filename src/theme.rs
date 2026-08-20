//! Centralized colors and fonts (One Dark-ish palette).

use gpui::{px, rgb, App, Font, FontFeatures, FontStyle, FontWeight, Global, Hsla, Pixels, SharedString};

/// Base font size, adjustable at runtime from the settings panel and stored
/// as a GPUI global so render code anywhere (including the terminal element)
/// can read it.
pub struct FontSettings {
    pub base: f32,
}

impl Global for FontSettings {}

pub const DEFAULT_FONT_SIZE: f32 = 12.;
pub const MIN_FONT_SIZE: f32 = 8.;
pub const MAX_FONT_SIZE: f32 = 24.;

pub fn base_font_size(cx: &App) -> f32 {
    cx.try_global::<FontSettings>()
        .map(|s| s.base)
        .unwrap_or(DEFAULT_FONT_SIZE)
        .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// UI text size (sidebar, dialogs, headers).
pub fn ui_font_size(cx: &App) -> Pixels {
    px(base_font_size(cx))
}

/// Terminal cell font size: slightly larger than the UI text.
pub fn terminal_font_size(cx: &App) -> Pixels {
    px(base_font_size(cx) + 1.)
}

pub fn bg() -> Hsla {
    rgb(0x21252b).into()
}
pub fn panel_bg() -> Hsla {
    rgb(0x1b1f23).into()
}
pub fn terminal_bg() -> Hsla {
    rgb(0x282c33).into()
}
pub fn terminal_selection_bg() -> Hsla {
    rgb(0x3e4f6f).into()
}
pub fn fg() -> Hsla {
    rgb(0xabb2bf).into()
}
pub fn fg_dim() -> Hsla {
    rgb(0x6b717d).into()
}
pub fn border() -> Hsla {
    rgb(0x3f4451).into()
}
pub fn accent() -> Hsla {
    rgb(0x4aa5f0).into()
}
pub fn selected_bg() -> Hsla {
    rgb(0x2c313a).into()
}
pub fn hover_bg() -> Hsla {
    rgb(0x323842).into()
}
pub fn error() -> Hsla {
    rgb(0xe05561).into()
}
pub fn ok() -> Hsla {
    rgb(0x8cc265).into()
}
pub fn warn() -> Hsla {
    rgb(0xd18f52).into()
}

pub fn ui_font() -> Font {
    Font {
        family: SharedString::from(
            std::env::var("HARMONIUM_UI_FONT").unwrap_or_else(|_| "DejaVu Sans".into()),
        ),
        features: FontFeatures::default(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        fallbacks: None,
    }
}

pub fn terminal_font() -> Font {
    Font {
        family: SharedString::from(
            std::env::var("HARMONIUM_TERMINAL_FONT").unwrap_or_else(|_| "DejaVu Sans Mono".into()),
        ),
        features: FontFeatures::default(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        fallbacks: None,
    }
}

