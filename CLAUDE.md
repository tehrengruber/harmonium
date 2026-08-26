# Harmonium — agent notes

## Build & test

- **Before the first cargo command in a session, check `CARGO_TARGET_DIR` is
  set** (`env | grep CARGO`). If it isn't, **stop and ask the user** for
  `.claude/settings.local.json` — do not build, and do not set the variables
  by hand for one command. That file is personal and gitignored, so a fresh
  worktree does not have it; it must look like this, with `<git root>` the
  directory `git rev-parse --path-format=absolute --git-common-dir` names
  (minus the trailing `/.git`):

  ```json
  {
    "env": {
      "CARGO_HOME": "<git root>/.cargo-home",
      "CARGO_TARGET_DIR": "<git root>/target"
    }
  }
  ```

  Building without it is not a slower build, it is a wasted one: cargo falls
  back to `~/.cargo` and the worktree's own `target/`, both tmpfs, re-downloads
  the registry, recompiles gpui, wgpu and alacritty_terminal from scratch into
  RAM, and throws all of it away when the session ends.
- Those paths are the main checkout's, shared by every worktree, and only
  reachable because the session bind-mounts the git root — see the sandbox
  section. If a build starts recompiling gpui, wgpu and alacritty_terminal from
  scratch, check the mount before anything else. Do **not** "fix" it by
  pointing either variable at `$PWD` — a per-worktree cache re-downloads the
  registry and rebuilds every dependency for no benefit. Two builds at once
  serialize on cargo's target-dir lock (`Blocking waiting for file lock`);
  that's expected. `target/debug/harmonium` is one path shared by all
  checkouts, so it holds whichever build ran last; copy the binary aside if you
  need it to stay yours.
- `cargo test` covers the planner and worktree lifecycle. For visual/UI
  verification use the `headless-gui-testing` skill
  (`.claude/skills/headless-gui-testing/SKILL.md`).
- **Nothing you run for testing may touch `~/.local/share/harmonium`** — a
  harmonium may be running out of it, and a second one there takes its session
  lock, saves over its `state.json` on the way out, and can delete worktrees
  live agents are working in. `cargo test` is safe on its own: under
  `cfg(test)` `data_dir()` refuses to resolve to a real data directory and
  falls back to a per-process temp dir. **Running the binary is not** — always
  start it with `HARMONIUM_DATA_DIR` pointing somewhere disposable.
- When a test needs the real `claude` CLI running inside a terminal (e.g.
  reproducing TUI rendering or selection behaviour), **always pin a cheap
  model**: `claude --model haiku`. The same goes for the planner, which
  already defaults to haiku (`HARMONIUM_PLANNER_MODEL`). Never drive tests
  with the default premium model.
- `HARMONIUM_AGENT_BIN` replaces the agent command entirely, which is the
  easiest way to put a scripted or cheap stand-in in the agent tab.

## Sandbox: what is actually mounted

Sessions run inside claude-container-isolation
(github.com/tehrengruber/claude-container-isolation), and only a few host
directories come along. Check with `findmnt` before assuming a path exists:

- **This worktree** — bind-mounted at its real path, so edits here persist.
- **The git root** (`~/Development/harmonium`), mounted by the preset's
  `-v $HARMONIUM_TASK_GIT_ROOT:$HARMONIUM_TASK_GIT_ROOT`. Two things depend
  on it: the shared cargo cache above, and git itself — a worktree's `.git`
  is a *file* pointing into `<git root>/.git/worktrees/…`, so without the
  mount every git command fails with `fatal: not a repository`.
- **`~/.claude`**, `~/.claude.json`, `~/.config/gh`, `~/.gitconfig` — Claude
  Code's own state. Not a scratch space: keep build caches out of it.
- **Everything else under `/home/tille` and `/` is tmpfs**: writes go to RAM
  and vanish with the session. Sibling worktrees are not mounted either, so
  the git root is the only place two worktrees can share anything.
- If the git root is missing, don't work around it — no repairing `.git`, no
  private cache. Say so and let the user restart the session with the mount.

## Sandbox: sudo may be impossible — stop and ask

In `--local`
(bubblewrap) mode sudo can never work: bwrap sets no-new-privileges and
uid-maps root away. Symptoms: `sudo: The "no new privileges" flag is set`,
`/etc/sudo.conf is owned by uid 65534`, missing pacman database.

If you hit this and the task needs system packages, **stop and ask the
user** to either restart the session in container mode (without `--local`)
or install the packages host-side. Do not build no-root workarounds
(manual package extraction, prefix installs) — they waste time and clutter
the repo.
