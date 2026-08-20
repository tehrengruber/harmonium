//! Terminal backend: wraps `alacritty_terminal`'s PTY + parser and exposes a
//! GPUI entity that views can observe and render.

pub mod element;
pub mod view;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb as AnsiRgb};
use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use crate::state;
use futures::StreamExt as _;
use gpui::{
    px, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, Keystroke, Pixels,
    Size, Task,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalSize {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub size: Size<Pixels>,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: px(8.),
            line_height: px(18.),
            size: Size {
                width: px(640.),
                height: px(432.),
            },
        }
    }
}

impl TerminalSize {
    pub fn rows(&self) -> usize {
        ((self.size.height / self.line_height).floor() as usize).max(2)
    }

    pub fn cols(&self) -> usize {
        ((self.size.width / self.cell_width).floor() as usize).max(2)
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows()
    }

    fn columns(&self) -> usize {
        self.cols()
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(s: TerminalSize) -> Self {
        WindowSize {
            num_lines: s.rows() as u16,
            num_cols: s.cols() as u16,
            cell_width: f32::from(s.cell_width) as u16,
            cell_height: f32::from(s.line_height) as u16,
        }
    }
}

#[derive(Clone)]
pub struct EventProxy(UnboundedSender<AlacEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TerminalEvent {
    Exited,
    TitleChanged,
    /// The child wrote to the PTY. Emitted on every wakeup, which is what
    /// makes "the resumed process has painted something" observable.
    Output,
}

/// A selection drag held outside the viewport: how many lines to scroll per
/// tick (positive scrolls up into the scrollback) plus the pointer's column,
/// so the selection can keep growing along the edge that scrolls into view.
#[derive(Clone, Copy)]
struct DragAutoscroll {
    lines: i32,
    column: Column,
    side: Side,
}

/// Button code for "no button", used by motion reports and by the legacy
/// encoding's release event.
pub const NO_BUTTON: u8 = 3;

/// A mouse event destined for a program that tracks the mouse. `button` uses
/// the X11 numbering (0 left, 1 middle, 2 right, [`NO_BUTTON`] none); `row`
/// and `column` are 0-based viewport cells.
#[derive(Clone, Copy)]
pub struct MouseReport {
    pub button: u8,
    pub pressed: bool,
    pub motion: bool,
    pub row: usize,
    pub column: usize,
    pub ctrl: bool,
    pub alt: bool,
}

/// The user's selection, in absolute grid coordinates.
///
/// We keep this instead of leaving the selection in `Term`, because the
/// parser throws `Term::selection` away whenever the program erases the
/// region it covers (`ClearMode::Below`/`All`) — which for a full-screen
/// inline TUI like the agent is *every redraw*, several times a second. A
/// selection belongs to the user, not to the program, so ours is the source
/// of truth and gets re-installed before every render and copy.
#[derive(Clone, Copy)]
struct SelectionState {
    ty: SelectionType,
    start: GridPoint,
    start_side: Side,
    end: GridPoint,
    end_side: Side,
}

impl SelectionState {
    fn build(&self) -> Selection {
        let mut selection = Selection::new(self.ty, self.start, self.start_side);
        selection.update(self.end, self.end_side);
        selection
    }

    /// Follow the content when `delta` lines scroll off into the scrollback:
    /// what sat on line `k` is on line `k - delta` afterwards.
    fn shift(&mut self, delta: i32) {
        self.start.line = Line(self.start.line.0 - delta);
        self.end.line = Line(self.end.line.0 - delta);
    }
}

pub struct Terminal {
    /// Stable id (agent or tab id) naming the on-disk history file.
    id: String,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    sender: EventLoopSender,
    pub size: TerminalSize,
    pub title: String,
    pub exited: bool,
    /// A mouse selection drag is in progress.
    pub selecting: bool,
    /// Simple selections are created lazily on the first drag movement so a
    /// plain click doesn't highlight a single cell.
    pending_selection: Option<(GridPoint, Side)>,
    /// The user's selection; see [`SelectionState`].
    selection: Option<SelectionState>,
    /// Scrollback size the selection was last aligned against, so output
    /// arriving between two frames can be compensated for.
    selection_history: usize,
    /// Last cell reported to a mouse-tracking program, so motion is only
    /// reported when the pointer actually changes cell.
    last_mouse_cell: Option<(usize, usize)>,
    /// A button press was forwarded to the program and not yet released.
    /// Its release has to be forwarded too, wherever the pointer ends up —
    /// but a release whose press we never sent must not leak through.
    mouse_pressed: bool,
    /// Set while a selection drag is held past the top/bottom edge; the task
    /// repeats the scroll because a pointer held still emits no more events.
    autoscroll: Option<DragAutoscroll>,
    autoscroll_task: Option<Task<()>>,
    /// Whether this terminal's scrollback is written to disk on shutdown and
    /// replayed on the next start. Only shell tabs persist; agent terminals
    /// resume via their own session command and repaint themselves.
    persist_history: bool,
    /// Cleared when the tab is removed for good, so dropping the entity
    /// doesn't leave an orphaned history file behind.
    save_history_on_drop: bool,
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Spawn `program args...` in a fresh PTY rooted at `workdir`. `id` names
    /// the history file written when this terminal is dropped; the file is
    /// only written when `persist_history` is set.
    pub fn create(
        id: String,
        program: String,
        args: Vec<String>,
        env_overrides: Vec<(String, String)>,
        workdir: PathBuf,
        persist_history: bool,
        cx: &mut App,
    ) -> Result<Entity<Terminal>> {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let proxy = EventProxy(tx);

        let size = TerminalSize::default();
        let config = TermConfig {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        // Overrides only: alacritty's `tty::new` never calls `env_clear`, so
        // the child already inherits our environment and these entries are
        // applied on top of it. Copying the whole environment in here would
        // also let an inherited WINDOWID clobber alacritty's per-PTY value.
        let mut env: HashMap<String, String> = HashMap::from([
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]);
        // Anything configured on the command wins over our defaults.
        env.extend(env_overrides);

        let options = PtyOptions {
            shell: Some(Shell::new(program.clone(), args)),
            working_directory: Some(workdir.clone()),
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&options, size.into(), 0)
            .with_context(|| format!("spawning `{program}` in {}", workdir.display()))?;

        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)
            .context("starting terminal event loop")?;
        let sender = event_loop.channel();
        event_loop.spawn();

        Ok(cx.new(|cx: &mut Context<Terminal>| {
            cx.spawn(async move |this, cx| {
                while let Some(event) = rx.next().await {
                    let Ok(()) = this.update(cx, |terminal: &mut Terminal, cx| {
                        terminal.process_event(event, cx);
                    }) else {
                        break;
                    };
                }
            })
            .detach();

            Terminal {
                id,
                term,
                sender,
                size,
                title: program,
                exited: false,
                selecting: false,
                pending_selection: None,
                selection: None,
                selection_history: 0,
                last_mouse_cell: None,
                mouse_pressed: false,
                autoscroll: None,
                autoscroll_task: None,
                persist_history,
                save_history_on_drop: true,
            }
        }))
    }

    fn process_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
        match event {
            AlacEvent::Wakeup => {
                cx.emit(TerminalEvent::Output);
                cx.notify();
            }
            AlacEvent::Title(title) => {
                self.title = title;
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            AlacEvent::ResetTitle => {
                self.title.clear();
                cx.notify();
            }
            AlacEvent::PtyWrite(text) => self.write(text.into_bytes()),
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, formatter) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                self.write(formatter(&text).into_bytes());
            }
            AlacEvent::ColorRequest(index, formatter) => {
                let rgb = palette_rgb(index);
                self.write(formatter(rgb).into_bytes());
            }
            AlacEvent::TextAreaSizeRequest(formatter) => {
                let window_size: WindowSize = self.size.into();
                self.write(formatter(window_size).into_bytes());
            }
            AlacEvent::Exit | AlacEvent::ChildExit(_) => {
                self.exited = true;
                cx.emit(TerminalEvent::Exited);
                cx.notify();
            }
            AlacEvent::MouseCursorDirty
            | AlacEvent::Bell
            | AlacEvent::CursorBlinkingChange => {}
        }
    }

    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.sender.send(Msg::Input(Cow::Owned(bytes)));
    }

    pub fn resize(&mut self, new_size: TerminalSize) {
        if self.size == new_size {
            return;
        }
        self.size = new_size;
        let _ = self.sender.send(Msg::Resize(new_size.into()));
        self.term.lock().resize(new_size);
    }

    /// Capture and write this terminal's scrollback history to disk.
    /// No-op for terminals that don't persist (agent sessions).
    pub fn save_history(&self) {
        if !self.persist_history {
            return;
        }
        if let Err(error) = std::fs::create_dir_all(state::history_dir()) {
            eprintln!("[harmonium] failed to create history dir: {error}");
            return;
        }
        let path = state::terminal_history_path(&self.id);
        let history = self.capture_history();
        if history.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        // Trailing newline so the replayed shell's prompt starts on its own
        // line: the shell redraws its prompt line on the first resize, which
        // would otherwise erase the last history line sharing it.
        let mut text = history.join("\n");
        text.push('\n');
        if let Err(error) = std::fs::write(&path, text) {
            eprintln!("[harmonium] failed to write history {}: {error}", path.display());
        }
    }

    /// Skip the history write on drop, leaving any file on disk untouched.
    /// Used for a terminal that has been superseded: its replacement owns the
    /// history file from now on and must not have it overwritten later.
    pub fn forget_history(&mut self) {
        self.save_history_on_drop = false;
    }

    /// Write the scrollback to disk right now and never again on drop, so a
    /// replacement terminal spawned immediately after reads the current
    /// contents rather than a stale file.
    pub fn save_history_now(&mut self) {
        self.save_history();
        self.forget_history();
    }

    /// Skip the history write on drop and delete any saved file — used when
    /// the tab is removed for good rather than the app shutting down.
    pub fn discard_history(&mut self) {
        self.save_history_on_drop = false;
        let _ = std::fs::remove_file(state::terminal_history_path(&self.id));
    }

    /// Capture all lines currently in the terminal grid (scrollback + visible)
    /// as ANSI-styled text: cell colors and attributes are re-encoded as SGR
    /// escape sequences so a replay through the PTY restores the styling.
    /// Trailing blank cells and trailing empty lines are trimmed.
    pub fn capture_history(&self) -> Vec<String> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::Line;

        let style_flags = CellFlags::BOLD
            | CellFlags::DIM
            | CellFlags::ITALIC
            | CellFlags::UNDERLINE
            | CellFlags::INVERSE
            | CellFlags::STRIKEOUT;
        let default_style = (
            AnsiColor::Named(NamedColor::Foreground),
            AnsiColor::Named(NamedColor::Background),
            CellFlags::empty(),
        );

        let term = self.term.lock();
        let grid = term.grid();
        let total = grid.total_lines();
        let screen = grid.screen_lines();
        let history = total.saturating_sub(screen);
        let mut lines = Vec::with_capacity(total);

        for i in 0..total {
            // History lines have negative indices; visible lines are 0..screen-1.
            let line_idx = Line(i as i32 - history as i32);
            let row = &grid[line_idx];
            let mut text = String::new();
            // Byte length of `text` up to the last cell worth keeping, so
            // trailing default-background blanks (and their escapes) drop off.
            let mut keep = 0;
            // Each line starts from the reset state; the previous line ends
            // with a reset whenever it emitted any styling.
            let mut current = default_style;
            for cell in row.into_iter() {
                if cell
                    .flags
                    .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let style = (cell.fg, cell.bg, cell.flags & style_flags);
                if style != current {
                    text.push_str(&sgr_sequence(cell.fg, cell.bg, cell.flags & style_flags));
                    current = style;
                }
                let c = if cell.c == '\0' { ' ' } else { cell.c };
                text.push(c);
                // A space is only truly invisible on the default background
                // without inverse/underline/strikeout styling.
                let blank = c == ' '
                    && cell.bg == default_style.1
                    && !cell.flags.intersects(
                        CellFlags::INVERSE | CellFlags::UNDERLINE | CellFlags::STRIKEOUT,
                    );
                if !blank {
                    keep = text.len();
                }
            }
            text.truncate(keep);
            if text.contains('\x1b') {
                text.push_str("\x1b[0m");
            }
            lines.push(text);
        }

        // Drop trailing empty lines.
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }

        lines
    }

    pub fn scroll_lines(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
        cx.notify();
    }

    /// Handle a wheel scroll of `lines` (positive scrolls up/back) with the
    /// pointer over viewport cell `row`/`column`, the way a real terminal
    /// does — which is what makes the wheel work inside full-screen programs
    /// like the agent TUI, where our own scrollback is empty:
    ///
    /// - the program tracks the mouse: report the wheel and let it scroll;
    /// - alternate screen (no scrollback of its own) with alternate-scroll
    ///   enabled: translate the wheel into arrow keys, like xterm;
    /// - otherwise: move our viewport over the scrollback.
    pub fn scroll_wheel(&mut self, lines: i32, row: usize, column: usize, cx: &mut Context<Self>) {
        if lines == 0 {
            return;
        }
        let mode = *self.term.lock().mode();
        let count = lines.unsigned_abs() as usize;

        if mode.intersects(TermMode::MOUSE_MODE) {
            let button = if lines > 0 { 64 } else { 65 };
            let mut bytes = Vec::new();
            for _ in 0..count {
                if let Some(report) = mouse_report(button, row, column, mode, true) {
                    bytes.extend_from_slice(&report);
                }
            }
            if !bytes.is_empty() {
                self.write(bytes);
            }
            return;
        }

        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let seq: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut bytes = Vec::with_capacity(seq.len() * count);
            for _ in 0..count {
                bytes.extend_from_slice(seq);
            }
            self.write(bytes);
            return;
        }

        self.scroll_lines(lines, cx);
    }

    pub fn paste(&mut self, text: &str) {
        self.clear_selection();
        let mode = *self.term.lock().mode();
        let bytes = if mode.contains(TermMode::BRACKETED_PASTE) {
            let mut b = b"\x1b[200~".to_vec();
            b.extend_from_slice(text.replace('\x1b', "").as_bytes());
            b.extend_from_slice(b"\x1b[201~");
            b
        } else {
            text.replace('\n', "\r").into_bytes()
        };
        self.scroll_to_bottom();
        self.write(bytes);
    }

    fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// Translate a keystroke to terminal input. Returns true if consumed.
    pub fn try_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        if self.exited {
            return false;
        }
        let mode = *self.term.lock().mode();
        if let Some(bytes) = keystroke_to_bytes(keystroke, mode) {
            self.clear_selection();
            self.scroll_to_bottom();
            self.write(bytes);
            cx.notify();
            true
        } else {
            false
        }
    }

    // ---- Mouse selection ----

    pub fn begin_selection(
        &mut self,
        kind: SelectionType,
        point: GridPoint,
        side: Side,
        cx: &mut Context<Self>,
    ) {
        if kind == SelectionType::Simple {
            self.selection = None;
            self.pending_selection = Some((point, side));
        } else {
            self.selection = Some(SelectionState {
                ty: kind,
                start: point,
                start_side: side,
                end: point,
                end_side: side,
            });
            self.pending_selection = None;
        }
        self.selection_history = self.term.lock().history_size();
        self.selecting = true;
        cx.notify();
    }

    pub fn drag_selection(&mut self, point: GridPoint, side: Side, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.extend_selection(point, side);
        cx.notify();
    }

    /// Move the selection's loose end, turning a pending simple selection
    /// into a real one on the first drag.
    fn extend_selection(&mut self, point: GridPoint, side: Side) {
        if let Some((start, start_side)) = self.pending_selection.take() {
            self.selection = Some(SelectionState {
                ty: SelectionType::Simple,
                start,
                start_side,
                end: point,
                end_side: side,
            });
        } else if let Some(state) = self.selection.as_mut() {
            state.end = point;
            state.end_side = side;
        }
    }

    /// Re-install the selection into the parser's term ahead of a render or
    /// a copy, compensating for output that scrolled into the scrollback
    /// since the last sync. The parser may drop or rotate the copy it holds
    /// at any time — ours is authoritative and overwrites it here.
    pub fn sync_selection(&mut self) {
        let term = self.term.clone();
        let mut term = term.lock();
        let history = term.history_size();
        if let Some(state) = self.selection.as_mut() {
            let delta = history as i32 - self.selection_history as i32;
            if delta != 0 {
                state.shift(delta);
            }
            term.selection = Some(state.build());
        } else {
            term.selection = None;
        }
        self.selection_history = history;
    }

    /// Drop the selection, e.g. because the user typed or pasted.
    fn clear_selection(&mut self) {
        self.selection = None;
        self.pending_selection = None;
        self.term.lock().selection = None;
    }

    /// Whether the running program asked to receive mouse events. Callers
    /// still decide: holding shift takes the mouse back for a terminal-side
    /// selection, exactly as in xterm and VTE.
    pub fn wants_mouse(&self) -> bool {
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether a forwarded press is still outstanding; see [`Self::mouse_pressed`].
    pub fn mouse_pressed(&self) -> bool {
        self.mouse_pressed
    }

    /// Forward a press, release or motion to a program that tracks the mouse,
    /// so it can run its own selection and scrolling (which is how full-screen
    /// TUIs let you select their own scrollback).
    pub fn report_mouse(&mut self, report: MouseReport) {
        let mode = *self.term.lock().mode();
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return;
        }
        if report.motion {
            // 1002 reports motion only while a button is held, 1003 always.
            let wanted = if report.button == NO_BUTTON {
                mode.contains(TermMode::MOUSE_MOTION)
            } else {
                mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
            };
            if !wanted || self.last_mouse_cell == Some((report.row, report.column)) {
                return;
            }
        }
        self.last_mouse_cell = Some((report.row, report.column));
        if !report.motion {
            self.mouse_pressed = report.pressed;
        }

        let mut code = report.button;
        if report.motion {
            code += 32;
        }
        if report.alt {
            code += 8;
        }
        if report.ctrl {
            code += 16;
        }
        if let Some(bytes) = mouse_report(code, report.row, report.column, mode, report.pressed) {
            self.write(bytes);
        }
    }

    pub fn end_selection(&mut self) {
        self.selecting = false;
        self.pending_selection = None;
        // Dropping the task cancels the timer.
        self.autoscroll = None;
        self.autoscroll_task = None;
    }

    /// Drive scrolling for a drag held past the top or bottom edge: `spec` is
    /// `(lines per tick, pointer column, side)`, or `None` once the pointer is
    /// back inside the viewport. A pointer resting outside emits no further
    /// mouse events, so the scroll has to repeat on its own.
    pub fn set_drag_autoscroll(
        &mut self,
        spec: Option<(i32, Column, Side)>,
        cx: &mut Context<Self>,
    ) {
        let Some((lines, column, side)) = spec else {
            self.autoscroll = None;
            self.autoscroll_task = None;
            return;
        };
        let already_running = self.autoscroll.is_some();
        self.autoscroll = Some(DragAutoscroll { lines, column, side });
        if already_running {
            return;
        }
        self.autoscroll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let keep_scrolling = this
                    .update(cx, |terminal, cx| terminal.autoscroll_tick(cx))
                    .unwrap_or(false);
                if !keep_scrolling {
                    break;
                }
            }
        }));
    }

    /// One auto-scroll step: move the viewport, then drag the selection's end
    /// onto the line that just scrolled into view at that edge.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(autoscroll) = self.autoscroll else {
            return false;
        };
        if !self.selecting {
            self.autoscroll = None;
            return false;
        }
        let point = {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Delta(autoscroll.lines));
            let display_offset = term.grid().display_offset() as i32;
            // Scrollback lines are negative: the top visible line is
            // `-display_offset`, the bottom one `screen_lines - 1` below it.
            let line = if autoscroll.lines > 0 {
                Line(-display_offset)
            } else {
                Line(term.screen_lines() as i32 - 1 - display_offset)
            };
            GridPoint::new(line, autoscroll.column)
        };
        self.extend_selection(point, autoscroll.side);
        cx.notify();
        true
    }

    pub fn selection_text(&mut self) -> Option<String> {
        self.sync_selection();
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

/// Encode a mouse event for a program that requested mouse reporting.
/// `button` follows the X11 numbering (64/65 are wheel up/down); `row` and
/// `column` are 0-based viewport cells. The legacy encoding can't express
/// coordinates past 222, in which case no report is sent.
fn mouse_report(
    button: u8,
    row: usize,
    column: usize,
    mode: TermMode,
    pressed: bool,
) -> Option<Vec<u8>> {
    if mode.contains(TermMode::SGR_MOUSE) {
        // SGR distinguishes press from release by the final byte, so the
        // button survives into the release event.
        let final_byte = if pressed { 'M' } else { 'm' };
        Some(format!("\x1b[<{button};{};{}{final_byte}", column + 1, row + 1).into_bytes())
    } else if row < 223 && column < 223 {
        // The legacy encoding has no release button: 3 means "let go", and
        // which button it was is lost.
        let button = if pressed { button } else { 3 | (button & !0b11) };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + button,
            (32 + 1 + column) as u8,
            (32 + 1 + row) as u8,
        ])
    } else {
        None
    }
}

