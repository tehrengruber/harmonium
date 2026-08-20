//! Focusable view wrapping a [`Terminal`] entity: routes keyboard input to the
//! PTY and renders the grid via [`TerminalElement`].

use gpui::{
    div, App, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Render, Styled as _, Window,
};

use super::element::TerminalElement;
use super::Terminal;
use crate::theme;

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
