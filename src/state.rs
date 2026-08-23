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

/// Which workspace a new agent gets. The task description always decides the
/// agent's *name*; this only decides where it works.
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
    /// Command line used to spawn the agent; the task text is appended as
    /// the final argument.
    pub command: String,
    /// Command line used to resume the agent's session in its workdir.
    pub resume_command: String,
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
    const MOUNT_GIT_ROOT: &str = "-v $HARMONIUM_TASK_GIT_ROOT:$HARMONIUM_TASK_GIT_ROOT";
    let mut presets = vec![PresetRecord {
        name: "claude-code".into(),
        command: "claude".into(),
        resume_command: "claude --continue".into(),
        env: String::new(),
    }];
    if program_available("claude-isol") {
        presets.push(PresetRecord {
            name: "claude-code isolated bubblewrap".into(),
            command: format!("claude-isol --local {MOUNT_GIT_ROOT}"),
            resume_command: format!("claude-isol --local {MOUNT_GIT_ROOT} -- --continue"),
            env: String::new(),
        });
        presets.push(PresetRecord {
            name: "claude-code isolated container".into(),
            command: format!("claude-isol {MOUNT_GIT_ROOT}"),
            resume_command: format!("claude-isol {MOUNT_GIT_ROOT} -- --continue"),
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
    /// Workspace choice last used to spawn an agent, likewise preselected.
    pub last_workspace_mode: WorkspaceMode,
    pub theme: crate::theme::ThemeMode,
    /// Full planner command line; when set it replaces the default
    /// `claude -p --model <planner_model>` and the model is unused.
    pub planner_command: String,
    /// Model for the default planner command.
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
            last_workspace_mode: WorkspaceMode::default(),
            theme: crate::theme::ThemeMode::default(),
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
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => AppState::default(),
    }
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
