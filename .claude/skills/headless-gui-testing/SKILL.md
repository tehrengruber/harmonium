---
name: headless-gui-testing
description: How to build and visually test the GPUI app (or any Wayland/Vulkan GUI) headlessly inside this AO isolation container — package installs, headless sway compositor, screenshots, input injection.
---

# Headless GUI testing in this isolation container

Verified working 2026-08-20 on the Arch-based AO worker container (no display
server, passwordless sudo, `$HOME` mostly read-only). Sessions run inside
claude-container-isolation (claude-isol).

> ⚠️ **If sudo fails with "no new privileges" (and `/etc/sudo.conf` appears
> owned by uid 65534): STOP.** The session is running in `claude-isol --local`
> (bubblewrap) mode, where root elevation is impossible by design — no
> workaround exists inside the sandbox. Tell the user and ask them to either
> restart the session in container mode (without `--local`) or install the
> needed packages host-side. Do not build no-root package-extraction hacks.
> Check first thing: `sudo -n whoami; which sway grim wtype`.

## Environment quirks

- `$HOME` is not writable → rustup fails. Install Rust via pacman instead, and
  set `CARGO_HOME` to a writable path inside the workdir:
  `export CARGO_HOME=$PWD/.cargo-home`
- `/tmp/.X11-unix` cannot be created (non-root euid) → Xvfb works but with a
  warning; unix sockets must live in a **short** path (108-char AF_UNIX limit),
  so use e.g. `XDG_RUNTIME_DIR=/tmp/claude-1000/xrt`, NOT the deep scratchpad.
- Binaries with file capabilities fail to exec with `Operation not permitted`
  (e.g. sway ships `cap_sys_nice=ep`). Fix: `sudo setcap -r /usr/bin/sway`.
- Prebuilt imagemagick/ffmpeg may need a newer glibc than installed — avoid;
  use `grim` (Wayland) or netpbm/custom converter (X11).
- No GPU: install `vulkan-swrast` (lavapipe). GPUI/blade picks it up
  automatically ("Adapter: llvmpipe").

## One-time setup (pacman)

```bash
# Toolchain + GPUI build deps
sudo pacman -S --noconfirm --needed rust base-devel cmake clang pkgconf git \
  fontconfig freetype2 libxkbcommon libxkbcommon-x11 wayland wayland-protocols \
  vulkan-headers vulkan-icd-loader libx11 libxcb xcb-util xcb-util-keysyms \
  xcb-util-wm xcb-util-image mesa alsa-lib openssl zstd

# Headless display + testing tools + a real font
sudo pacman -S --noconfirm --needed vulkan-swrast sway grim wtype jq ttf-dejavu

# sway ships with a file capability that this container refuses to exec
sudo setcap -r /usr/bin/sway
```

## Start the headless compositor (sway)

X11/Xvfb does NOT work for GPUI: the app renders (verify via
`RUST_LOG=debug` showing cosmic_text shaping) but lavapipe-presented frames
never reach Xvfb's framebuffer — screenshots stay black. Use Wayland:

```bash
export XDG_RUNTIME_DIR=/tmp/claude-1000/xrt   # short path! AF_UNIX limit
mkdir -p $XDG_RUNTIME_DIR && chmod 700 $XDG_RUNTIME_DIR

cat > /tmp/claude-1000/sway-config <<'EOF'
output HEADLESS-1 resolution 1280x820
default_border none
EOF

WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  sway -c /tmp/claude-1000/sway-config >/tmp/claude-1000/sway.log 2>&1 &
# wait ~2s; socket appears as $XDG_RUNTIME_DIR/wayland-1
```

Xwayland/swaybg errors in sway.log are harmless (not installed).

## Keyboard without wtype: wlpoint virtual keyboard

`tools/wlpoint` also speaks zwp_virtual_keyboard_v1. Generate a keymap once
(`xkbcli compile-keymap --layout us > /tmp/claude-1000/keymap.xkb` — xkbcli
ships with libxkbcommon), export `WLPOINT_KEYMAP=/tmp/claude-1000/keymap.xkb`,
then use `key enter|escape|tab|space|up|down|left|right|<evdev-code>` in serve
mode. Enough for accepting claude's trust/permission prompts (enter, down);
it does NOT type text — for text entry use wtype (and if wtype can't be
installed, see the sudo warning at the top: stop and ask).

