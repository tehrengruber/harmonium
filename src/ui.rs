//! The metric every one-line control is drawn on.
//!
//! Chips, buttons and text fields are one family: the same padding, the same
//! line box, the same 1px border and the same corner radius. Only colour
//! tells them apart, so a chip standing beside a field is exactly as tall as
//! the field and the row they share doesn't jog.
//!
//! gpui-component's `Input` picks its padding from a `Size` whose steps are
//! fixed pixels — none of them our spacing, and none of them moving with the
//! font-size setting — so [`field`] restates the metric on top of it rather
//! than choosing whichever `Size` comes closest.

use gpui::{
    div, transparent_black, Div, ElementId, Entity, InteractiveElement as _, Stateful,
    Styled as _,
};
use gpui_component::input::{Input, InputState};

use crate::theme;

/// A one-line control — chip, toggle, button. What is fixed here is only the
/// box; the colours, the hover and the click belong to the caller, and are
/// the whole of what says which kind of control this is.
pub fn control(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .px_1p5()
        .py_0p5()
        // Bordered even where nothing is drawn: a 1px line present on some
        // controls and absent on others is a 2px height difference between
        // two of them in the same row.
        .border_1()
        .border_color(transparent_black())
        .rounded(theme::CORNER_RADIUS)
        .text_sm()
        .line_height(theme::CONTROL_LINE_HEIGHT)
        .cursor_pointer()
}

/// A text field on that same metric. `Input` refines the caller's style
/// last, so these replace its own padding rather than adding to it —
/// including its fixed 32px height, which is the whole reason a field used
/// to tower over the chips under it. Left to size itself, it comes out as
/// the line box plus this padding, which is what [`control`] is.
pub fn field(state: &Entity<InputState>) -> Input {
    Input::new(state).px_1p5().py_0p5().h_auto()
}