/// Encode a cell style as one SGR sequence, starting from the reset state.
/// Default foreground/background are covered by the leading `0`.
fn sgr_sequence(fg: AnsiColor, bg: AnsiColor, flags: CellFlags) -> String {
    use std::fmt::Write as _;

    let mut params = String::from("0");
    for (flag, code) in [
        (CellFlags::BOLD, 1),
        (CellFlags::DIM, 2),
        (CellFlags::ITALIC, 3),
        (CellFlags::UNDERLINE, 4),
        (CellFlags::INVERSE, 7),
        (CellFlags::STRIKEOUT, 9),
    ] {
        if flags.contains(flag) {
            let _ = write!(params, ";{code}");
        }
    }
    for (color, foreground) in [(fg, true), (bg, false)] {
        match color {
            AnsiColor::Named(named) => {
                if let Some(code) = named_sgr(named, foreground) {
                    let _ = write!(params, ";{code}");
                }
            }
            AnsiColor::Indexed(i) => {
                let _ = write!(params, ";{};5;{i}", if foreground { 38 } else { 48 });
            }
            AnsiColor::Spec(rgb) => {
                let _ = write!(
                    params,
                    ";{};2;{};{};{}",
                    if foreground { 38 } else { 48 },
                    rgb.r,
                    rgb.g,
                    rgb.b
                );
            }
        }
    }
    format!("\x1b[{params}m")
}

