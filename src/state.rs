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

    /// Give a sandboxed preset the git-root mount if it was saved before that
    /// became a default. Without it the agent's worktree has no working git —
    /// `.git` there is a file pointing into `<git root>/.git/worktrees/…`,
    /// which the sandbox can't see — so this is repairing a broken sandbox,
    /// not overriding a preference.
    fn migrate_git_root_mount(&mut self) {
        let Some(command) = with_git_root_mount(&self.command) else {
            return;
        };
        crate::log::info(format!(
            "preset `{}`: added the git root mount ({MOUNT_GIT_ROOT})",
            self.name
        ));
        self.command = command;
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

/// Mounts the project's git root into a sandboxed agent at its own path. An
/// agent's workdir is usually a worktree whose `.git` is a *file* pointing
/// into `<git root>/.git/worktrees/…`, so a sandbox that sees only the workdir
/// has no working git — and neither do the build caches that live next to it.
/// `-v` is repeatable, understood by both of `claude-isol`'s modes, and must
/// come before the `--` separator: everything after that goes to the agent.
pub const MOUNT_GIT_ROOT: &str = "-v $HARMONIUM_TASK_GIT_ROOT:$HARMONIUM_TASK_GIT_ROOT";

/// The sandbox wrapper the mount is meaningful for. Other commands are left
/// alone: harmonium has no way to know how they spell a bind mount.
const ISOLATION_WRAPPER: &str = "claude-isol";

/// `command` with [`MOUNT_GIT_ROOT`] inserted, or `None` if it doesn't need it
/// — not a sandboxed command, or already mounting the git root.
///
/// The fragment goes in front of the `--` separator when there is one, so it
/// reaches the wrapper rather than the agent behind it. That also makes this
/// correct for a stored resume command (`… -- --continue`), where appending
/// would put the mount on the wrong side of the separator.
fn with_git_root_mount(command: &str) -> Option<String> {
    let program = command.split_whitespace().next()?;
    let program = program.rsplit('/').next().unwrap_or(program);
    if program != ISOLATION_WRAPPER || command.contains("HARMONIUM_TASK_GIT_ROOT") {
        return None;
    }
    let mut words: Vec<&str> = command.split_whitespace().collect();
    let at = words.iter().position(|word| *word == "--").unwrap_or(words.len());
    words.splice(at..at, MOUNT_GIT_ROOT.split(' '));
    Some(words.join(" "))
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
    // The isolated presets mount the main repository at its own path (see
    // `MOUNT_GIT_ROOT`) and end in `--`: everything harmonium appends (the
    // task, --continue, the planner's flags) is meant for claude, not for
    // claude-isol, which rejects options it doesn't know.
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
    // A test must never end up on the directory a real harmonium is running
    // out of. There it would take the session lock, save its own idea of the
    // session over the file on the way out, or remove worktrees that live
    // agents are sitting in. Tests that want somewhere to put files still set
    // the variable; this is the floor under the ones that don't think about it
    // at all. It does nothing for the *binary* under test — a harmonium
    // launched by a GUI test is an ordinary release of it and needs
    // `HARMONIUM_DATA_DIR` set for it.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("harmonium-test-data-{}", std::process::id()));
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

pub fn state_file() -> PathBuf {
    data_dir().join("state.json")
}

/// Name of the copy a save is parked in when the state file turns out to have
/// been written by something else. It sits next to the state file so both
/// halves of the conflict are in one place.
const REJECTED_FILE_NAME: &str = "state.rejected.json";

/// The file a running harmonium holds a lock on, claiming the session.
///
/// A file of its own, rather than the lock being taken on `state.json`: a save
/// replaces that file by rename, so a lock held on it would sit on an inode
/// that is no longer at that path, and the next harmonium would lock the
/// replacement and see nothing amiss.
const LOCK_FILE_NAME: &str = "state.lock";

/// The session file, together with the state harmonium last agreed with the
/// disk on. Every save checks the file against that stamp first, so an edit
/// made behind harmonium's back is refused instead of overwritten: the
/// in-memory session is authoritative only for as long as nothing else writes
/// the file.
pub struct StateFile {
    path: PathBuf,
    /// `None` if the file wasn't there when last looked at.
    stamp: Option<Stamp>,
    /// Held open, and locked, for as long as this handle lives: it is what
    /// keeps a second harmonium from opening the same session. Released by the
    /// kernel when the process ends, however it ends.
    _lock: std::fs::File,
}

/// What the state file looked like when harmonium last read or wrote it.
/// Modification time and length: a foreign write moves at least one of the
/// two, unless it lands within the same filesystem timestamp tick *and* keeps
/// the byte count exactly — which an edit from a person or another harmonium
/// won't. Never persisted; only compared with another stamp from the same run.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
struct Stamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

impl Stamp {
    /// The file's current stamp, or `None` if it isn't there.
    fn of(path: &Path) -> Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(Self {
                // Absent on filesystems that don't keep one, in which case the
                // length carries the comparison on its own.
                modified: meta.modified().ok(),
                len: meta.len(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
        }
    }
}

/// Why the session couldn't be opened.
#[derive(Debug)]
pub enum LoadError {
    /// Another harmonium has it. Two of them would each save their own idea of
    /// the session over the other's, so the second one doesn't start.
    Locked { path: PathBuf },
    /// Missing is fine and yields a default session; this is the file being
    /// there but unreadable, or in a format this version can't parse.
    Unreadable(anyhow::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Locked { path } => write!(
                f,
                "another harmonium is already running (it holds {})",
                path.display()
            ),
            Self::Unreadable(error) => write!(f, "{error:#}"),
        }
    }
}

/// Why a save didn't happen.
#[derive(Debug)]
pub enum SaveError {
    /// The file changed since harmonium last read or wrote it. Nothing was
    /// written to it; the session went to `rejected` instead, unless writing
    /// that failed too.
    Conflict {
        path: PathBuf,
        rejected: Option<PathBuf>,
    },
    Io(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict {
                path,
                rejected: Some(rejected),
            } => write!(
                f,
                "{} changed on disk — not saving over it. This session was written to {} instead; \
                 merge the two and restart harmonium.",
                path.display(),
                rejected.display()
            ),
            Self::Conflict {
                path,
                rejected: None,
            } => write!(
                f,
                "{} changed on disk — not saving over it, and the copy of this session could not \
                 be written either (see the log). Nothing is being saved.",
                path.display()
            ),
            Self::Io(error) => write!(f, "{error:#}"),
        }
    }
}

/// Read the saved session without claiming it, for callers that only look and
/// never save. They mustn't be kept out by a running harmonium — and, taking no
/// lock, they may see a session that changes under them.
pub fn read_state() -> Result<AppState> {
    Ok(read_at(&state_file())?.unwrap_or_default())
}

/// The parsed contents of `path`, or `None` if there is no file there. A
/// missing file is a fresh install; anything else — unreadable, or written by a
/// version whose format this one doesn't understand — is an error rather than a
/// silent default, because the caller would go on to save over the file and
/// take every project and preset in it with them.
fn read_at(path: &Path) -> Result<Option<AppState>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    parse_state(&contents)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

impl StateFile {
    /// Claim the saved session and read it. Fails if another harmonium holds
    /// it, or if the file is there but can't be read (see `read_at`).
    pub fn load() -> Result<(Self, AppState), LoadError> {
        Self::load_from(state_file())
    }

    fn load_from(path: PathBuf) -> Result<(Self, AppState), LoadError> {
        let lock = claim(&path)?;
        // Under the lock, so the only writer that can slip between the read and
        // the stamp is one that isn't harmonium.
        let state = read_at(&path).map_err(LoadError::Unreadable)?;
        let stamp = Stamp::of(&path).map_err(LoadError::Unreadable)?;
        Ok((
            Self {
                path,
                stamp,
                _lock: lock,
            },
            state.unwrap_or_default(),
        ))
    }

    /// Replace the file's contents with `state` — unless somebody else has
    /// written it since this handle last looked, in which case neither side is
    /// lost: the file is left alone and the session is parked next to it.
    pub fn save(&mut self, state: &AppState) -> Result<(), SaveError> {
        let json = serde_json::to_string_pretty(state)
            .context("serialising the session")
            .map_err(SaveError::Io)?;
        if self.changed_on_disk().map_err(SaveError::Io)? {
            return Err(SaveError::Conflict {
                path: self.path.clone(),
                rejected: self.write_rejected(&json),
            });
        }
        write_atomic(&self.path, &json).map_err(SaveError::Io)?;
        // Stamping what was just written can only fail if the file went away
        // again underneath us. Keeping the old stamp then makes the next save
        // refuse, which is the safe way round.
        self.stamp = Stamp::of(&self.path).unwrap_or(self.stamp);
        Ok(())
    }

    /// Whether the file is something other than what this handle put there (or
    /// read from it).
    fn changed_on_disk(&self) -> Result<bool> {
        match Stamp::of(&self.path)? {
            // Gone. Whoever removed it — or moved it aside, which is exactly
            // what harmonium asks for when a state file won't parse — left
            // nothing behind to destroy, so recreating it costs nobody data.
            None => Ok(false),
            found => Ok(found != self.stamp),
        }
    }

    /// Park a save that would have clobbered somebody else's write. The
    /// previous rejected copy is overwritten rather than kept: saves keep
    /// arriving for as long as the session runs, and it is the newest one that
    /// is worth reconciling.
    fn write_rejected(&self, json: &str) -> Option<PathBuf> {
        let path = self.path.with_file_name(REJECTED_FILE_NAME);
        match write_atomic(&path, json) {
            Ok(()) => Some(path),
            Err(error) => {
                crate::log::error(format!("could not write {}: {error:#}", path.display()));
                None
            }
        }
    }
}

/// Take the session lock that goes with the state file at `path`. The returned
/// handle holds it; dropping it, or the process ending, releases it.
fn claim(path: &Path) -> Result<std::fs::File, LoadError> {
    let lock_path = path.with_file_name(LOCK_FILE_NAME);
    let open = || -> Result<std::fs::File> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))
    };
    let file = open().map_err(LoadError::Unreadable)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(LoadError::Locked { path: lock_path }),
        // Advisory locks aren't supported here (some network filesystems).
        // Refusing to start over that would be worse than the race it
        // prevents, so this only gets a note in the log.
        Err(std::fs::TryLockError::Error(error)) => {
            crate::log::error(format!(
                "could not lock {}: {error} — another harmonium could overwrite this session",
                lock_path.display()
            ));
            Ok(file)
        }
    }
}

