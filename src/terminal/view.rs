//! Focusable view wrapping a [`Terminal`] entity: routes keyboard input to the
//! PTY and renders the grid via [`TerminalElement`].

use gpui::{
    div, App, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyBinding, KeyDownEvent, MouseButton, NoAction, ParentElement as _, Render, Styled as _,
    Window,
};

use super::element::TerminalElement;
use super::Terminal;
use crate::theme;

/// Key context of a focused terminal. It sits deeper in the dispatch tree than
/// the window root, so bindings scoped to it outrank the ones the component
/// library installs there — see [`init`].
pub const KEY_CONTEXT: &str = "Terminal";

/// Free `tab` and `shift-tab` for the shell. `gpui_component::init` binds both
/// on the window root to move focus, and key bindings are matched *before* any
/// `on_key_down` listener, so a focused terminal never saw the key at all and
/// completion did nothing. Disabling them in the terminal's own context — the
/// deeper, higher-precedence one — lets the keystroke fall through to
/// [`TerminalView::on_key_down`] and out to the PTY. Call after the component
/// library's own `init`.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(KEY_CONTEXT)),
    ]);
}

pub struct TerminalView {
    pub terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut Context<Self>) -> Self {
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;

        // Paste with ctrl-shift-v (the terminal ignores ctrl-shift chords).
        if keystroke.modifiers.control
            && keystroke.modifiers.shift
            && keystroke.key == "v"
        {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.terminal.update(cx, |terminal, _| terminal.paste(&text));
            }
            cx.stop_propagation();
            return;
        }

        // Copy the mouse selection with ctrl-shift-c.
        if keystroke.modifiers.control
            && keystroke.modifiers.shift
            && keystroke.key == "c"
        {
            let selected = self
                .terminal
                .update(cx, |terminal, _| terminal.selection_text());
            if let Some(text) = selected {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }

        let handled = self
            .terminal
            .update(cx, |terminal, cx| terminal.try_keystroke(keystroke, cx));
        if handled {
            cx.stop_propagation();
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let focus_handle = self.focus_handle.clone();

        div()
            .id("terminal-view")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme::terminal_bg())
            .p_2()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, _| {
                    window.focus(&focus_handle);
                }),
            )
            .child(TerminalElement::new(self.terminal.clone(), focused))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::prelude::FluentBuilder as _;
    use gpui::{AppContext as _, Render, Window};
    use std::cell::Cell;
    use std::rc::Rc;

    /// A stand-in for [`TerminalView`]'s keyboard half — same key context, same
    /// listener — so the test needs no PTY to ask the question that matters:
    /// does `tab` reach the listener, or does the window root eat it first?
    struct KeyProbe {
        focus_handle: FocusHandle,
        context: Option<&'static str>,
        seen: Rc<Cell<usize>>,
    }

    impl Render for KeyProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let seen = self.seen.clone();
            div()
                .id("probe")
                .when_some(self.context, |this, context| this.key_context(context))
                .track_focus(&self.focus_handle)
                .size_full()
                .on_key_down(move |_: &KeyDownEvent, _, _| seen.set(seen.get() + 1))
        }
    }

    /// Presses `tab` on a focused element in `context` and reports how many
    /// key-down events the element saw. The probe is mounted under
    /// [`gpui_component::Root`], as it is in the real window — the library's
    /// focus-cycling binding is scoped to the root's key context, so a window
    /// without one would never eat the key and the test would pass on its own.
    fn tabs_seen(cx: &mut gpui::TestAppContext, context: Option<&'static str>) -> usize {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let seen = Rc::new(Cell::new(0));
        let probe = seen.clone();
        let holder: Rc<std::cell::RefCell<Option<FocusHandle>>> = Rc::default();
        let keep = holder.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let focus_handle = cx.focus_handle();
                *keep.borrow_mut() = Some(focus_handle.clone());
                KeyProbe {
                    focus_handle,
                    context,
                    seen: probe,
                }
            });
            gpui_component::Root::new(view, window, cx)
        });
        let focus_handle = holder.borrow().clone().expect("the probe was built");
        cx.update(|window, _| window.focus(&focus_handle));
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        seen.get()
    }

    /// The bug: the component library binds `tab` on the window root to cycle
    /// focus, bindings beat `on_key_down`, and the shell never got its byte.
    #[gpui::test]
    fn tab_reaches_a_focused_terminal(cx: &mut gpui::TestAppContext) {
        assert_eq!(
            tabs_seen(cx, Some(KEY_CONTEXT)),
            1,
            "tab was swallowed before the terminal could send it to the shell"
        );
    }

    /// The other half of the pair: outside the terminal's context `tab` is
    /// still focus navigation, so the fix is scoped and not a global unbind.
    #[gpui::test]
    fn tab_still_moves_focus_elsewhere(cx: &mut gpui::TestAppContext) {
        assert_eq!(
            tabs_seen(cx, None),
            0,
            "tab stopped being focus navigation outside the terminal"
        );
    }
}