/// SGR color code for a named color, or `None` for the terminal defaults
/// (which the reset in [`sgr_sequence`] already restores). Dim variants map
/// to their base color; the DIM attribute flag carries the dimming.
fn named_sgr(named: NamedColor, foreground: bool) -> Option<u16> {
    use NamedColor::*;
    let (index, bright) = match named {
        Black | DimBlack => (0, false),
        Red | DimRed => (1, false),
        Green | DimGreen => (2, false),
        Yellow | DimYellow => (3, false),
        Blue | DimBlue => (4, false),
        Magenta | DimMagenta => (5, false),
        Cyan | DimCyan => (6, false),
        White | DimWhite => (7, false),
        BrightBlack => (0, true),
        BrightRed => (1, true),
        BrightGreen => (2, true),
        BrightYellow => (3, true),
        BrightBlue => (4, true),
        BrightMagenta => (5, true),
        BrightCyan => (6, true),
        BrightWhite => (7, true),
        Foreground | Background | Cursor | BrightForeground | DimForeground => return None,
    };
    Some(match (foreground, bright) {
        (true, false) => 30 + index,
        (true, true) => 90 + index,
        (false, false) => 40 + index,
        (false, true) => 100 + index,
    })
}

/// History is written when the entity is dropped: this covers graceful quit,
/// tab/agent teardown, and the window being closed by the window manager
/// (WM_DELETE_WINDOW), where gpui removes the window — and drops its entities —
/// before `on_app_quit` observers run.
impl Drop for Terminal {
    fn drop(&mut self) {
        if self.save_history_on_drop {
            self.save_history();
        }
        self.shutdown();
    }
}

fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let mods = keystroke.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Leave ctrl-shift-* chords (except paste, handled by the view) to the app.
    if mods.control && mods.shift {
        return None;
    }

    let mod_code: Option<u8> = {
        // xterm modifier encoding: 1 + shift(1) + alt(2) + ctrl(4)
        let mut code = 1u8;
        if mods.shift {
            code += 1;
        }
        if mods.alt {
            code += 2;
        }
        if mods.control {
            code += 4;
        }
        (code > 1).then_some(code)
    };

    let cursor_seq = |c: char| -> Vec<u8> {
        match mod_code {
            Some(m) => format!("\x1b[1;{m}{c}").into_bytes(),
            None if app_cursor => format!("\x1bO{c}").into_bytes(),
            None => format!("\x1b[{c}").into_bytes(),
        }
    };
    let tilde_seq = |n: u8| -> Vec<u8> {
        match mod_code {
            Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
            None => format!("\x1b[{n}~").into_bytes(),
        }
    };

    let bytes = match keystroke.key.as_str() {
        "enter" => vec![b'\r'],
        "tab" if mods.shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "backspace" => {
            if mods.alt {
                b"\x1b\x7f".to_vec()
            } else if mods.control {
                vec![0x17] // ctrl-backspace commonly maps to ctrl-w
            } else {
                vec![0x7f]
            }
        }
        "escape" => vec![0x1b],
        "up" => cursor_seq('A'),
        "down" => cursor_seq('B'),
        "right" => cursor_seq('C'),
        "left" => cursor_seq('D'),
        "home" => cursor_seq('H'),
        "end" => cursor_seq('F'),
        "insert" => tilde_seq(2),
        "delete" => tilde_seq(3),
        "pageup" => tilde_seq(5),
        "pagedown" => tilde_seq(6),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde_seq(15),
        "f6" => tilde_seq(17),
        "f7" => tilde_seq(18),
        "f8" => tilde_seq(19),
        "f9" => tilde_seq(20),
        "f10" => tilde_seq(21),
        "f11" => tilde_seq(23),
        "f12" => tilde_seq(24),
        "space" if mods.control => vec![0x00],
        key => {
            // Control characters: ctrl-a .. ctrl-z and friends.
            if mods.control {
                let c = key.chars().next()?;
                let byte = match c {
                    'a'..='z' => c as u8 - b'a' + 1,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '-' | '_' | '/' => 0x1f,
                    _ => return None,
                };
                if key.chars().count() == 1 {
                    vec![byte]
                } else {
                    return None;
                }
            } else {
                let text = keystroke.key_char.clone().or_else(|| {
                    // Fall back to the raw key for simple printable keys when
                    // key_char is absent (e.g. alt-<letter> on some platforms).
                    (key.chars().count() == 1 && mods.alt).then(|| key.to_string())
                })?;
                let mut bytes = Vec::new();
                if mods.alt {
                    bytes.push(0x1b);
                }
                bytes.extend_from_slice(text.as_bytes());
                bytes
            }
        }
    };
    Some(bytes)
}

