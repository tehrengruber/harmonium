use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub type AgentId = String;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentRecord {
    pub id: AgentId,
    /// Short display name, derived from the task by the planner.
    pub name: String,
    /// Full task description; editable in the UI.
    pub description: String,
    /// Directory the agent runs in (worktree or the project base path).
    pub workdir: PathBuf,
    /// Branch checked out in `workdir`, if any. `None` means the agent works
    /// on whatever the base path has checked out.
    pub branch: Option<String>,
    /// Command line this agent was spawned with (from the preset).
    #[serde(default)]
    pub command: Option<String>,
    /// Command line used to resume this agent's session.
    #[serde(default)]
    pub resume_command: Option<String>,
    /// `KEY=value` words from the preset, given to every process started for
    /// this agent. Snapshotted at spawn like the commands above, so editing a
    /// preset doesn't change the environment of agents already spawned from it.
    #[serde(default)]
    pub env: Option<String>,
    /// Extra terminal tabs opened next to the agent tab. Persisted so the
    /// same tabs reappear after a restart; each restarts as a fresh shell
    /// in the agent's workdir.
    #[serde(default)]
    pub terminals: Vec<TerminalTabRecord>,
}

/// A plain shell terminal tab attached to an agent.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TerminalTabRecord {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProjectRecord {
    /// Path to a directory containing a git repository.
    pub path: PathBuf,
    pub name: String,
    pub agents: Vec<AgentRecord>,
    #[serde(default = "default_true")]
    pub expanded: bool,
}

fn default_true() -> bool {
    true
}

impl ProjectRecord {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            path,
            name,
            agents: Vec::new(),
            expanded: true,
        }
    }
}

impl PresetRecord {
    /// The command line that resumes a session started by this preset.
    pub fn resume_command(&self) -> String {
        format!("{} {RESUME_FLAG}", self.command.trim())
    }

    /// The command line that runs this preset's agent as the planner: print
    /// mode, a model, and (appended by the planner itself) the prompt.
    pub fn planner_command(&self, model: &str) -> String {
        format!(
            "{} {} --model {model}",
            self.command.trim(),
            PLANNER_FLAGS.join(" ")
        )
    }

    /// Fold a pre-`RESUME_FLAG` state file's second command line into this
    /// one. The old `resume_command` was the command repeated with a resume
    /// flag on the end; anything *between* the two was a separator the wrapper
    /// needs (`--` for claude-isol), which now belongs to the command itself.
    fn migrate_resume_command(&mut self) {
        let Some(legacy) = self.resume_command.take() else {
            return;
        };
        let (legacy, command) = (legacy.trim(), self.command.trim().to_string());
        if let Some(separator) = legacy
            .strip_prefix(&command)
            .and_then(|rest| rest.trim().strip_suffix(RESUME_FLAG))
        {
            let separator = separator.trim();
            if !separator.is_empty() {
                self.command = format!("{command} {separator}");
            }
            return;
        }
        // Hand-written and not expressible as "command + flag" any more. Say
        // so rather than silently changing what the preset does.
        crate::log::error(format!(
            "preset `{}`: resume command `{legacy}` cannot be expressed as `{command} \
             {RESUME_FLAG}` — resuming will use the latter; adjust the preset if that is wrong",
            self.name
        ));
    }
}

/// Appended to a preset's command to resume its session instead of starting a
/// new one. Every agent CLI harmonium ships a preset for spells it this way.
pub const RESUME_FLAG: &str = "--continue";

/// Flags that put an agent CLI in non-interactive print mode for planning.
/// `--model` is appended after these, then the prompt.
pub const PLANNER_FLAGS: [&str; 1] = ["-p"];

/// Which workspace a new agent gets. The task description always decides the
/// agent's *name*; this only decides where it works.
///
/// Deliberately not persisted: the spawn dialog opens on `Auto` every time.
/// Forcing a workspace is a decision about one task, so carrying it over to
/// the next one would quietly put work in the wrong place.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Derived from the task description, along with everything else: a new
    /// branch, an existing one the task refers to, or the base checkout.
    #[default]
    Auto,
    /// Always a worktree on its own branch, whatever the task sounds like.
    NewWorktree,
    /// Always the project's own checkout — no worktree, no branch of its own.
    MainBranch,
}

impl WorkspaceMode {
    pub const ALL: [WorkspaceMode; 3] = [
        WorkspaceMode::Auto,
        WorkspaceMode::NewWorktree,
        WorkspaceMode::MainBranch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            WorkspaceMode::Auto => "Auto",
            WorkspaceMode::NewWorktree => "New worktree",
            WorkspaceMode::MainBranch => "Main branch",
        }
    }
}

