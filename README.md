# Harmonium

A GPUI-based agent orchestrator: a terminal emulator with orchestration UI on
top. Manage projects (git repositories), spawn coding agents (claude-code)
into per-branch git worktrees derived from a plain-language task description,
and interact with each agent through an embedded terminal.

![layout](docs/layout.png)

## How it works

- **Left sidebar** — list of projects; each project expands to its agents
  (name + branch). A project is simply a path to a directory containing a
  git repository. The agent name is LLM-generated from the task — for work
  on an existing PR it has the form `#<PR number> <short description>`
  (open PRs are fed to the planner via `gh pr list` when available) — and
  can be edited inline via the row's *edit* button or by double-clicking
  the row. The sidebar is collapsible (the `◂`/`▸` buttons) and its width
  can be adjusted by dragging the divider; both persist.
- **Right pane** — the selected agent's terminal (a real PTY running
  claude-code), filling the pane.
- **Settings** — the gear button at the bottom of the sidebar opens the
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
projects/agents persist in `~/.local/share/harmonium/state.json`. Agents without
a live terminal (e.g. after restarting harmonium) offer a *Resume session*
button that runs `claude --continue` in the agent's workdir.

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
| `HARMONIUM_DATA_DIR` | `~/.local/share/harmonium` | State + worktrees |
| `HARMONIUM_TERMINAL_FONT` | `DejaVu Sans Mono` | Terminal font family |
| `HARMONIUM_UI_FONT` | `DejaVu Sans` | UI font family |

Base font size defaults to 12 px and is adjustable in the settings panel
(stored in `state.json`).

## Building

```bash
cargo build --release
```

Needs the usual GPUI Linux dependencies (Wayland/X11 headers, fontconfig,
freetype, libxkbcommon, Vulkan loader). GPUI is pinned to Zed tag `v0.217.5`.

`harmonium plan <repo> <task…>` prints the derived plan without starting the UI
(debugging helper).

## Testing

- `cargo test` — planner JSON parsing and the full worktree lifecycle
  (create / reuse-per-branch / existing branch / base mode) against a
  scratch git repository.
- Headless UI testing (screenshots + synthetic input under a headless
  Wayland compositor): see `.claude/skills/headless-gui-testing/SKILL.md`.
  `tools/wlpoint` is the pointer-injection helper used there.

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

Known limitations (v1): removing an agent leaves its worktree on disk,
terminal selection does not auto-scroll past the viewport edge, the agent
description is stored but not editable in the UI (only the name is).