// One Dark-ish 16-color palette.
const DARK_PALETTE: [u32; 16] = [
    0x3f4451, // black
    0xe05561, // red
    0x8cc265, // green
    0xd18f52, // yellow
    0x4aa5f0, // blue
    0xc162de, // magenta
    0x42b3c2, // cyan
    0xd7dae0, // white
    0x4f5666, // bright black
    0xff616e, // bright red
    0xa5e075, // bright green
    0xf0a45d, // bright yellow
    0x4dc4ff, // bright blue
    0xde73ff, // bright magenta
    0x4cd1e0, // bright cyan
    0xe6e6e6, // bright white
];

// One Light-ish 16-color palette.
const LIGHT_PALETTE: [u32; 16] = [
    0x383a42, // black
    0xe45649, // red
    0x50a14f, // green
    0xc18401, // yellow
    0x4078f2, // blue
    0xa626a4, // magenta
    0x0184bc, // cyan
    0xa0a1a7, // white
    0x696c77, // bright black
    0xca1243, // bright red
    0x23974a, // bright green
    0xdf6c1c, // bright yellow
    0x275fe4, // bright blue
    0x823ff1, // bright magenta
    0x27618d, // bright cyan
    0x0f1013, // bright white
];

fn palette() -> &'static [u32; 16] {
    match crate::theme::mode() {
        crate::theme::ThemeMode::Dark => &DARK_PALETTE,
        crate::theme::ThemeMode::Light => &LIGHT_PALETTE,
    }
}

