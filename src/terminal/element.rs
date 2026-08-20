//! Custom GPUI element that paints the terminal grid: background quads, one
//! shaped line of text per row, and the cursor.

use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    fill, point, px, relative, size, App, Bounds, DispatchPhase, Element, Entity, GlobalElementId,
    Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Style,
    StrikethroughStyle, TextRun, UnderlineStyle, Window,
};

use super::{resolve_color, rgb_to_hsla, Terminal, TerminalSize};
use crate::theme;

pub struct TerminalElement {
    terminal: Entity<Terminal>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(terminal: Entity<Terminal>, focused: bool) -> Self {
        Self { terminal, focused }
    }
}

pub struct PrepaintState {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    cell_width: Pixels,
    display_offset: i32,
    num_rows: usize,
    num_cols: usize,
    bg_quads: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<ShapedLine>,
    cursor: Option<Cursor>,
}

/// Map a window position to a grid point + cell side, clamped to the
/// visible viewport.
fn grid_point_at(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    display_offset: i32,
    num_rows: usize,
    num_cols: usize,
) -> (GridPoint, Side) {
    let col_f = f32::from((position.x - origin.x) / cell_width);
    let row_f = f32::from((position.y - origin.y) / line_height);
    let col = (col_f.floor() as i64).clamp(0, num_cols as i64 - 1) as usize;
    let row = (row_f.floor() as i64).clamp(0, num_rows as i64 - 1) as i32;
    let side = if col_f.fract() < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (GridPoint::new(Line(row - display_offset), Column(col)), side)
}

