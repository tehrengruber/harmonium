# Harmonium

[![CI](https://github.com/tehrengruber/harmonium/actions/workflows/ci.yml/badge.svg)](https://github.com/tehrengruber/harmonium/actions/workflows/ci.yml)
[![Packages](https://github.com/tehrengruber/harmonium/actions/workflows/packages.yml/badge.svg)](https://github.com/tehrengruber/harmonium/actions/workflows/packages.yml)

A GPUI-based agent orchestrator: a terminal emulator with orchestration UI on
top. Manage projects (git repositories), spawn coding agents (claude-code)
into per-branch git worktrees derived from a plain-language task description,
and interact with each agent through an embedded terminal.

![layout](docs/layout.png)

## How it works

- **Left sidebar** — list of projects; each project expands to its agents.
  A project is simply a path to a directory containing a git repository.
  The agent name is LLM-generated from the task — for work on an existing
  PR it has the form `#<PR number> <short description>` (open PRs are fed
  to the planner via `gh pr list` when available) — and can be edited
  inline via the row's pencil button or by double-clicking the row.
  Projects are added via the *New project* row at the end of the list,
  which opens the system directory picker. The sidebar is collapsible
  (the `◂`/`▸` buttons in the bottom bar, next to the settings gear) and
  its width can be adjusted by dragging the divider; both persist.
- **Right pane** — the selected agent as a tab view. The first tab
  (*Agent*) is the agent session itself (a real PTY running claude-code);
  the `+` button opens additional plain shell tabs in the agent's workdir.
  Shell tabs persist: on restart the same tabs come back as fresh shells
  with their previous scrollback (colors included) replayed above the new
  prompt, so earlier output stays scrollable and selectable.
- **Settings** — the gear button in the sidebar's bottom bar opens the
  settings dialog: base font size (UI and terminal scale together), the
  theme (light by default, dark available — covers the UI and the
  terminal's ANSI palette), and
  **agent presets**. A preset is a named command pair — the spawn command
  (task text appended as the last argument) and the resume command. Three
  defaults ship: plain `claude`, and two sandboxed variants via
  [claude-container-isolation](https://github.com/tehrengruber/claude-container-isolation)
  (`claude-isol --local` for bubblewrap, `claude-isol` for a podman
  container). Presets are freely editable/addable/removable; everything is
  persisted.
- **Spawning an agent** (`+` next to a project) opens a dialog where you
  describe the task and pick a preset (the last-used one is preselected).
  An LLM (the `claude` CLI in print mode) derives from the task and the
  repo's branch list:
  - `existing_branch` — the task refers to an existing branch/PR: that branch
    is checked out in a worktree (reused if one already exists — git allows
    only one worktree per branch, which harmonium respects);
  - `new_branch` — a fresh kebab-case branch off a base branch (default:
    the repo's default branch);
  - `base` — work directly on the project's base checkout, no worktree.

  Then the preset's command is spawned with the task appended, in the
  resulting directory inside the embedded terminal. The agent records the
  preset's spawn and resume commands, so later restarts/resumes use the
  same setup. If the planner fails (e.g. usage/session limit reached, CLI
  missing), the error — including the planner's reply — is shown in the
  dialog and no agent or branch is created; the task text is kept so you
  can retry.

Worktrees live under `~/.local/share/harmonium/worktrees/<repo>-<hash>/<branch>`;
projects/agents persist in `~/.local/share/harmonium/state.json`. After
restarting harmonium, an agent's terminals are restored lazily the first
time it is selected: the agent session restarts with its preset's resume
command in its workdir, and its shell tabs respawn with their saved
scrollback. If a session exits or fails to spawn, a *Resume session*
button restarts it.

## Keyboard & mouse

- Terminal: full keyboard passthrough (arrows, ctrl-keys, function keys,
  alt-chords), mouse wheel scrollback, and mouse selection — drag to select,
  double-click for a word, triple-click for a line; `ctrl-shift-c` copies the
  selection, `ctrl-shift-v` pastes. Typing or pasting clears the selection.
- Text inputs (dialogs, name editing): full editing — mouse selection and
  click-to-position, arrow keys, shift-arrow selection, home/end, `ctrl-a`
  select all, `ctrl-c`/`ctrl-v`/`ctrl-x` clipboard, IME composition;
  `enter` submits, `escape` cancels. The task description input is
  multi-line (wraps and grows): `enter` inserts a newline, `up`/`down` move
  between lines, and `ctrl-enter` spawns.
- `ctrl-q` quits.

## Configuration (environment variables)

| Variable | Default | Purpose |
| --- | --- | --- |
| `HARMONIUM_AGENT_BIN` | – | Overrides the preset command entirely (testing) |
| `HARMONIUM_PLANNER_CMD` | `claude -p --model haiku` | Planner command line; the planning prompt is appended as the last argument. Haiku is the default because planning is a tiny classification task — a `claude -p` call boots a full Claude Code session (~15–19K tokens of scaffolding), which costs ~$0.14 on a premium model but ~$0.003 on haiku with a warm prompt cache. |
| `HARMONIUM_PLANNER_MODEL` | `haiku` | Sets just the planner's `--model`, keeping the default `claude -p` command. Mutually exclusive with `HARMONIUM_PLANNER_CMD` — setting both is an error. |
| `HARMONIUM_DATA_DIR` | `~/.local/share/harmonium` | State + worktrees |
| `HARMONIUM_TERMINAL_FONT` | `DejaVu Sans Mono` | Terminal font family |
| `HARMONIUM_UI_FONT` | `DejaVu Sans` | UI font family |

Base font size defaults to 12 px and is adjustable in the settings panel
(stored in `state.json`).

An agent preset command may start with shell-style `KEY=value` words, which
become environment for that agent's process:

```
CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 claude
```

`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` is **optional**: the claude CLI
normally runs on the *alternate screen*, where the transcript belongs to the
program and it drives scrolling and selection through mouse reporting (which
works out of the box, see below). Set the variable if you would rather have
the agent render inline, so its output lands in the terminal's own
scrollback and is scrolled and selected like any other output — at the cost
of claude's own scrollback UI. `CLAUDE_CODE_DISABLE_MOUSE=1` similarly hands
the mouse back to the terminal.

## Terminal scrolling & selection

- Mouse wheel scrolls the scrollback. If the running program tracks the
  mouse, the wheel is reported to it instead; on the alternate screen with
  alternate-scroll enabled it is translated to arrow keys — the same
  dispatch xterm and alacritty use, so pagers and TUIs scroll as expected.
- Programs that ask for mouse tracking (`?1000`/`?1002`/`?1003`, SGR or the
  legacy encoding) receive presses, drags and releases, so full-screen TUIs
  like the agent run their own selection and scrolling over their own
  scrollback. **Hold shift** to take the mouse back and select with the
  terminal instead — the xterm/VTE convention. Shift also keeps the wheel on
  the terminal's scrollback.
- Click-drag selects; double/triple click select word/line. Dragging past
  the top or bottom edge auto-scrolls and keeps extending the selection,
  which is how a selection grows beyond one screenful.
- `ctrl-shift-c` copies the selection, including the parts scrolled out of
  view; `ctrl-shift-v` pastes.
- The selection is owned by the terminal, not by the parser: a program that
  erases and redraws its region (any Ink-style TUI does this several times a
  second) cannot wipe a selection out from under the user.

## Building

```bash
cargo build --release
```

Needs the usual GPUI Linux dependencies (Wayland/X11 headers, fontconfig,
freetype, libxkbcommon, Vulkan loader). GPUI is pinned to Zed tag `v0.217.5`.
The exact Debian/Ubuntu package list lives in
`.github/actions/linux-build-deps/action.yml`; the Arch list is in
`packaging/arch/PKGBUILD` (and in the headless-testing skill).

`harmonium plan <repo> <task…>` prints the derived plan without starting the UI
(debugging helper).

## Testing

- `cargo test` — planner JSON parsing and the full worktree lifecycle
  (create / reuse-per-branch / existing branch / base mode) against a
  scratch git repository.
- Headless UI testing (screenshots + synthetic input under a headless
  Wayland compositor): see `.claude/skills/headless-gui-testing/SKILL.md`.
  `tools/wlpoint` is the pointer-injection helper used there.

## CI

| Workflow | Trigger | What it does |
| --- | --- | --- |
| `.github/workflows/ci.yml` | push to `main`, every PR | debug build of all targets, `cargo test`, release build (uploaded as an artifact); a second, advisory job runs clippy and prints the rustfmt diff |
| `.github/workflows/packages.yml` | push to `main`, `v*` tags, packaging-related PRs, manual | builds the Ubuntu `.deb` and the Arch package, install-tests both, and on a tag attaches them to the GitHub release |
| `.github/workflows/smoke.yml` | manual, weekly | starts the real binary under headless sway with a software Vulkan renderer and asserts the window painted; uploads the screenshot |

Clippy is advisory (`continue-on-error`) because the tree still carries a few
pre-existing lint warnings — clear them and the job can be made blocking with
`-- -D warnings`. `cargo fmt --check` is intentionally not a gate: the source
is hand-wrapped at ~80 columns, which rustfmt's defaults disagree with.

## Packaging

- **Ubuntu/Debian** — [`cargo-deb`](https://github.com/kornelski/cargo-deb),
  configured under `[package.metadata.deb]` in `Cargo.toml`:

  ```bash
  cargo install cargo-deb
  cargo deb --output dist/
  ```

  Library dependencies are derived from the binary; `libwayland-client0`,
  `libvulkan1` and `git` are declared explicitly because GPUI dlopens the
  first two and the planner shells out to the third.

- **Arch Linux** — `packaging/arch/PKGBUILD`, which builds from a `git
  archive` tarball and runs `cargo test` in its `check()`:

  ```bash
  version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  git archive --format=tar.gz --prefix="harmonium-$version/" \
    -o "packaging/arch/harmonium-$version.tar.gz" HEAD
  (cd packaging/arch && makepkg -sf)
  ```

Both install `/usr/bin/harmonium` plus `packaging/harmonium.desktop`. Tagging
`vX.Y.Z` builds both and uploads them to the matching GitHub release.

## Architecture

```
src/
  main.rs          entry point, window setup, `plan` subcommand
  workspace.rs     root view: sidebar, dialogs, agent lifecycle
  state.rs         Project/Agent records + JSON persistence
  planner.rs       LLM task planning + git worktree management
  input.rs         minimal text input widget (typing, editing keys, paste)
  theme.rs         colors & fonts
  terminal/
    mod.rs         alacritty_terminal PTY wrapper (GPUI entity), key encoding,
                   ANSI color resolution
    element.rs     custom GPUI element painting the grid + cursor
    view.rs        focusable view routing keys/scroll to the PTY
```

Known limitations (v1): removing an agent leaves its worktree on disk, and
the agent description is stored but not editable in the UI (only the name
is).