/// Write `contents` to `path` by way of a temporary file in the same directory.
/// A reader then sees either the whole old file or the whole new one, and a
/// crash mid-write can't leave a half-written session behind — which, now that
/// an unparseable state file stops harmonium, would be a session that refuses
/// to start.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // The pid keeps two harmoniums from writing one temporary file.
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(name);
    let written = std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, path)
                .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn parse_state(contents: &str) -> Result<AppState> {
    let mut state: AppState = serde_json::from_str(contents)?;
    for preset in &mut state.settings.presets {
        preset.migrate_resume_command();
        preset.migrate_git_root_mount();
    }
    // Agents snapshot their preset's command line at spawn, so fixing the
    // presets alone would leave every agent that already exists resuming into
    // a sandbox with no git. The snapshot is there to keep *preference*
    // changes away from running agents; a missing mount isn't one.
    for agent in state.projects.iter_mut().flat_map(|p| p.agents.iter_mut()) {
        for command in [&mut agent.command, &mut agent.resume_command]
            .into_iter()
            .filter_map(Option::as_mut)
        {
            if let Some(mounted) = with_git_root_mount(command) {
                crate::log::info(format!(
                    "agent `{}`: added the git root mount to `{command}`",
                    agent.name
                ));
                *command = mounted;
            }
        }
    }
    Ok(state)
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
    fn sandboxed_commands_gain_the_git_root_mount() {
        // Before the separator, so the wrapper gets it and not the agent.
        assert_eq!(
            with_git_root_mount("claude-isol --local --").unwrap(),
            format!("claude-isol --local {MOUNT_GIT_ROOT} --")
        );
        // A stored resume command has words on both sides of the separator.
        assert_eq!(
            with_git_root_mount("claude-isol --local -- --continue").unwrap(),
            format!("claude-isol --local {MOUNT_GIT_ROOT} -- --continue")
        );
        // Pre-separator command lines: nothing to insert in front of.
        assert_eq!(
            with_git_root_mount("claude-isol").unwrap(),
            format!("claude-isol {MOUNT_GIT_ROOT}")
        );
        // Invoked by path.
        assert!(with_git_root_mount("/opt/bin/claude-isol --")
            .unwrap()
            .contains(MOUNT_GIT_ROOT));

        // Already mounting it — including under a hand-written mount that
        // spells the same variable differently.
        assert_eq!(with_git_root_mount("claude-isol -v $HARMONIUM_TASK_GIT_ROOT:/repo --"), None);
        assert_eq!(
            with_git_root_mount(&format!("claude-isol {MOUNT_GIT_ROOT} --")),
            None
        );
        // Not a sandboxed command: harmonium doesn't know how these spell a
        // bind mount, so it leaves them alone.
        assert_eq!(with_git_root_mount("claude"), None);
        assert_eq!(with_git_root_mount("docker run claude-isol"), None);
        assert_eq!(with_git_root_mount(""), None);
    }

    #[test]
    fn saved_presets_and_agents_are_migrated_on_load() {
        let state = parse_state(
            r#"{
                "projects": [{
                    "path": "/repo", "name": "repo", "agents": [{
                        "id": "a", "name": "fix-login", "description": "…",
                        "workdir": "/worktrees/fix-login", "branch": "fix-login",
                        "command": "claude-isol --local --",
                        "resume_command": "claude-isol --local -- --continue"
                    }]
                }],
                "settings": {"presets": [{"name": "isolated", "command": "claude-isol --local --"}]}
            }"#,
        )
        .unwrap();

        let preset = &state.settings.presets[0];
        assert_eq!(preset.command, format!("claude-isol --local {MOUNT_GIT_ROOT} --"));
        assert_eq!(
            preset.resume_command(),
            format!("claude-isol --local {MOUNT_GIT_ROOT} -- {RESUME_FLAG}")
        );

        // The agent's own snapshot too, or it would resume without git.
        let agent = &state.projects[0].agents[0];
        assert_eq!(
            agent.resume_command.as_deref().unwrap(),
            format!("claude-isol --local {MOUNT_GIT_ROOT} -- --continue")
        );
        assert!(agent.command.as_deref().unwrap().contains(MOUNT_GIT_ROOT));
    }

    #[test]
    fn state_that_does_not_parse_is_reported_not_defaulted() {
        // A file from a version whose format this one doesn't know: reading it
        // as an empty session would wipe every project and preset on the next
        // save, so it has to come back as an error.
        assert!(parse_state(r#"{"projects": "not a list"}"#).is_err());
        assert!(parse_state("").is_err());
        // Truncated by a crash mid-write.
        assert!(parse_state(r#"{"projects": [{"path": "/repo""#).is_err());

        // Unknown fields from a newer version are still readable, though —
        // only what serde genuinely can't map is fatal.
        let state = parse_state(r#"{"projects": [], "future_field": 7}"#).unwrap();
        assert!(state.projects.is_empty());
    }

    /// A private directory for one test. The state file is addressed by path
    /// here rather than through `HARMONIUM_DATA_DIR`, so these tests don't race
    /// the other modules' tests over one process-wide variable.
    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harmonium-state-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state_with_project(path: &str) -> AppState {
        AppState {
            projects: vec![ProjectRecord::new(PathBuf::from(path))],
            ..AppState::default()
        }
    }

    /// Read a state file back without taking the lock the live handle holds.
    fn reload(path: &Path) -> AppState {
        read_at(path).unwrap().unwrap()
    }

    #[test]
    fn saving_replaces_the_file_it_last_wrote() {
        let file = temp_dir("save").join("nested/state.json");
        let (mut handle, state) = StateFile::load_from(file.clone()).unwrap();
        // Nothing there yet: the parent directory is created on the way.
        assert!(state.projects.is_empty());
        handle.save(&state_with_project("/one")).unwrap();
        handle.save(&state_with_project("/two")).unwrap();

        assert_eq!(reload(&file).projects[0].path, PathBuf::from("/two"));
        // Every write goes through a temporary file, and none is left behind.
        let strays: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "state.json" && name != LOCK_FILE_NAME)
            .collect();
        assert!(strays.is_empty(), "left behind {strays:?}");
    }

    #[test]
    fn a_second_harmonium_does_not_get_the_session() {
        let file = temp_dir("lock").join("state.json");
        let (first, _) = StateFile::load_from(file.clone()).unwrap();
        match StateFile::load_from(file.clone()) {
            Err(LoadError::Locked { path }) => {
                assert_eq!(path, file.with_file_name(LOCK_FILE_NAME))
            }
            Err(error) => panic!("expected the session to be locked, got {error}"),
            Ok(_) => panic!("expected the session to be locked"),
        }
        // Looking without claiming is always allowed — `harmonium plan` does it
        // while the app is running.
        assert!(read_at(&file).unwrap().is_none());

        // The lock goes with the handle, so quitting hands the session on.
        drop(first);
        assert!(StateFile::load_from(file).is_ok());
    }

    #[test]
    fn a_foreign_write_is_parked_not_overwritten() {
        let dir = temp_dir("conflict");
        let file = dir.join("state.json");
        let (mut handle, _) = StateFile::load_from(file.clone()).unwrap();
        handle.save(&state_with_project("/mine")).unwrap();

        // Somebody edits the file behind harmonium's back — the jq splice, an
        // editor, a second harmonium.
        let theirs = r#"{"projects": [{"path": "/theirs", "name": "theirs", "agents": []}]}"#;
        std::fs::write(&file, theirs).unwrap();

        let rejected = match handle.save(&state_with_project("/mine")) {
            Err(SaveError::Conflict { rejected, .. }) => rejected.unwrap(),
            other => panic!("expected a conflict, got {other:?}"),
        };
        // Neither side is lost: their file stands, ours is next to it.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
        assert_eq!(rejected, dir.join(REJECTED_FILE_NAME));
        assert_eq!(reload(&rejected).projects[0].path, PathBuf::from("/mine"));

        // And it stays refused for the rest of the session — the later save is
        // the one that ends up parked, not an extra file.
        assert!(matches!(
            handle.save(&state_with_project("/mine-again")),
            Err(SaveError::Conflict { .. })
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
        assert_eq!(
            reload(&rejected).projects[0].path,
            PathBuf::from("/mine-again")
        );
    }

    #[test]
    fn a_state_file_that_is_only_read_still_counts_as_ours() {
        let file = temp_dir("reload").join("state.json");
        std::fs::write(
            &file,
            r#"{"projects": [{"path": "/repo", "name": "repo", "agents": []}]}"#,
        )
        .unwrap();
        // Loaded, then saved without anyone else touching it: no conflict,
        // even though what harmonium writes back isn't byte-identical to what
        // it read (migrations, formatting).
        let (mut handle, state) = StateFile::load_from(file).unwrap();
        handle.save(&state).unwrap();
        handle.save(&state).unwrap();
    }

    #[test]
    fn a_deleted_state_file_is_recreated() {
        let file = temp_dir("deleted").join("state.json");
        let (mut handle, _) = StateFile::load_from(file.clone()).unwrap();
        handle.save(&state_with_project("/one")).unwrap();
        // Moving it aside is what harmonium tells people to do; there is
        // nothing left to clobber, so this is not a conflict.
        std::fs::remove_file(&file).unwrap();
        handle.save(&state_with_project("/two")).unwrap();
        assert_eq!(reload(&file).projects[0].path, PathBuf::from("/two"));
    }

    /// The floor under every test in this crate: whatever `HARMONIUM_DATA_DIR`
    /// happens to be — unset, or pointed somewhere by another module's test —
    /// a test process must not resolve to the directory a real harmonium runs
    /// out of, where it would take that session's lock, save over its state
    /// file, or delete worktrees agents are working in.
    #[test]
    fn a_test_never_lands_on_the_real_data_dir() {
        let real = std::env::var("XDG_DATA_HOME")
            .map(|xdg| PathBuf::from(xdg).join("harmonium"))
            .or_else(|_| {
                std::env::var("HOME").map(|home| PathBuf::from(home).join(".local/share/harmonium"))
            });
        if let Ok(real) = real {
            assert_ne!(
                data_dir(),
                real,
                "a test would be working in the running harmonium's data directory"
            );
        }
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
