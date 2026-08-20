# Harmonium — agent notes

## Build & test

- `CARGO_HOME` defaults to `~/.cargo`, which may be read-only here — use
  `export CARGO_HOME=$PWD/.cargo-home` before cargo commands.
- `cargo test` covers the planner and worktree lifecycle. For visual/UI
  verification use the `headless-gui-testing` skill
  (`.claude/skills/headless-gui-testing/SKILL.md`).
- When a test needs the real `claude` CLI running inside a terminal (e.g.
  reproducing TUI rendering or selection behaviour), **always pin a cheap
  model**: `claude --model haiku`. The same goes for the planner, which
  already defaults to haiku (`HARMONIUM_PLANNER_MODEL`). Never drive tests
  with the default premium model.
- `HARMONIUM_AGENT_BIN` replaces the agent command entirely, which is the
  easiest way to put a scripted or cheap stand-in in the agent tab.

## Sandbox: sudo may be impossible — stop and ask

Sessions run inside claude-container-isolation
(github.com/tehrengruber/claude-container-isolation). In `--local`
(bubblewrap) mode sudo can never work: bwrap sets no-new-privileges and
uid-maps root away. Symptoms: `sudo: The "no new privileges" flag is set`,
`/etc/sudo.conf is owned by uid 65534`, missing pacman database.

If you hit this and the task needs system packages, **stop and ask the
user** to either restart the session in container mode (without `--local`)
or install the packages host-side. Do not build no-root workarounds
(manual package extraction, prefix installs) — they waste time and clutter
the repo.
