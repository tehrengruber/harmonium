//! Text input with full editing support: mouse selection, click-to-position,
//! shift-arrow selection, clipboard, and IME. Adapted from gpui's official
//! `input.rs` example, extended with a multiline mode (wrapping + newlines;
//! enter inserts a newline, ctrl-enter submits). Key bindings for the
//! `TextInput` key context are registered in `main.rs`.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, px, relative, size, App, AvailableSpace, Bounds, ClipboardItem,
    ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, InteractiveElement as _, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    ParentElement as _, Pixels, Point, Render, ScrollWheelEvent, SharedString, Style, Styled as _,
    TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::theme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Newline,
        Submit,
        Cancel,
    ]
);

#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Submitted(String),
    Cancelled,
}

/// Shaped lines of the displayed text plus enough information to map between
/// byte offsets and positions (relative to the text origin).
struct LayoutInfo {
    lines: Vec<WrappedLine>,
    /// Byte offset of each hard line's start within the displayed text.
    line_starts: Vec<usize>,
    line_height: Pixels,
}

impl LayoutInfo {
    fn line_block_height(&self, index: usize) -> Pixels {
        self.line_height * (self.lines[index].wrap_boundaries().len() + 1) as f32
    }

    fn total_height(&self) -> Pixels {
        (0..self.lines.len())
            .map(|i| self.line_block_height(i))
            .fold(px(0.), |a, b| a + b)
    }

    fn total_len(&self) -> usize {
        match (self.line_starts.last(), self.lines.last()) {
            (Some(start), Some(line)) => start + line.len(),
            _ => 0,
        }
    }

    fn position_for_index(&self, offset: usize) -> Option<Point<Pixels>> {
        let mut y = px(0.);
        for (i, line) in self.lines.iter().enumerate() {
            let start = self.line_starts[i];
            if offset >= start && offset <= start + line.len() {
                let local = line.position_for_index(offset - start, self.line_height)?;
                return Some(point(local.x, local.y + y));
            }
            y += self.line_block_height(i);
        }
        None
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        if position.y < px(0.) {
            return 0;
        }
        let mut y = px(0.);
        for (i, line) in self.lines.iter().enumerate() {
            let block_height = self.line_block_height(i);
            if position.y < y + block_height {
                let local = point(position.x.max(px(0.)), position.y - y);
                let index = match line.index_for_position(local, self.line_height) {
                    Ok(index) | Err(index) => index,
                };
                return self.line_starts[i] + index;
            }
            y += block_height;
        }
        self.total_len()
    }
}

/// Width of the caret quad. The visible area is kept this much wider than the
/// cursor position so a cursor at the very end of the text isn't half-clipped.
const CARET_WIDTH: Pixels = px(2.);

/// How far a single-line field can be scrolled: the text plus room for the
/// caret sitting just past its last glyph, minus what is already visible.
fn max_scroll(text_width: Pixels, field_width: Pixels) -> Pixels {
    (text_width + CARET_WIDTH - field_width).max(px(0.))
}

