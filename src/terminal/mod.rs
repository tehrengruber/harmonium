//! Terminal backend: wraps `alacritty_terminal`'s PTY + parser and exposes a
//! GPUI entity that views can observe and render.

pub mod element;
pub mod view;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb as AnsiRgb};
use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use futures::StreamExt as _;
use gpui::{
    px, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, Keystroke, Pixels,
    Size,
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
}

pub struct Terminal {
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
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    /// Spawn `program args...` in a fresh PTY rooted at `workdir`.
    pub fn create(
        program: String,
        args: Vec<String>,
        workdir: PathBuf,
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

        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

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
                term,
                sender,
                size,
                title: program,
                exited: false,
                selecting: false,
                pending_selection: None,
            }
        }))
    }

    fn process_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
        match event {
            AlacEvent::Wakeup => cx.notify(),
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

    pub fn scroll_lines(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
        cx.notify();
    }

    pub fn paste(&mut self, text: &str) {
        self.term.lock().selection = None;
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
            self.term.lock().selection = None;
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
        {
            let mut term = self.term.lock();
            if kind == SelectionType::Simple {
                term.selection = None;
                self.pending_selection = Some((point, side));
            } else {
                term.selection = Some(Selection::new(kind, point, side));
                self.pending_selection = None;
            }
        }
        self.selecting = true;
        cx.notify();
    }

    pub fn drag_selection(&mut self, point: GridPoint, side: Side, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        {
            let mut term = self.term.lock();
            if let Some((start, start_side)) = self.pending_selection.take() {
                term.selection = Some(Selection::new(SelectionType::Simple, start, start_side));
            }
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
        }
        cx.notify();
    }

    pub fn end_selection(&mut self) {
        self.selecting = false;
        self.pending_selection = None;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
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
