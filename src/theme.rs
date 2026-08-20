//! Centralized colors and fonts. Two palettes (One Light-ish / One Dark-ish)
//! selected by a runtime theme mode.

use gpui::{px, rgb, App, Font, FontFeatures, FontStyle, FontWeight, Global, Hsla, Pixels, SharedString};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

// A process-wide flag rather than a gpui Global so the plain color fns below
// (called from ~100 sites, including element paint code) keep their no-arg
// signatures.
static DARK: AtomicBool = AtomicBool::new(false);

pub fn set_mode(mode: ThemeMode) {
    DARK.store(mode == ThemeMode::Dark, Ordering::Relaxed);
}

pub fn mode() -> ThemeMode {
    if dark() {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

fn dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

fn pick(dark_hex: u32, light_hex: u32) -> Hsla {
    rgb(if dark() { dark_hex } else { light_hex }).into()
}

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
    pick(0x21252b, 0xf2f3f5)
}
pub fn panel_bg() -> Hsla {
    pick(0x1b1f23, 0xe9eaee)
}
pub fn terminal_bg() -> Hsla {
    pick(0x282c33, 0xfafafa)
}
pub fn terminal_selection_bg() -> Hsla {
    pick(0x3e4f6f, 0xbfd4ef)
}
pub fn fg() -> Hsla {
    pick(0xabb2bf, 0x383a42)
}
pub fn fg_dim() -> Hsla {
    pick(0x6b717d, 0x8a8f98)
}
pub fn border() -> Hsla {
    pick(0x3f4451, 0xd0d3d9)
}
pub fn accent() -> Hsla {
    pick(0x4aa5f0, 0x2b6fd0)
}
pub fn selected_bg() -> Hsla {
    pick(0x2c313a, 0xdde2ea)
}
pub fn hover_bg() -> Hsla {
    pick(0x323842, 0xe2e6ec)
}
pub fn error() -> Hsla {
    pick(0xe05561, 0xca1243)
}
pub fn ok() -> Hsla {
    pick(0x8cc265, 0x3f8b3f)
}
pub fn warn() -> Hsla {
    pick(0xd18f52, 0xbf6c00)
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