struct Cursor {
    bounds: Bounds<Pixels>,
    color: Hsla,
    shape: CursorShape,
    overlay: Option<ShapedLine>,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let font = theme::terminal_font();
        let font_size = theme::terminal_font_size(cx);
        let font_id = window.text_system().resolve_font(&font);
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|adv| adv.width)
            .unwrap_or(px(8.));
        let line_height = px((f32::from(font_size) * 1.4).round());

        let new_size = TerminalSize {
            cell_width,
            line_height,
            size: bounds.size,
        };
        self.terminal.update(cx, |terminal, _| terminal.resize(new_size));

        let default_bg = rgb_to_hsla(super::hex_to_rgb(super::default_bg_hex()));
        let num_rows = new_size.rows();

        // Collect per-row text + style runs and background quads while the
        // terminal lock is held, then shape after releasing it.
        let mut row_text: Vec<String> = vec![String::new(); num_rows];
        let mut row_runs: Vec<Vec<TextRun>> = vec![Vec::new(); num_rows];
        let mut bg_quads: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
        let mut cursor = None;
        let display_offset_out;

        {
            let term_arc = self.terminal.read(cx).term.clone();
            let term = term_arc.lock();
            let content = term.renderable_content();
            let display_offset = content.display_offset as i32;
            display_offset_out = display_offset;
            let selection = content.selection;
            let colors = content.colors;

            for indexed in content.display_iter {
                let row = indexed.point.line.0 + display_offset;
                if row < 0 || row as usize >= num_rows {
                    continue;
                }
                let row = row as usize;
                let col = indexed.point.column.0;
                let cell = &indexed.cell;
                let flags = cell.flags;
                if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }

                let mut fg = resolve_color(cell.fg, colors);
                let mut bg = resolve_color(cell.bg, colors);
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let mut fg = rgb_to_hsla(fg);
                let mut bg = rgb_to_hsla(bg);
                if flags.contains(Flags::DIM) {
                    fg.a = 0.6;
                }
                let selected = selection
                    .map_or(false, |range| range.contains(indexed.point));
                if selected {
                    bg = theme::terminal_selection_bg();
                }

                let c = if flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                };

                if bg != default_bg {
                    let cell_bounds = Bounds {
                        origin: point(
                            bounds.origin.x + cell_width * col as f32,
                            bounds.origin.y + line_height * row as f32,
                        ),
                        size: size(cell_width, line_height),
                    };
                    // Merge with the previous quad when contiguous and same color.
                    if let Some((last, last_color)) = bg_quads.last_mut() {
                        if *last_color == bg
                            && last.origin.y == cell_bounds.origin.y
                            && (last.origin.x + last.size.width - cell_bounds.origin.x).abs()
                                < px(0.5)
                        {
                            last.size.width += cell_width;
                        } else {
                            bg_quads.push((cell_bounds, bg));
                        }
                    } else {
                        bg_quads.push((cell_bounds, bg));
                    }
                }

                let mut run_font = font.clone();
                if flags.contains(Flags::BOLD) {
                    run_font.weight = gpui::FontWeight::BOLD;
                }
                if flags.contains(Flags::ITALIC) {
                    run_font.style = gpui::FontStyle::Italic;
                }
                let underline = flags.contains(Flags::UNDERLINE).then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(fg),
                    wavy: false,
                });
                let strikethrough =
                    flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(fg),
                    });

                row_text[row].push(c);
                let len = c.len_utf8();
                let runs = &mut row_runs[row];
                match runs.last_mut() {
                    Some(last)
                        if last.font == run_font
                            && last.color == fg
                            && last.underline == underline
                            && last.strikethrough == strikethrough =>
                    {
                        last.len += len;
                    }
                    _ => runs.push(TextRun {
                        len,
                        font: run_font,
                        color: fg,
                        background_color: None,
                        underline,
                        strikethrough,
                    }),
                }
            }

            // Cursor.
            let cursor_point = content.cursor.point;
            let shape = content.cursor.shape;
            let row = cursor_point.line.0 + display_offset;
            if shape != CursorShape::Hidden && row >= 0 && (row as usize) < num_rows {
                let row_f = row as f32;
                let col = cursor_point.column.0;
                let cursor_color = rgb_to_hsla(super::palette_rgb(
                    alacritty_terminal::vte::ansi::NamedColor::Cursor as usize,
                ));
                let cursor_bounds = Bounds {
                    origin: point(
                        bounds.origin.x + cell_width * col as f32,
                        bounds.origin.y + line_height * row_f,
                    ),
                    size: size(cell_width, line_height),
                };
                let ch = term.grid()[cursor_point].c;
                let overlay = (shape == CursorShape::Block && self.focused && ch != ' ')
                    .then(|| {
                        window.text_system().shape_line(
                            SharedString::from(ch.to_string()),
                            font_size,
                            &[TextRun {
                                len: ch.len_utf8(),
                                font: font.clone(),
                                color: default_bg,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            Some(cell_width),
                        )
                    });
                cursor = Some(Cursor {
                    bounds: cursor_bounds,
                    color: cursor_color,
                    shape: if self.focused {
                        shape
                    } else {
                        CursorShape::HollowBlock
                    },
                    overlay,
                });
            }
        }

        let lines = row_text
            .into_iter()
            .zip(row_runs)
            .map(|(text, runs)| {
                window
                    .text_system()
                    .shape_line(SharedString::from(text), font_size, &runs, Some(cell_width))
            })
            .collect();

        PrepaintState {
            bounds,
            line_height,
            cell_width,
            display_offset: display_offset_out,
            num_rows,
            num_cols: new_size.cols(),
            bg_quads,
            lines,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, theme::terminal_bg()));

        for (quad_bounds, color) in &prepaint.bg_quads {
            window.paint_quad(fill(*quad_bounds, *color));
        }

        let line_height = prepaint.line_height;
        for (row, line) in prepaint.lines.iter().enumerate() {
            let origin = point(
                bounds.origin.x,
                bounds.origin.y + line_height * row as f32,
            );
            let _ = line.paint(origin, line_height, window, cx);
        }

        if let Some(cursor) = &prepaint.cursor {
            match cursor.shape {
                CursorShape::Block => {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                    if let Some(overlay) = &cursor.overlay {
                        let _ = overlay.paint(cursor.bounds.origin, line_height, window, cx);
                    }
                }
                CursorShape::HollowBlock => {
                    let mut faded = cursor.color;
                    faded.a = 0.35;
                    window.paint_quad(fill(cursor.bounds, faded));
                }
                CursorShape::Beam => {
                    let mut b = cursor.bounds;
                    b.size.width = px(2.);
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Underline => {
                    let mut b = cursor.bounds;
                    b.origin.y += b.size.height - px(2.);
                    b.size.height = px(2.);
                    window.paint_quad(fill(b, cursor.color));
                }
                CursorShape::Hidden => {}
            }
        }

        // Mouse wheel scrolls the scrollback.
        let terminal = self.terminal.clone();
        let hit_bounds = prepaint.bounds;
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble && hit_bounds.contains(&event.position) {
                let delta_y = event.delta.pixel_delta(line_height).y / line_height;
                let delta = delta_y.round() as i32;
                if delta != 0 {
                    terminal.update(cx, |terminal, cx| terminal.scroll_lines(delta, cx));
                }
            }
        });

        // Mouse selection: press starts (double = word, triple = line),
        // drag extends, release ends.
        let origin = prepaint.bounds.origin;
        let cell_width = prepaint.cell_width;
        let display_offset = prepaint.display_offset;
        let num_rows = prepaint.num_rows;
        let num_cols = prepaint.num_cols;
        let at = move |position: Point<Pixels>| {
            grid_point_at(
                position,
                origin,
                cell_width,
                line_height,
                display_offset,
                num_rows,
                num_cols,
            )
        };

        let terminal = self.terminal.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble
                && event.button == MouseButton::Left
                && hit_bounds.contains(&event.position)
            {
                let kind = match event.click_count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                let (grid_point, side) = at(event.position);
                terminal.update(cx, |terminal, cx| {
                    terminal.begin_selection(kind, grid_point, side, cx);
                });
            }
        });

        let terminal = self.terminal.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble
                && event.pressed_button == Some(MouseButton::Left)
                && terminal.read(cx).selecting
            {
                let (grid_point, side) = at(event.position);
                terminal.update(cx, |terminal, cx| {
                    terminal.drag_selection(grid_point, side, cx);
                });
            }
        });

        let terminal = self.terminal.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                terminal.update(cx, |terminal, _| terminal.end_selection());
            }
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