fn pick(dark: u32, light: u32) -> u32 {
    match crate::theme::mode() {
        crate::theme::ThemeMode::Dark => dark,
        crate::theme::ThemeMode::Light => light,
    }
}

pub fn default_fg_hex() -> u32 {
    pick(0xabb2bf, 0x383a42)
}

pub fn default_bg_hex() -> u32 {
    pick(0x282c33, 0xfafafa)
}

pub fn hex_to_rgb(hex: u32) -> AnsiRgb {
    AnsiRgb {
        r: ((hex >> 16) & 0xff) as u8,
        g: ((hex >> 8) & 0xff) as u8,
        b: (hex & 0xff) as u8,
    }
}

/// Resolve an indexed color (0-255 plus alacritty's special indices) to RGB.
pub fn palette_rgb(index: usize) -> AnsiRgb {
    match index {
        0..=15 => hex_to_rgb(palette()[index]),
        16..=231 => {
            let i = index - 16;
            let to_channel = |v: usize| -> u8 {
                if v == 0 {
                    0
                } else {
                    (55 + v * 40) as u8
                }
            };
            AnsiRgb {
                r: to_channel(i / 36),
                g: to_channel((i / 6) % 6),
                b: to_channel(i % 6),
            }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            AnsiRgb { r: v, g: v, b: v }
        }
        _ => hex_to_rgb(default_fg_hex()),
    }
}

