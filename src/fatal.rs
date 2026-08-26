//! The window harmonium puts up when it can't start.
//!
//! Startup refusals — a state file that won't parse, a session another
//! harmonium already holds — happen before there is anything to show them in,
//! and the message goes to a terminal that in the usual case nobody is looking
//! at: started from a launcher or a desktop file there isn't one at all, so the
//! app would simply not appear. This gives that message a window of its own.
//! It is deliberately its own little application: there is no session behind it
//! and nothing it could do but say why and end.

use gpui::{
    div, px, size, App, AppContext as _, Application, Bounds, Context, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, TitlebarOptions, Window, WindowBounds, WindowOptions,
};

use crate::theme;

struct Fatal {
    headline: SharedString,
    paragraphs: Vec<SharedString>,
    /// Only here so the window can have the keyboard: without focus somewhere
    /// the escape/enter shortcut never fires and the button is the only way out.
    focus_handle: FocusHandle,
}

impl Fatal {
    fn new(
        headline: SharedString,
        paragraphs: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            headline,
            paragraphs,
            focus_handle,
        }
    }
}

impl Render for Fatal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Same rem scaling as the workspace, so this window's text matches the
        // rest of the app at whatever font size is configured.
        window.set_rem_size(theme::rem_size(cx));

        div()
            .id("fatal")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(theme::bg())
            .text_color(theme::fg())
            .text_size(theme::ui_font_size(cx))
            .font_family(theme::ui_font().family.clone())
            // The window says one thing and has one button; both keys people
            // reach for to dismiss it do what the button does.
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "escape" | "enter") {
                    cx.quit();
                }
            }))
            .child(
                // A block of its own, centred and no wider than a readable
                // line: the window is sized for this text, but a tiling
                // compositor will hand it the whole screen anyway, and the
                // message shouldn't stretch across it or leave the button
                // stranded in a far corner.
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w_full()
                    .max_w(px(520.))
                    .child(
                        div()
                            .text_color(theme::error())
                            .text_lg()
                            .child(self.headline.clone()),
                    )
                    .children(
                        self.paragraphs
                            .iter()
                            .map(|text| div().text_color(theme::fg_dim()).child(text.clone())),
                    )
                    .child(
                        div().flex().justify_end().pt_2().child(
                            div()
                                .id("fatal-quit")
                                .px_3()
                                .py_1()
                                .rounded_sm()
                                .bg(theme::accent())
                                .text_color(theme::panel_bg())
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.9))
                                .on_click(|_, _, cx| cx.quit())
                                .child("Quit"),
                        ),
                    ),
            )
    }
}

/// Show `headline` and `paragraphs`, and return once the window is dismissed.
///
/// Callers print the same text to stderr *first*: this runs a whole gpui
/// application, and on a machine that can't open a window at all it is the part
/// most likely to fail. Better a message already on the terminal than a panic
/// in place of one.
pub fn show(headline: impl Into<SharedString>, paragraphs: Vec<SharedString>) {
    let headline = headline.into();
    Application::new()
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::sync_component_theme(cx);

            // Closing the window is the only way out of here, so it has to end
            // the process — deferred a tick for the same reason as in `main`:
            // quitting inside gpui's window teardown re-enters a borrow the
            // teardown itself holds.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.spawn(async move |cx| {
                        cx.update(|cx| cx.quit()).ok();
                    })
                    .detach();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(560.), px(260.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Harmonium".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Fatal::new(headline, paragraphs, window, cx)),
            );
            // Nowhere to report this to but the terminal, which already has the
            // message this window was going to carry.
            if let Err(error) = window {
                eprintln!("harmonium: could not open a window for the message above ({error})");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}
