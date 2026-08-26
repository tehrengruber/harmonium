//! Centralized colors and fonts. Two palettes (One Light-ish / One Dark-ish)
//! selected by a runtime theme mode.

use gpui::{
    px, rgb, App, Font, FontFeatures, FontStyle, FontWeight, Global, Hsla, Pixels, Rems,
    SharedString,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

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

pub const DEFAULT_UI_FONT: &str = "DejaVu Sans";
pub const DEFAULT_TERMINAL_FONT: &str = "DejaVu Sans Mono";

/// Font families from the persisted settings. Same reasoning as `DARK`: the
/// font accessors are called from paint code that has no settings handle.
static FONTS: RwLock<Fonts> = RwLock::new(Fonts {
    ui: None,
    terminal: None,
});

struct Fonts {
    ui: Option<String>,
    terminal: Option<String>,
}

/// Apply the configured font families. Empty values fall back to the
/// defaults; the `HARMONIUM_*_FONT` variables still override both.
pub fn set_fonts(ui: &str, terminal: &str) {
    let clean = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    if let Ok(mut fonts) = FONTS.write() {
        fonts.ui = clean(ui);
        fonts.terminal = clean(terminal);
    }
}

/// Env var, then the persisted setting, then the built-in default.
fn font_family(var: &str, configured: Option<String>, default: &str) -> SharedString {
    let from_env = std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    SharedString::from(
        from_env
            .or(configured)
            .unwrap_or_else(|| default.to_string()),
    )
}

fn font(family: SharedString) -> Font {
    Font {
        family,
        features: FontFeatures::default(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        fallbacks: None,
    }
}

/// Switch the palette, library included. The two are one setting: a dialog
/// is our text on gpui-component's panel, so a mode that reaches only one of
/// them is unreadable — which is what a saved dark theme used to look like,
/// the library left on the light mode `init` gave it because the session's
/// mode was applied after the one call that synced it.
pub fn set_mode(mode: ThemeMode, cx: &mut App) {
    DARK.store(mode == ThemeMode::Dark, Ordering::Relaxed);
    sync_component_theme(cx);
}

/// Point gpui-component's semantic theme at the same light/dark mode as our
/// own palette, and re-resolve the metrics below. [`set_mode`] does this for
/// you; call it directly only to align the library with what is already in
/// force — at startup, or after the font size moves, which changes the rem
/// the radius is resolved from.
pub fn sync_component_theme(cx: &mut App) {
    let mode = match mode() {
        ThemeMode::Dark => gpui_component::ThemeMode::Dark,
        ThemeMode::Light => gpui_component::ThemeMode::Light,
    };
    gpui_component::Theme::change(mode, None, cx);
    // After `change`, and again on every call: it ends in `apply_config`,
    // which rewrites each metric from a freshly built default, so a radius
    // set here is stamped back to the library's 6px/8px by the next theme
    // toggle. Resolved to pixels because that is what its theme holds; the
    // fraction is the one our own chrome rounds by, so a library widget and
    // the chip beside it match.
    //
    // TODO(upstream): report to gpui-component (0.5.1,
    // github.com/longbridge/gpui-component) — the theme owns metrics an app
    // has no supported way to set.
    //
    //  * `radius`/`radius_lg` can only come from a theme *file*, as a whole
    //    number of pixels (`ThemeConfig::radius: Option<usize>`). There is
    //    no API for "round everything by this", and an assignment to the
    //    global does not survive `Theme::change`, which is the very call an
    //    app makes to follow the system appearance.
    //  * `Theme` carries `font_size` but no line height, while `Input`
    //    hard-codes its own — see [`CONTROL_LINE_HEIGHT`].
    //
    // Both decide how tall a control comes out, so an app drawing its own
    // chrome beside the library's widgets has to guess at them to line
    // anything up. Until they are settable, this override stands.
    let radius = corner_radius(cx);
    let theme = gpui_component::Theme::global_mut(cx);
    theme.radius = radius;
    theme.radius_lg = radius;
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

/// The rem the window is scaled to, and the one scale every size in the UI
/// is drawn from — text through the sizes below, spacing and icons through
/// gpui's own rem units (`px_2`, `size_3`, `text_sm`). One scale is the point:
/// a size defined outside it keeps its own pace as the setting moves, and the
/// UI comes apart at every setting except the one it was drawn at.
pub fn rem_size(cx: &App) -> Pixels {
    px(base_font_size(cx) * 16. / DEFAULT_FONT_SIZE)
}

/// UI text size (sidebar, dialogs, headers) — the same step as `text_sm`,
/// which is what nearly every element in the app asks for. Set as the window's
/// default too, so text that names no size of its own matches the text it sits
/// beside instead of coming out a step smaller.
pub fn ui_font_size(cx: &App) -> Pixels {
    rem_size(cx) * 0.875
}

/// Small-label text — sidebar group headers and the like. Under the UI size
/// so a label reads as a label rather than as another row, with a floor so it
/// stays legible when the whole UI is scaled down.
pub fn label_font_size(cx: &App) -> Pixels {
    px(f32::from(rem_size(cx) * 0.625).max(MIN_FONT_SIZE))
}

/// Terminal cell font size: a step under the UI text, where a monospace face
/// of the same nominal size reads larger.
pub fn terminal_font_size(cx: &App) -> Pixels {
    rem_size(cx) * 0.8125
}

/// Corner radius, for everything the app rounds. One step off a square
/// corner and no further — 2px at the default font size — so a rounded box
/// reads as a box with its edge taken off rather than as a pill. In rems
/// like every other size here, so it keeps its proportion to the text as the
/// font-size setting moves.
pub const CORNER_RADIUS: Rems = Rems(0.125);

/// The line box a one-line control draws its text in — chips, buttons and
/// text fields alike. Matching it is what makes a chip exactly as tall as
/// the field beside it; see [`crate::ui`].
///
/// The number is a copy of a private `const LINE_HEIGHT: Rems = Rems(1.25)`
/// inside gpui-component's `Input::render`, not a value we are free to pick:
/// a gpui `div` defaults to `phi()` instead, which at our text size is a
/// 23px line box against the field's 20px. Nothing here fails to compile if
/// they change it upstream — the chips just quietly stop matching the
/// fields. See the TODO in [`sync_component_theme`].
pub const CONTROL_LINE_HEIGHT: Rems = Rems(1.25);

/// [`CORNER_RADIUS`] resolved against the current rem, for gpui-component's
/// theme, which holds pixels rather than a rem fraction.
pub fn corner_radius(cx: &App) -> Pixels {
    CORNER_RADIUS.to_pixels(rem_size(cx))
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
/// A hairline weaker than [`border`], for rules that divide *within* a panel
/// rather than between panels — a 1px line is already the thinnest thing the
/// display can draw, so the only way left to make one lighter is its colour.
pub fn rule() -> Hsla {
    pick(0x2f343c, 0xdcdee3)
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
    let configured = FONTS.read().ok().and_then(|fonts| fonts.ui.clone());
    font(font_family("HARMONIUM_UI_FONT", configured, DEFAULT_UI_FONT))
}

pub fn terminal_font() -> Font {
    let configured = FONTS.read().ok().and_then(|fonts| fonts.terminal.clone());
    font(font_family(
        "HARMONIUM_TERMINAL_FONT",
        configured,
        DEFAULT_TERMINAL_FONT,
    ))
}



#[cfg(test)]
mod tests {
    use super::*;

    /// Every text size in the UI is a fixed fraction of one rem, so moving
    /// the font-size setting moves all of them by the same factor. A size
    /// defined outside that scale is how a UI comes apart at every setting
    /// except the one someone happened to draw it at.
    #[gpui::test]
    fn text_sizes_all_move_with_the_setting(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let sizes = |base: f32, cx: &mut App| {
                cx.set_global(FontSettings { base });
                [rem_size(cx), ui_font_size(cx), terminal_font_size(cx), label_font_size(cx)]
            };

            // At the default setting the app draws what it has always drawn,
            // and `ui_font_size` is the step `text_sm` resolves to (0.875rem)
            // rather than a second opinion about how big UI text is.
            assert_eq!(
                sizes(DEFAULT_FONT_SIZE, cx),
                [px(16.), px(14.), px(13.), px(10.)]
            );

            // Double the setting and every size doubles with it.
            assert_eq!(
                sizes(DEFAULT_FONT_SIZE * 2., cx),
                [px(32.), px(28.), px(26.), px(20.)]
            );
        });
    }

    /// The radius gpui-component's widgets round by is the one place in the
    /// UI where a size leaves our rem scale and becomes fixed pixels, so it
    /// has to be re-resolved whenever the rem moves — and it has to survive
    /// `Theme::change`, which rewrites every metric from the library's own
    /// defaults. Both halves have gone wrong in a way nothing else catches:
    /// the widgets keep rounding by a stale radius while the chrome beside
    /// them scales, which reads as a rendering glitch, not a bug.
    #[gpui::test]
    fn component_radius_follows_the_font_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            let radius = |base: f32, cx: &mut App| {
                cx.set_global(FontSettings { base });
                sync_component_theme(cx);
                let theme = gpui_component::Theme::global(cx);
                (theme.radius, theme.radius_lg)
            };

            assert_eq!(radius(DEFAULT_FONT_SIZE, cx), (px(2.), px(2.)));
            assert_eq!(radius(DEFAULT_FONT_SIZE * 2., cx), (px(4.), px(4.)));

            // Not the library's own 6px/8px, which is what a `change` after
            // the override — or no override at all — leaves behind.
            let (r, r_lg) = radius(DEFAULT_FONT_SIZE, cx);
            assert!(r != px(6.) && r_lg != px(8.));
        });
    }
}