/// Height of a multiline field, in rows: it starts at the minimum and grows
/// with the text up to the maximum, after which it scrolls.
const MIN_MULTILINE_ROWS: usize = 3;
const MAX_MULTILINE_ROWS: usize = 12;

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    multiline: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<LayoutInfo>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    /// How far the view is scrolled away from the text's origin. Overflow is
    /// clipped, not shrunk, so this is what makes it reachable: the caret
    /// drags the view along, and the wheel moves it without moving the caret.
    /// Single-line fields scroll horizontally (they never wrap); multiline
    /// fields wrap and so scroll vertically once they hit their maximum
    /// height. Only ever one axis at a time; the other stays zero.
    scroll_offset: Point<Pixels>,
    /// Set whenever the caret moves or the text changes: the next paint scrolls
    /// it back into view. Cleared there, so a wheel scroll that deliberately
    /// looks away isn't yanked back on the following frame.
    scroll_to_cursor: bool,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self::build(placeholder, false, cx)
    }

    pub fn multiline(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self::build(placeholder, true, cx)
    }

    fn build(placeholder: impl Into<SharedString>, multiline: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            multiline,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            scroll_offset: point(px(0.), px(0.)),
            scroll_to_cursor: true,
        }
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let len = self.content.len();
        self.selected_range = len..len;
        self.marked_range = None;
        // Show the beginning even though the caret lands at the end: a field
        // filled in by the app (a preset command, a path) is there to be read
        // first, and any edit scrolls the caret back into view anyway.
        self.scroll_offset = point(px(0.), px(0.));
        self.scroll_to_cursor = false;
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submitted(self.content.to_string()));
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Cancelled);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn move_vertically(&mut self, row_delta: f32, cx: &mut Context<Self>) {
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let Some(position) = layout.position_for_index(self.cursor_offset()) else {
            return;
        };
        let target_y = position.y + layout.line_height * (row_delta + 0.5);
        if target_y < px(0.) {
            self.move_to(0, cx);
        } else if target_y > layout.total_height() {
            self.move_to(self.content.len(), cx);
        } else {
            let index = layout.index_for_position(point(position.x, target_y));
            self.move_to(index.min(self.content.len()), cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1., cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1., cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    /// Select everything, as reopening a search box does so the next
    /// keystroke replaces the old query.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        // Beginning of the current hard line in multiline mode.
        let offset = if self.multiline {
            self.content[..self.cursor_offset()]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            0
        };
        self.move_to(offset, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let offset = if self.multiline {
            let cursor = self.cursor_offset();
            self.content[cursor..]
                .find('\n')
                .map(|i| cursor + i)
                .unwrap_or(self.content.len())
        } else {
            self.content.len()
        };
        self.move_to(offset, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    /// Wheel over a single-line field pans the text horizontally, so overflow
    /// can be read (and clicked into) without first moving the caret. Either
    /// axis scrolls: a plain vertical wheel is the only one most mice have.
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        // Single-line fields have only one axis to give, so either wheel
        // direction drives it; multiline scrolls vertically like any text.
        let delta = if self.multiline {
            delta.y
        } else if delta.x == px(0.) {
            delta.y
        } else {
            delta.x
        };
        let current = self.scroll_along_axis();
        let target = (current - delta).clamp(px(0.), self.max_scroll_offset());
        if target == current {
            // Nothing to pan here — let the event bubble so the surrounding
            // dialog scrolls instead, which is what the user meant.
            return;
        }
        if self.multiline {
            self.scroll_offset.y = target;
        } else {
            self.scroll_offset.x = target;
        }
        self.scroll_to_cursor = false;
        // A field that *did* pan swallows the event: scrolling inside an input
        // must not also scroll the dialog behind it.
        cx.stop_propagation();
        cx.notify();
    }

    /// The scroll offset along whichever axis this field scrolls on.
    fn scroll_along_axis(&self) -> Pixels {
        if self.multiline {
            self.scroll_offset.y
        } else {
            self.scroll_offset.x
        }
    }

    /// Scroll limit from the last painted layout. Prepaint recomputes this from
    /// the fresh layout; this is for input handlers, which run between frames.
    fn max_scroll_offset(&self) -> Pixels {
        let (Some(layout), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds.as_ref())
        else {
            return px(0.);
        };
        if self.multiline {
            return (layout.total_height() - bounds.size.height).max(px(0.));
        }
        let text_width = layout
            .lines
            .first()
            .map(|line| line.size(layout.line_height).width)
            .unwrap_or_default();
        max_scroll(text_width, bounds.size.width)
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.scroll_to_cursor = true;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        layout.index_for_position(self.text_position(position, bounds))
    }

    /// Window position → position within the shaped text, undoing both the
    /// element's origin and the horizontal scroll.
    fn text_position(&self, position: Point<Pixels>, bounds: &Bounds<Pixels>) -> Point<Pixels> {
        position - bounds.origin + self.scroll_offset
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.scroll_to_cursor = true;
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.scroll_to_cursor = true;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.scroll_to_cursor = true;

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position_for_index(range.start)?;
        let end = layout.position_for_index(range.end)?;
        // Where the range is *painted*: the IME candidate window must follow
        // the scrolled text, not the unscrolled layout.
        let origin = bounds.origin - self.scroll_offset;
        Some(Bounds::from_corners(
            origin + start,
            origin + end + point(px(0.), layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let layout = self.last_layout.as_ref()?;
        let utf8_index = layout.index_for_position(self.text_position(point, &bounds));
        if utf8_index > self.content.len() {
            return None;
        }
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    layout: Option<LayoutInfo>,
    cursor: Option<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
    /// Where the first line is painted: the element origin shifted left by the
    /// horizontal scroll.
    text_origin: Point<Pixels>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Shape the displayed text (content or placeholder) into a LayoutInfo.
fn shape_layout(
    text: &SharedString,
    runs: &[TextRun],
    font_size: Pixels,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    window: &Window,
) -> LayoutInfo {
    let lines = window
        .text_system()
        .shape_text(text.clone(), font_size, runs, wrap_width, None)
        .unwrap_or_default()
        .into_vec();
    let mut line_starts = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in &lines {
        line_starts.push(offset);
        offset += line.len() + 1; // +1 for the '\n' separator
    }
    LayoutInfo {
        lines,
        line_starts,
        line_height,
    }
}

fn text_runs(
    display_text: &SharedString,
    text_color: gpui::Hsla,
    marked_range: Option<&Range<usize>>,
    window: &Window,
) -> Vec<TextRun> {
    let style = window.text_style();
    let run = TextRun {
        len: display_text.len(),
        font: style.font(),
        color: text_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    if let Some(marked_range) = marked_range {
        vec![
            TextRun {
                len: marked_range.start,
                ..run.clone()
            },
            TextRun {
                len: marked_range.end - marked_range.start,
                underline: Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: display_text.len() - marked_range.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|run| run.len > 0)
        .collect()
    } else {
        vec![run]
    }
}

impl gpui::Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let multiline = self.input.read(cx).multiline;
        if !multiline {
            style.size.height = window.line_height().into();
            return (window.request_layout(style, [], cx), ());
        }

        // Multiline: height depends on the wrapped content.
        let input = self.input.clone();
        let text_style = window.text_style();
        let layout_id = window.request_measured_layout(
            style,
            move |known_dimensions, available_space, window, cx| {
                let width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let input = input.read(cx);
                let display_text = if input.content.is_empty() {
                    input.placeholder.clone()
                } else {
                    input.content.clone()
                };
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                // From the *captured* style, not `window.line_height()`: this
                // closure runs during layout, outside the element's style
                // context, where the window would answer with the default
                // rem-based line height. Measuring rows taller than they are
                // painted left a growing band of dead space under the text.
                let line_height = text_style.line_height_in_pixels(window.rem_size());
                let min_height = line_height * MIN_MULTILINE_ROWS as f32;
                // Bounded by the rows *and* by the window, so a short window
                // doesn't hide the dialog's buttons behind a field that fits
                // its 12 rows.
                let max_height = (line_height * MAX_MULTILINE_ROWS as f32)
                    .min(window.viewport_size().height * 0.5)
                    .max(min_height);
                // The intrinsic-size passes (min/max-content) ask how wide
                // this wants to be with no width to wrap against. The answer
                // is "no wider than you like": the field wraps, so it has no
                // intrinsic width. Answering with the unwrapped text width
                // instead made the field's minimum grow with every word
                // typed, dragging the dialog wider and making its height jump
                // as the two layout passes disagreed about the wrap points.
                let Some(width) = width else {
                    return size(px(0.), min_height);
                };
                let runs = [TextRun {
                    len: display_text.len(),
                    font: text_style.font(),
                    color: text_style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }];
                // Grows with the text, but only so far: past the maximum the
                // field scrolls instead. Unbounded growth pushed the dialog's
                // buttons off the bottom, so a long task could be typed but
                // not submitted.
                let layout = shape_layout(
                    &display_text,
                    &runs,
                    font_size,
                    line_height,
                    Some(width),
                    window,
                );
                size(width, layout.total_height().clamp(min_height, max_height))
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let multiline = input.multiline;
        let marked_range = input.marked_range.clone();
        let mut scroll_offset = input.scroll_offset;
        let scroll_to_cursor = input.scroll_to_cursor;
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), theme::fg_dim())
        } else {
            (content, style.color)
        };

        let runs = text_runs(&display_text, text_color, marked_range.as_ref(), window);
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let wrap_width = multiline.then_some(bounds.size.width);
        let layout = shape_layout(
            &display_text,
            &runs,
            font_size,
            line_height,
            wrap_width,
            window,
        );

        // Overflow is panned, not shrunk, along whichever axis this field can
        // overflow on: single-line text never wraps and so runs off the right,
        // multiline wraps and so runs off the bottom once it has grown to its
        // maximum height.
        let caret = scroll_to_cursor
            .then(|| layout.position_for_index(cursor_offset))
            .flatten();
        if multiline {
            let limit = (layout.total_height() - bounds.size.height).max(px(0.));
            let mut offset = scroll_offset.y.clamp(px(0.), limit);
            if let Some(caret) = caret {
                if caret.y < offset {
                    offset = caret.y;
                } else if caret.y + line_height > offset + bounds.size.height {
                    offset = caret.y + line_height - bounds.size.height;
                }
            }
            scroll_offset = point(px(0.), offset.clamp(px(0.), limit));
        } else {
            let text_width = layout
                .lines
                .first()
                .map(|line| line.size(line_height).width)
                .unwrap_or_default();
            let limit = max_scroll(text_width, bounds.size.width);
            let mut offset = scroll_offset.x.clamp(px(0.), limit);
            if let Some(caret) = caret {
                if caret.x < offset {
                    offset = caret.x;
                } else if caret.x + CARET_WIDTH > offset + bounds.size.width {
                    offset = caret.x + CARET_WIDTH - bounds.size.width;
                }
            }
            scroll_offset = point(offset, px(0.));
        }
        let origin = bounds.origin - scroll_offset;
        self.input.update(cx, |input, _cx| {
            input.scroll_offset = scroll_offset;
            input.scroll_to_cursor = false;
        });

        let mut selection_quads = Vec::new();
        let mut cursor = None;
        if selected_range.is_empty() {
            if let Some(position) = layout.position_for_index(cursor_offset) {
                cursor = Some(fill(
                    Bounds::new(origin + position, size(CARET_WIDTH, line_height)),
                    theme::accent(),
                ));
            }
        } else {
            let mut selection_color = theme::accent();
            selection_color.a = 0.3;
            // One quad per visual row overlapped by the selection.
            let total_rows =
                (f32::from(layout.total_height() / line_height)).round().max(1.) as usize;
            for row in 0..total_rows {
                let row_top = line_height * row as f32;
                let row_mid = row_top + line_height * 0.5;
                let row_start = layout.index_for_position(point(px(0.), row_mid));
                let row_end = layout.index_for_position(point(px(f32::MAX), row_mid));
                let start = selected_range.start.max(row_start);
                let end = selected_range.end.min(row_end);
                if start > end || (start == end && !(row_start < selected_range.end && row_end > selected_range.start)) {
                    continue;
                }
                // An index on a wrap boundary belongs to two rows and is
                // reported on the *earlier* one, so asking for its x here
                // would place this row's highlight where the previous row
                // ended. Only an index that actually renders on this row can
                // give an x; otherwise the selection runs through the row and
                // the relevant edge is the row's own.
                let row_bottom = row_top + line_height;
                let x_on_row = |index: usize, edge: Pixels| {
                    layout
                        .position_for_index(index)
                        .filter(|p| p.y >= row_top && p.y < row_bottom)
                        .map_or(edge, |p| p.x)
                };
                let start_x = x_on_row(start, px(0.));
                let end_x = x_on_row(end, bounds.size.width);
                selection_quads.push(fill(
                    Bounds::from_corners(
                        origin + point(start_x, row_top),
                        origin + point(end_x, row_bottom),
                    ),
                    selection_color,
                ));
            }
        }

        PrepaintState {
            layout: Some(layout),
            cursor,
            selection_quads,
            text_origin: origin,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        // Scrolled-away text must not paint over the field's border or its
        // neighbours, so everything below is clipped to the element's bounds.
        let mask = ContentMask { bounds };
        let origin = prepaint.text_origin;
        let selection_quads: Vec<_> = prepaint.selection_quads.drain(..).collect();
        let layout = prepaint.layout.take();
        let cursor = prepaint.cursor.take();
        let focused = focus_handle.is_focused(window);

        let painted_layout = window.with_content_mask(Some(mask), |window| {
            for quad in selection_quads {
                window.paint_quad(quad);
            }
            if let Some(layout) = layout.as_ref() {
                let mut y = px(0.);
                for (i, line) in layout.lines.iter().enumerate() {
                    let _ = line.paint(
                        origin + point(px(0.), y),
                        layout.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    y += layout.line_block_height(i);
                }
            }
            if focused {
                if let Some(cursor) = cursor {
                    window.paint_quad(cursor);
                }
            }
            layout
        });

        if let Some(layout) = painted_layout {
            self.input.update(cx, |input, _cx| {
                input.last_layout = Some(layout);
                input.last_bounds = Some(bounds);
            });
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);

        div()
            .flex()
            .key_context(if self.multiline {
                "TextInput multiline"
            } else {
                "TextInput"
            })
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .w_full()
            .overflow_hidden()
            .px_2()
            .py_1()
            .bg(theme::terminal_bg())
            .border_1()
            .border_color(if focused {
                theme::accent()
            } else {
                theme::border()
            })
            .rounded_sm()
            .text_color(theme::fg())
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_limit_leaves_room_for_the_caret() {
        // Text wider than the field: scrollable by the difference, plus the
        // caret width so the position past the last glyph is reachable.
        assert_eq!(max_scroll(px(300.), px(100.)), px(202.));
        // Text that fits never scrolls.
        assert_eq!(max_scroll(px(50.), px(100.)), px(0.));
        assert_eq!(max_scroll(px(0.), px(100.)), px(0.));
    }
}