Focus gotcha: clicking a button that *spawns* a terminal does not focus the
terminal — click inside the terminal area before sending keys.

## Run the app + screenshot + input

```bash
export XDG_RUNTIME_DIR=/tmp/claude-1000/xrt WAYLAND_DISPLAY=wayland-1
export XDG_CACHE_HOME=/tmp/claude-1000/cache   # HOME/.cache not writable
./target/debug/harmonium &   # GPUI apps prefer Wayland automatically

grim /tmp/claude-1000/shot.png        # screenshot (works, verified)

# Keyboard input (wlr virtual keyboard):
wtype "hello world"
wtype -k Return                # named keys: Return, Escape, Tab, BackSpace…
wtype -M ctrl -k Return -m ctrl   # modified keys (ctrl-enter)
```

Then `Read` the PNG to visually verify. Iterate: act → `grim` → look.

### Mouse input: swaymsg does NOT work — use tools/wlpoint

`swaymsg seat seat0 cursor set/press …` moves sway's cursor but a headless
seat has **no pointer capability**, so clients never bind `wl_pointer` and
receive nothing (no hover, no clicks). Verified failure mode: UI shows no
hover highlight and clicks are dropped.

Fix: `tools/wlpoint` in this repo — a zwlr_virtual_pointer_v1 injector.
Creating the virtual pointer adds the pointer capability to the seat. It must
*stay connected* while events are delivered (clients bind the pointer
asynchronously after the capability change), hence its `serve` mode:

```bash
cd tools/wlpoint && cargo build --release   # once
mkfifo /tmp/claude-1000/ptr.fifo
(tools/wlpoint/target/release/wlpoint serve < /tmp/claude-1000/ptr.fifo &)
exec 3>/tmp/claude-1000/ptr.fifo    # keep FIFO writer open
echo "click 236 16" >&3   # move|click|press|release <x> <y> [button] ; scroll <x> <y> <dy>
# scroll sends axis_source(wheel) + axis_discrete — plain axis events without
# a source are silently dropped by the compositor (looks like "scroll broken")
# drags (e.g. text selection): press x0 y0 → move x1 y1 … → release x1 y1
# clipboard can be verified from outside the app with wl-paste (wl-clipboard)
sleep 0.5; grim shot.png
exec 3>&-                            # closing stdin ends the daemon
```

Window coordinates: sway tiles a single window to fill the output, so with
`default_border none` surface coordinates == output coordinates:
`swaymsg -t get_tree | jq '..|select(.name? and .pid?)|{name,rect}'`

⚠️ **wlpoint's coordinate space is `WLPOINT_EXTENT`, default `1280x820`.**
At any other output resolution set `WLPOINT_EXTENT=<W>x<H>` to match, or
every click/drag is silently rescaled and lands in the wrong place — which
looks exactly like an app bug (dialogs "not opening", drops "ignored").
This has cost real debugging time twice.

## Gotchas when driving the app

- `pkill -f <pattern>` can match your own bash compound command and kill the
  shell — use `pkill -x <exact-binary-name>`.
- Keystrokes only reach the focused surface; click first if focus is unclear.
- wtype creates a fresh virtual keyboard per invocation and keystrokes sent
  before the client binds it are **silently lost** (observed: leading chars
  and even a lone `-k Return` dropped, repeatedly). The reliable pattern is
  ONE wtype invocation for the whole interaction with a leading `-s` delay,
  e.g.:
  `wtype -s 500 -M ctrl e -m ctrl -s 100 "new text" -s 150 -k Return`
  (`-s <ms>` sleeps inline; the initial 500 ms lets the client bind the
  keyboard). Do not split an interaction across multiple wtype calls.
- Fonts: only ttf-dejavu is installed. Glyphs outside DejaVu coverage render
  as tofu boxes — stick to ASCII or well-covered symbols (× ▸ ▾ ●) in UI text.
- Xvfb screenshots of the *root* are 24-bit (netpbm-convertible), but GPUI
  windows are 32-bit ARGB (`xwdtopnm` fails: "pixmap_depth > 24"); grim on
  Wayland avoids all of this.
