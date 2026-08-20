//! Minimal virtual-pointer injector for wlroots compositors (e.g. headless
//! sway). Needed because a headless seat has no physical devices, so
//! `swaymsg seat cursor …` events never reach clients.
//!
//! Usage:
//!   wlpoint serve        # read commands from stdin (one per line):
//!                        #   move <x> <y>
//!                        #   click <x> <y> [left|right|middle]
//!                        #   scroll <x> <y> <dy>
//!   wlpoint <move|click|scroll> …   # one-shot (racy: clients bind the
//!                                   # pointer asynchronously — prefer serve)
//!
//! Coordinates are absolute; the output extent is taken from $WLPOINT_EXTENT
//! ("WxH", default 1280x820). The virtual pointer must stay connected long
//! enough for clients to notice the seat capability change, which is why
//! `serve` mode exists.

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct App;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn evdev_code(name: &str) -> Option<u32> {
    Some(match name {
        "enter" => 28,
        "escape" => 1,
        "tab" => 15,
        "space" => 57,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        other => other.parse().ok()?,
    })
}

/// Write bytes to an unlinked temp file and return the open handle.
fn tempfile_with(bytes: &[u8]) -> Option<std::fs::File> {
    use std::io::{Seek, Write};
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join(format!("wlpoint-keymap-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    let _ = std::fs::remove_file(&path);
    file.write_all(bytes).ok()?;
    file.rewind().ok()?;
    Some(file)
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

struct Ctx {
    pointer: ZwlrVirtualPointerV1,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    extent: (u32, u32),
    time: u32,
}

impl Ctx {
    fn exec(&mut self, cmd: &str, args: &[&str]) {
        self.time += 20;
        let t = self.time;

        // Keyboard commands skip the pointer motion entirely.
        if cmd == "key" {
            let (Some(keyboard), Some(code)) = (
                self.keyboard.as_ref(),
                args.first().and_then(|name| evdev_code(name)),
            ) else {
                eprintln!("wlpoint: key unavailable (no keymap or unknown key {args:?})");
                return;
            };
            keyboard.key(t, code, 1);
            keyboard.key(t + 10, code, 0);
            return;
        }

        let x: u32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let y: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        self.pointer
            .motion_absolute(t, x, y, self.extent.0, self.extent.1);
        self.pointer.frame();
        match cmd {
            "move" => {}
            "click" => {
                let button = match args.get(2).copied() {
                    Some("right") => BTN_RIGHT,
                    Some("middle") => BTN_MIDDLE,
                    _ => BTN_LEFT,
                };
                self.pointer.button(t + 5, button, wl_pointer::ButtonState::Pressed);
                self.pointer.frame();
                self.pointer
                    .button(t + 15, button, wl_pointer::ButtonState::Released);
                self.pointer.frame();
            }
            "press" | "release" => {
                let button = match args.get(2).copied() {
                    Some("right") => BTN_RIGHT,
                    Some("middle") => BTN_MIDDLE,
                    _ => BTN_LEFT,
                };
                let state = if cmd == "press" {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                self.pointer.button(t + 5, button, state);
                self.pointer.frame();
            }
            "scroll" => {
                // Full wheel event: source + continuous value + discrete
                // steps, then frame. Compositors may drop axis events that
                // lack a source.
                let dy: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(15.0);
                let discrete = if dy < 0. { -1 } else { 1 } * ((dy.abs() / 15.).ceil() as i32).max(1);
                self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                self.pointer.axis_discrete(
                    t + 5,
                    wl_pointer::Axis::VerticalScroll,
                    dy,
                    discrete,
                );
                self.pointer.frame();
            }
            other => eprintln!("wlpoint: unknown command `{other}`"),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: wlpoint serve | wlpoint <move|click|scroll> <x> <y> [button|dy]";
    let cmd = args.get(1).expect(usage).clone();

    let extent = std::env::var("WLPOINT_EXTENT").unwrap_or_else(|_| "1280x820".into());
    let (ex, ey) = extent.split_once('x').expect("WLPOINT_EXTENT must be WxH");
    let extent: (u32, u32) = (ex.parse().unwrap(), ey.parse().unwrap());

    let conn = Connection::connect_to_env().expect("connect to wayland display");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry init");
    let qh = queue.handle();

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).expect("bind wl_seat");
    let manager: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .expect("bind zwlr_virtual_pointer_manager_v1 (compositor must support it)");
    let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());

    // Optional virtual keyboard: needs an xkb keymap file, e.g. generated
    // with `xkbcli compile-keymap --layout us > /tmp/keymap.xkb`.
    let keyboard = std::env::var("WLPOINT_KEYMAP").ok().and_then(|path| {
        let keymap = std::fs::read(&path).ok()?;
        let kbd_manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        let keyboard = kbd_manager.create_virtual_keyboard(&seat, &qh, ());
        // The fd must stay valid until the compositor maps it; keep the
        // file open for the program's lifetime via a leaked handle.
        let file = Box::leak(Box::new(tempfile_with(&keymap)?));
        use std::os::fd::AsFd;
        keyboard.keymap(1, file.as_fd(), keymap.len() as u32); // 1 = XKB_V1
        Some(keyboard)
    });

    let mut app = App;
    let mut ctx = Ctx {
        pointer,
        keyboard,
        extent,
        time: 0,
    };

    if cmd == "serve" {
        // Give clients a moment to see the seat capability change and bind
        // the pointer before the first event.
        queue.roundtrip(&mut app).expect("roundtrip");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            if std::io::BufRead::read_line(&mut stdin.lock(), &mut line).unwrap_or(0) == 0 {
                break;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let Some((c, rest)) = parts.split_first() else {
                continue;
            };
            ctx.exec(c, rest);
            queue.roundtrip(&mut app).expect("roundtrip");
            println!("ok");
        }
    } else {
        let rest: Vec<&str> = args[2..].iter().map(|s| s.as_str()).collect();
        queue.roundtrip(&mut app).expect("roundtrip");
        std::thread::sleep(std::time::Duration::from_millis(300));
        ctx.exec(&cmd, &rest);
        queue.roundtrip(&mut app).expect("roundtrip");
        // Let the compositor deliver events before tearing down.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    ctx.pointer.destroy();
    queue.roundtrip(&mut app).ok();
}