pub fn named_color_rgb(color: NamedColor) -> AnsiRgb {
    use NamedColor::*;
    let palette = palette();
    match color {
        Foreground | Cursor => hex_to_rgb(default_fg_hex()),
        Background => hex_to_rgb(default_bg_hex()),
        Black => hex_to_rgb(palette[0]),
        Red => hex_to_rgb(palette[1]),
        Green => hex_to_rgb(palette[2]),
        Yellow => hex_to_rgb(palette[3]),
        Blue => hex_to_rgb(palette[4]),
        Magenta => hex_to_rgb(palette[5]),
        Cyan => hex_to_rgb(palette[6]),
        White => hex_to_rgb(palette[7]),
        BrightBlack => hex_to_rgb(palette[8]),
        BrightRed => hex_to_rgb(palette[9]),
        BrightGreen => hex_to_rgb(palette[10]),
        BrightYellow => hex_to_rgb(palette[11]),
        BrightBlue => hex_to_rgb(palette[12]),
        BrightMagenta => hex_to_rgb(palette[13]),
        BrightCyan => hex_to_rgb(palette[14]),
        BrightWhite | BrightForeground => hex_to_rgb(palette[15]),
        DimBlack => hex_to_rgb(pick(0x2a2e39, 0x8b8f98)),
        DimRed => hex_to_rgb(pick(0x99404a, 0xdf9a92)),
        DimGreen => hex_to_rgb(pick(0x5e8245, 0x9cc59b)),
        DimYellow => hex_to_rgb(pick(0x8f6238, 0xd9bd7f)),
        DimBlue => hex_to_rgb(pick(0x3371a3, 0x9cb6ef)),
        DimMagenta | DimForeground => hex_to_rgb(pick(0x854397, 0xcf9ace)),
        DimCyan => hex_to_rgb(pick(0x2e7a84, 0x86b9d0)),
        DimWhite => hex_to_rgb(pick(0x92959c, 0xc9cbd0)),
    }
}

/// Resolve any alacritty color to RGB, honoring colors set at runtime via OSC.
pub fn resolve_color(
    color: AnsiColor,
    runtime_colors: &alacritty_terminal::term::color::Colors,
) -> AnsiRgb {
    match color {
        AnsiColor::Spec(rgb) => rgb,
        AnsiColor::Indexed(index) => runtime_colors[index as usize]
            .unwrap_or_else(|| palette_rgb(index as usize)),
        AnsiColor::Named(named) => runtime_colors[named as usize]
            .unwrap_or_else(|| named_color_rgb(named)),
    }
}

pub fn rgb_to_hsla(rgb: AnsiRgb) -> gpui::Hsla {
    gpui::Rgba {
        r: rgb.r as f32 / 255.,
        g: rgb.g as f32 / 255.,
        b: rgb.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

