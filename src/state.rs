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

/// A named agent command configuration selectable when spawning a task.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PresetRecord {
    pub name: String,
    /// Command line used to spawn the agent; the task text is appended as
    /// the final argument.
    pub command: String,
    /// Command line used to resume the agent's session in its workdir.
    pub resume_command: String,
}

pub fn default_presets() -> Vec<PresetRecord> {
    vec![
        PresetRecord {
            name: "claude-code".into(),
            command: "claude".into(),
            resume_command: "claude --continue".into(),
        },
        PresetRecord {
            name: "claude-code isolated bubblewrap".into(),
            command: "claude-isol --local".into(),
            resume_command: "claude-isol --local -- --continue".into(),
        },
        PresetRecord {
            name: "claude-code isolated container".into(),
            command: "claude-isol".into(),
            resume_command: "claude-isol -- --continue".into(),
        },
    ]
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