/// A named agent command configuration selectable when spawning a task.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PresetRecord {
    pub name: String,
    /// How to launch the agent. Everything else is appended to it as further
    /// arguments: the task text when spawning, [`RESUME_FLAG`] when resuming,
    /// the planner's flags and prompt when this preset plans. A wrapper that
    /// needs a separator before the agent's own flags carries it here — the
    /// shipped `claude-isol` presets end in `--` for exactly that reason.
    pub command: String,
    /// Legacy: a second, near-identical command line for resuming. Read from
    /// old state files and folded into nothing — the flag is appended now —
    /// but never written back.
    #[serde(default, skip_serializing)]
    pub resume_command: Option<String>,
    /// Environment for the agent's processes, written as shell-style
    /// `KEY=value` words (`FOO=bar BAZ="a b"`). Applies to the agent session,
    /// its resumes and its shell tabs, unlike a `KEY=value` prefix on
    /// `command`, which only reaches the process that command starts.
    #[serde(default)]
    pub env: String,
}

/// Whether `program` could actually be exec'd. Agent commands are exec'd
/// directly, without a shell, so this mirrors what the spawn will do: a name
/// is looked up in `PATH`, anything containing a `/` is taken as a path (`~`
/// included — nothing would expand it, so such a command genuinely can't run).
/// Only consulted when seeding defaults; saved presets are the user's list and
/// are never second-guessed.
fn program_available(program: &str) -> bool {
    fn executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    if program.is_empty() {
        return false;
    }
    if program.contains('/') {
        return executable(Path::new(program));
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .any(|dir| executable(&Path::new(dir).join(program)))
}

/// The presets a fresh install starts with. The sandboxed ones are only
/// created when `claude-isol` is actually installed: a preset that can't be
/// exec'd is a spawn failure waiting to happen, and most machines don't have
/// it. This is a one-time seeding decision — installing `claude-isol` later
/// doesn't retro-add them, since persisted presets are the user's own list.
pub fn default_presets() -> Vec<PresetRecord> {
    // The isolated presets mount the main repository at its own path: an
    // agent's workdir is usually a worktree whose `.git` is a file pointing
    // into `<git root>/.git/worktrees/…`, so a sandbox that sees only the
    // workdir has no working git. `-v` is repeatable and understood by both
    // modes (bubblewrap translates it to a bind), and it must come before the
    // `--` separator — everything after that goes to claude.
    // The isolated presets end in `--`: everything harmonium appends (the
    // task, --continue, the planner's flags) is meant for claude, not for
    // claude-isol, which rejects options it doesn't know.
    const MOUNT_GIT_ROOT: &str = "-v $HARMONIUM_TASK_GIT_ROOT:$HARMONIUM_TASK_GIT_ROOT";
    let mut presets = vec![PresetRecord {
        name: "claude-code".into(),
        command: "claude".into(),
        resume_command: None,
        env: String::new(),
    }];
    if program_available("claude-isol") {
        presets.push(PresetRecord {
            name: "claude-code isolated bubblewrap".into(),
            command: format!("claude-isol --local {MOUNT_GIT_ROOT} --"),
            resume_command: None,
            env: String::new(),
        });
        presets.push(PresetRecord {
            name: "claude-code isolated container".into(),
            command: format!("claude-isol {MOUNT_GIT_ROOT} --"),
            resume_command: None,
            env: String::new(),
        });
    }
    presets
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SettingsRecord {
    pub font_size: f32,
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    pub presets: Vec<PresetRecord>,
    /// Index of the preset last used to spawn an agent (preselected in the
    /// spawn dialog).
    pub last_preset: usize,
    pub theme: crate::theme::ThemeMode,
    /// Name of the agent preset the planner runs through, so planning happens
    /// with the same binary, wrapper and environment as the work itself.
    /// `None` — or a name no preset has — means plain `claude`.
    #[serde(default)]
    pub planner_preset: Option<String>,
    /// Full planner command line; when set it replaces both the preset and
    /// the default, and the model is unused.
    pub planner_command: String,
    /// Model for the planner, appended as `--model <model>` to whichever
    /// command the preset (or the default) provides.
    pub planner_model: String,
    pub terminal_font: String,
    pub ui_font: String,
    /// Whether the log panel is showing under the terminal.
    pub log_panel_open: bool,
}

pub const DEFAULT_SIDEBAR_WIDTH: f32 = 280.;

impl Default for SettingsRecord {
    fn default() -> Self {
        Self {
            font_size: crate::theme::DEFAULT_FONT_SIZE,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_collapsed: false,
            presets: default_presets(),
            last_preset: 0,
            theme: crate::theme::ThemeMode::default(),
            planner_preset: None,
            planner_command: String::new(),
            planner_model: crate::planner::DEFAULT_MODEL.to_string(),
            terminal_font: crate::theme::DEFAULT_TERMINAL_FONT.to_string(),
            ui_font: crate::theme::DEFAULT_UI_FONT.to_string(),
            log_panel_open: false,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct AppState {
    pub projects: Vec<ProjectRecord>,
    #[serde(default)]
    pub settings: SettingsRecord,
}

impl AppState {
    pub fn agent_mut(&mut self, id: &str) -> Option<&mut AgentRecord> {
        self.projects
            .iter_mut()
            .flat_map(|p| p.agents.iter_mut())
            .find(|a| a.id == id)
    }

    pub fn agent(&self, id: &str) -> Option<&AgentRecord> {
        self.projects
            .iter()
            .flat_map(|p| p.agents.iter())
            .find(|a| a.id == id)
    }

    /// The project an agent belongs to. Agents don't store their project, but
    /// spawning needs its path (the main repository root), so resolve it from
    /// state on every spawn/resume/restore.
    pub fn project_for_agent(&self, id: &str) -> Option<&ProjectRecord> {
        self.projects
            .iter()
            .find(|p| p.agents.iter().any(|a| a.id == id))
    }
}

/// Root directory for harmonium's own data (state file, managed worktrees).
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HARMONIUM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return Path::new(&xdg).join("harmonium");
    }
    if let Ok(home) = std::env::var("HOME") {
        return Path::new(&home).join(".local/share/harmonium");
    }
    PathBuf::from(".harmonium")
}

/// Directory where per-terminal scrollback history files are stored.
pub fn history_dir() -> PathBuf {
    data_dir().join("history")
}

/// File path for a terminal tab's saved scrollback history.
pub fn terminal_history_path(terminal_id: &str) -> PathBuf {
    history_dir().join(format!("{terminal_id}.txt"))
}

fn state_file() -> PathBuf {
    data_dir().join("state.json")
}

pub fn load_state() -> AppState {
    let path = state_file();
    let mut state: AppState = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => AppState::default(),
    };
    for preset in &mut state.settings.presets {
        preset.migrate_resume_command();
    }
    state
}

pub fn save_state(state: &AppState) -> Result<()> {
    let path = state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_availability_matches_what_exec_would_find() {
        assert!(program_available("sh"));
        assert!(program_available("/bin/sh"));
        assert!(!program_available("harmonium-no-such-program"));
        assert!(!program_available("/bin/harmonium-no-such-program"));
        assert!(!program_available(""));
        // A directory is not something exec can run.
        assert!(!program_available("/bin"));
        // `~` is never expanded for a directly exec'd command.
        assert!(!program_available("~/bin/sh"));
    }

    fn preset(command: &str, resume: Option<&str>) -> PresetRecord {
        PresetRecord {
            name: "p".into(),
            command: command.into(),
            resume_command: resume.map(str::to_string),
            env: String::new(),
        }
    }

    #[test]
    fn resume_and_planner_commands_are_appended_to_the_one_command() {
        let plain = preset("claude", None);
        assert_eq!(plain.resume_command(), "claude --continue");
        assert_eq!(plain.planner_command("haiku"), "claude -p --model haiku");

        // A wrapper carries its separator in the command, so everything
        // appended lands on the agent rather than on the wrapper.
        let wrapped = preset("claude-isol -v /r:/r --", None);
        assert_eq!(wrapped.resume_command(), "claude-isol -v /r:/r -- --continue");
        assert_eq!(
            wrapped.planner_command("haiku"),
            "claude-isol -v /r:/r -- -p --model haiku"
        );
    }

    #[test]
    fn legacy_resume_commands_migrate_into_the_command() {
        // Plain duplication: nothing to keep.
        let mut p = preset("claude", Some("claude --continue"));
        p.migrate_resume_command();
        assert_eq!(p.command, "claude");
        assert_eq!(p.resume_command, None);
        assert_eq!(p.resume_command(), "claude --continue");

        // The separator between the two is what the wrapper needs, and moves
        // into the command so spawning passes the task through it too.
        let mut p = preset(
            "claude-isol -v /r:/r",
            Some("claude-isol -v /r:/r -- --continue"),
        );
        p.migrate_resume_command();
        assert_eq!(p.command, "claude-isol -v /r:/r --");
        assert_eq!(p.resume_command(), "claude-isol -v /r:/r -- --continue");

        // Not expressible: the command is left alone (and a note is logged).
        let mut p = preset("claude", Some("claude --resume deadbeef"));
        p.migrate_resume_command();
        assert_eq!(p.command, "claude");
    }

    #[test]
    fn isolated_presets_only_created_when_installed() {
        let isolated = default_presets()
            .iter()
            .filter(|preset| preset.command.starts_with("claude-isol"))
            .count();
        let expected = if program_available("claude-isol") { 2 } else { 0 };
        assert_eq!(isolated, expected);
        // The plain preset is always there.
        assert!(default_presets().iter().any(|preset| preset.command == "claude"));
    }
}
