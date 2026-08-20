//! Root view: project/agent sidebar on the left, terminal pane on the right,
//! modal dialogs for adding projects and spawning agents.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, svg, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window,
};

use crate::input::{InputEvent, TextInput};
use crate::planner;
use crate::state::{
    load_state, save_state, AgentId, AgentRecord, AppState, ProjectRecord, TerminalTabRecord,
};
use crate::state;
use crate::terminal::view::TerminalView;
use crate::terminal::{Terminal, TerminalEvent};
use crate::theme;

struct PresetInputs {
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    resume_command: Entity<TextInput>,
}

enum Dialog {
    NewAgent {
        project_path: PathBuf,
        input: Entity<TextInput>,
        planning: bool,
        preset: usize,
        error: Option<String>,
        _subscription: Subscription,
    },
    Settings {
        /// Focus target for the panel itself, so the modal always owns the
        /// keyboard even when it has no input to focus (no presets yet).
        focus_handle: FocusHandle,
        planner_command: Entity<TextInput>,
        planner_model: Entity<TextInput>,
        terminal_font: Entity<TextInput>,
        ui_font: Entity<TextInput>,
        preset_inputs: Vec<PresetInputs>,
        _subscriptions: Vec<Subscription>,
    },
}

const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH: f32 = 600.;

/// A resolved command line for a terminal: the program to exec, its
/// arguments, and any `KEY=value` prefixes that came with it.
struct Spawn {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

/// Inline editing of an agent's name in the sidebar.
struct InlineEdit {
    id: AgentId,
    input: Entity<TextInput>,
    _subscription: Subscription,
}

pub struct Workspace {
    state: AppState,
    /// Live terminals, keyed by terminal id: the agent's own terminal uses
    /// the agent id, extra shell tabs use their tab id.
    terminals: HashMap<String, Entity<TerminalView>>,
    terminal_subscriptions: HashMap<String, Subscription>,
    /// Per agent: terminal id of the currently shown tab (defaults to the
    /// agent id, i.e. the first tab).
    active_tabs: HashMap<AgentId, String>,
    /// Agent terminals that have been superseded by a resume: shut down, but
    /// still rendered until the replacement emits its first output so the
    /// previous screen doesn't flash blank. Presence here *is* the "not yet
    /// painted" state — an entry is removed the moment the new terminal for
    /// the same id emits an event. Only agent terminals appear here; shell
    /// tabs are restarted from their history file instead, because a live and
    /// an outgoing terminal would otherwise fight over one history file.
    outgoing_terminals: HashMap<String, Entity<TerminalView>>,
    /// Agents whose saved terminals have been spawned in this session.
    /// Restoration is lazy: nothing is spawned until an agent is selected.
    restored: HashSet<AgentId>,
    selected: Option<AgentId>,
    dialog: Option<Dialog>,
    inline_edit: Option<InlineEdit>,
    resizing_sidebar: bool,
    status: Option<(String, bool)>,
    focus_handle: FocusHandle,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = load_state();
        cx.set_global(theme::FontSettings {
            base: state.settings.font_size,
        });
        theme::set_mode(state.settings.theme);
        theme::set_fonts(&state.settings.ui_font, &state.settings.terminal_font);
        let workspace = Self {
            state,
            terminals: HashMap::new(),
            terminal_subscriptions: HashMap::new(),
            active_tabs: HashMap::new(),
            outgoing_terminals: HashMap::new(),
            restored: HashSet::new(),
            selected: None,
            dialog: None,
            inline_edit: None,
            resizing_sidebar: false,
            status: None,
            focus_handle: cx.focus_handle(),
        };
        cx.on_app_quit(|this, cx| {
            this.persist(cx);
            std::future::ready(())
        })
        .detach();
        workspace
    }

    /// Write scrollback history for every live terminal and persist state.
    /// Called from the window's should-close hook, which is the only signal
    /// delivered on a window-manager close (i3 `kill`) before teardown.
    pub fn save_session(&mut self, cx: &mut Context<Self>) {
        for view in self.terminals.values() {
            view.read(cx).terminal.read(cx).save_history();
        }
        self.persist(cx);
    }

    /// Respawn the live processes for a saved agent: the agent session with
    /// its stored resume command, plus one shell per saved tab replaying that
    /// tab's history. Done lazily on first selection rather than for every
    /// saved agent at startup, which would mean dozens of PTYs before the
    /// first frame. A no-op once the agent has been restored this session.
    fn restore_agent(&mut self, id: &AgentId, cx: &mut Context<Self>) {
        if self.restored.contains(id) {
            return;
        }
        let Some(record) = self.state.agent(id).cloned() else {
            return;
        };
        self.restored.insert(id.clone());
        let resume = record
            .resume_command
            .clone()
            .unwrap_or_else(|| "claude --continue".into());
        if !self.terminals.contains_key(id) {
            self.start_agent_terminal(id, resume, Vec::new(), record.workdir.clone(), cx);
        }
        for tab in &record.terminals {
            // Guard against restoring a tab that was just spawned by hand.
            if !self.terminals.contains_key(&tab.id) {
                self.start_shell_tab(&tab.id, &record.workdir, cx);
            }
        }
        self.active_tabs
            .entry(id.clone())
            .or_insert_with(|| id.clone());
    }

    /// Spawn a shell tab, replaying any saved history file into the PTY first.
    fn start_shell_tab(&mut self, id: &str, workdir: &Path, cx: &mut Context<Self>) {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let history_path = state::terminal_history_path(id);
        let (program, args) = if history_path.exists() {
            let quote = |s: &str| {
                shlex::try_quote(s)
                    .map(|q| q.into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            let path = quote(&history_path.to_string_lossy());
            let shell = quote(&shell);
            // `;` rather than `&&`: a `cat` that fails or is interrupted must
            // not take the whole tab down with it — always exec the shell.
            (
                "/bin/sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!("cat {path} 2>/dev/null; exec {shell}"),
                ],
            )
        } else {
            // Passed as the program, never word-split: $SHELL is a path, not
            // a command line.
            (shell, Vec::new())
        };
        let spawn = Spawn {
            program,
            args,
            env: Vec::new(),
        };
        self.start_terminal(id, spawn, workdir.to_path_buf(), true, cx);
    }

    fn set_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let new_size = (theme::base_font_size(cx) + delta)
            .clamp(theme::MIN_FONT_SIZE, theme::MAX_FONT_SIZE);
        cx.set_global(theme::FontSettings { base: new_size });
        self.state.settings.font_size = new_size;
        self.persist(cx);
        // Terminals re-measure their cell size on next paint; wake them up.
        let views: Vec<_> = self.terminals.values().cloned().collect();
        for view in views {
            view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    fn set_theme(&mut self, mode: theme::ThemeMode, cx: &mut Context<Self>) {
        theme::set_mode(mode);
        self.state.settings.theme = mode;
        self.persist(cx);
        let views: Vec<_> = self.terminals.values().cloned().collect();
        for view in views {
            view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = save_state(&self.state) {
            self.set_status(format!("Failed to save state: {error}"), true, cx);
        }
    }

    fn set_status(&mut self, message: String, is_error: bool, cx: &mut Context<Self>) {
        self.status = Some((message, is_error));
        cx.notify();
    }

    // ---- Projects ----

    fn add_project_via_picker(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None, // user cancelled
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.set_status(format!("File picker failed: {error}"), true, cx);
                    })
                    .ok();
                    None
                }
                Err(_) => None,
            };
            if let Some(path) = picked {
                this.update(cx, |this, cx| {
                    this.add_project(path.to_string_lossy().into_owned(), cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn add_project(&mut self, path: String, cx: &mut Context<Self>) {
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(shellexpand_home(&path));
        if !path.is_dir() {
            self.set_status(format!("Not a directory: {}", path.display()), true, cx);
            return;
        }
        if !planner::is_git_repo(&path) {
            self.set_status(
                format!("Not a git repository: {}", path.display()),
                true,
                cx,
            );
            return;
        }
        if self.state.projects.iter().any(|p| p.path == path) {
            self.set_status("Project already added".into(), true, cx);
            return;
        }
        self.state.projects.push(ProjectRecord::new(path));
        self.dialog = None;
        self.status = None;
        self.persist(cx);
        cx.notify();
    }

    fn remove_project(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.state.projects.len() {
            return;
        }
        let project = self.state.projects.remove(index);
        for agent in &project.agents {
            let tab_ids: Vec<String> =
                agent.terminals.iter().map(|t| t.id.clone()).collect();
            self.teardown_agent(&agent.id, &tab_ids, cx);
        }
        self.persist(cx);
        cx.notify();
    }

    // ---- Agents ----

    fn open_new_agent_dialog(
        &mut self,
        project_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            TextInput::multiline("Describe the task… (ctrl-enter to spawn)", cx)
        });
        let path = project_path.clone();
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::Submitted(task) => {
                    let task = task.trim().to_string();
                    if !task.is_empty() {
                        this.spawn_agent(path.clone(), task, window, cx);
                    }
                }
                InputEvent::Cancelled => this.close_dialog(window, cx),
            },
        );
        window.focus(&input.focus_handle(cx));
        let preset = self
            .state
            .settings
            .last_preset
            .min(self.state.settings.presets.len().saturating_sub(1));
        self.dialog = Some(Dialog::NewAgent {
            project_path,
            input,
            planning: false,
            preset,
            error: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    // ---- Settings ----

    /// One text field in the settings dialog, wired to the same save/cancel
    /// handling as the preset fields.
    fn make_setting_input(
        &mut self,
        placeholder: &'static str,
        value: &str,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let input = cx.new(|cx| {
            let mut input = TextInput::new(placeholder, cx);
            if !value.is_empty() {
                input.set_text(value.to_string(), cx);
            }
            input
        });
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::Submitted(_) => this.save_settings(window, cx),
                InputEvent::Cancelled => this.close_dialog(window, cx),
            },
        ));
        input
    }

    fn make_preset_inputs(
        &mut self,
        name: &str,
        command: &str,
        resume_command: &str,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PresetInputs {
        let mut make = |placeholder: &'static str, value: &str| {
            let input = cx.new(|cx| {
                let mut input = TextInput::new(placeholder, cx);
                if !value.is_empty() {
                    input.set_text(value.to_string(), cx);
                }
                input
            });
            // `subscribe_in` rather than `subscribe`: closing the dialog has
            // to hand the keyboard back to the terminal, which needs a window.
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::Submitted(_) => this.save_settings(window, cx),
                    InputEvent::Cancelled => this.close_dialog(window, cx),
                },
            ));
            input
        };
        PresetInputs {
            name: make("Preset name", name),
            command: make("Command (task appended)", command),
            resume_command: make("Resume command", resume_command),
        }
    }

    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let presets = self.state.settings.presets.clone();
        let mut subscriptions = Vec::new();
        let preset_inputs: Vec<PresetInputs> = presets
            .iter()
            .map(|p| {
                self.make_preset_inputs(
                    &p.name,
                    &p.command,
                    &p.resume_command,
                    &mut subscriptions,
                    window,
                    cx,
                )
            })
            .collect();
        // The dialog is modal, so it has to take the keyboard: without this
        // the terminal keeps focus and typing goes into the PTY behind the
        // dialog. Prefer the first field; fall back to the panel itself when
        // there are no presets to focus.
        let settings = self.state.settings.clone();
        let planner_command = self.make_setting_input(
            "Full command; leave empty to use the model below",
            &settings.planner_command,
            &mut subscriptions,
            window,
            cx,
        );
        let planner_model = self.make_setting_input(
            planner::DEFAULT_MODEL,
            &settings.planner_model,
            &mut subscriptions,
            window,
            cx,
        );
        let terminal_font = self.make_setting_input(
            theme::DEFAULT_TERMINAL_FONT,
            &settings.terminal_font,
            &mut subscriptions,
            window,
            cx,
        );
        let ui_font = self.make_setting_input(
            theme::DEFAULT_UI_FONT,
            &settings.ui_font,
            &mut subscriptions,
            window,
            cx,
        );
        let focus_handle = cx.focus_handle();
        match preset_inputs.first() {
            Some(first) => window.focus(&first.name.focus_handle(cx)),
            None => window.focus(&focus_handle),
        }
        self.dialog = Some(Dialog::Settings {
            focus_handle,
            planner_command,
            planner_model,
            terminal_font,
            ui_font,
            preset_inputs,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn add_preset_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let inputs = self.make_preset_inputs("", "", "", &mut subscriptions, window, cx);
        // Typing should land in the row that was just added.
        window.focus(&inputs.name.focus_handle(cx));
        if let Some(Dialog::Settings {
            preset_inputs,
            _subscriptions,
            ..
        }) = &mut self.dialog
        {
            preset_inputs.push(inputs);
            _subscriptions.extend(subscriptions);
        }
        cx.notify();
    }

    fn remove_preset_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(Dialog::Settings { preset_inputs, .. }) = &mut self.dialog {
            if index < preset_inputs.len() {
                preset_inputs.remove(index);
            }
        }
        cx.notify();
    }

    /// Dismiss the open dialog and hand the keyboard back to the selected
    /// agent's terminal — otherwise focus dies with the dialog's inputs and
    /// typing goes nowhere until the terminal is clicked.
    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = None;
        if let Some(id) = self.selected.clone() {
            if let Some(view) = self.active_terminal(&id) {
                let handle = view.focus_handle(cx);
                window.focus(&handle);
            }
        }
        cx.notify();
    }

    fn save_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Settings {
            planner_command,
            planner_model,
            terminal_font,
            ui_font,
            preset_inputs,
            ..
        }) = &self.dialog
        else {
            return;
        };
        let text = |input: &Entity<TextInput>| input.read(cx).text().trim().to_string();
        let planner_command = text(planner_command);
        let planner_model = text(planner_model);
        let terminal_font = text(terminal_font);
        let ui_font = text(ui_font);
        let presets: Vec<crate::state::PresetRecord> = preset_inputs
            .iter()
            .filter_map(|inputs| {
                let name = inputs.name.read(cx).text().trim().to_string();
                let command = inputs.command.read(cx).text().trim().to_string();
                let resume_command =
                    inputs.resume_command.read(cx).text().trim().to_string();
                if name.is_empty() && command.is_empty() {
                    return None;
                }
                Some(crate::state::PresetRecord {
                    name: if name.is_empty() { command.clone() } else { name },
                    command,
                    resume_command,
                })
            })
            .collect();
        self.state.settings.presets = presets;
        self.state.settings.last_preset = self
            .state
            .settings
            .last_preset
            .min(self.state.settings.presets.len().saturating_sub(1));
        self.state.settings.planner_command = planner_command;
        self.state.settings.planner_model = planner_model;
        self.state.settings.terminal_font = terminal_font;
        self.state.settings.ui_font = ui_font;
        self.apply_fonts(cx);
        self.close_dialog(window, cx);
        self.persist(cx);
        cx.notify();
    }

    /// Push the configured font families into the theme and wake every
    /// terminal so it re-measures its cell size, like a font-size change.
    fn apply_fonts(&mut self, cx: &mut Context<Self>) {
        theme::set_fonts(
            &self.state.settings.ui_font,
            &self.state.settings.terminal_font,
        );
        let views: Vec<_> = self.terminals.values().cloned().collect();
        for view in views {
            view.update(cx, |_, cx| cx.notify());
        }
    }

    fn spawn_agent(
        &mut self,
        project_path: PathBuf,
        task: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preset_index;
        if let Some(Dialog::NewAgent {
            planning,
            preset,
            error,
            ..
        }) = &mut self.dialog
        {
            if *planning {
                return;
            }
            *planning = true;
            *error = None;
            preset_index = *preset;
        } else {
            preset_index = self.state.settings.last_preset;
        }
        let (command, resume_command) = self
            .state
            .settings
            .presets
            .get(preset_index)
            .map(|p| (p.command.clone(), p.resume_command.clone()))
            .unwrap_or_else(|| ("claude".into(), "claude --continue".into()));
        self.state.settings.last_preset = preset_index;
        self.persist(cx);
        cx.notify();

        let repo = project_path.clone();
        let planner_settings = planner::PlannerSettings {
            command: self.state.settings.planner_command.clone(),
            model: self.state.settings.planner_model.clone(),
        };
        cx.spawn_in(window, async move |this, cx| {
            let repo_for_bg = repo.clone();
            let task_for_bg = task.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    // No fallback: if the planner fails (e.g. usage limit
                    // reached), report it instead of inventing a branch.
                    let plan = planner::plan_task(&repo_for_bg, &task_for_bg, &planner_settings)?;
                    planner::resolve_workspace(&repo_for_bg, &plan, &task_for_bg)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(workspace) => {
                        this.dialog = None;
                        this.finish_spawn(
                            repo,
                            task,
                            workspace,
                            command,
                            resume_command,
                            window,
                            cx,
                        );
                    }
                    Err(spawn_error) => {
                        // Keep the dialog open so the task text isn't lost;
                        // show the error inline for a retry.
                        let message = format!("{spawn_error:#}");
                        if let Some(Dialog::NewAgent {
                            planning, error, ..
                        }) = &mut this.dialog
                        {
                            *planning = false;
                            *error = Some(message);
                        } else {
                            this.set_status(message, true, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_spawn(
        &mut self,
        project_path: PathBuf,
        task: String,
        workspace: planner::Workspace,
        command: String,
        resume_command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let record = AgentRecord {
            id: id.clone(),
            name: workspace.agent_name.clone(),
            description: task.clone(),
            workdir: workspace.workdir.clone(),
            branch: workspace.branch.clone(),
            command: Some(command.clone()),
            resume_command: Some(resume_command),
            terminals: Vec::new(),
        };
        let Some(project) = self
            .state
            .projects
            .iter_mut()
            .find(|p| p.path == project_path)
        else {
            self.set_status("Project disappeared while planning".into(), true, cx);
            return;
        };
        project.agents.push(record);
        project.expanded = true;
        self.persist(cx);

        self.start_agent_terminal(&id, command, vec![task], workspace.workdir, cx);
        // A brand-new agent has nothing saved to restore.
        self.restored.insert(id.clone());
        self.active_tabs.insert(id.clone(), id.clone());
        self.status = None;
        self.select_agent(id, window, cx);
    }

    /// Spawn a fresh shell in the agent's workdir for a new terminal tab.
    fn add_terminal_tab(
        &mut self,
        agent_id: AgentId,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self.state.agent(&agent_id).cloned() else {
            return;
        };
        let tab_id = uuid::Uuid::new_v4().to_string();
        self.start_shell_tab(&tab_id, &record.workdir, cx);
        if let Some(record) = self.state.agent_mut(&agent_id) {
            record.terminals.push(TerminalTabRecord {
                id: tab_id.clone(),
                name: name.clone(),
            });
        }
        self.active_tabs.insert(agent_id.clone(), tab_id.clone());
        self.select_agent(agent_id, window, cx);
        self.persist(cx);
    }

    /// Remove a terminal tab. Refuses to remove the agent's own first tab.
    fn remove_terminal_tab(
        &mut self,
        agent_id: AgentId,
        tab_id: String,
        cx: &mut Context<Self>,
    ) {
        if tab_id == agent_id {
            return;
        }
        if let Some(view) = self.terminals.remove(&tab_id) {
            let terminal = view.read(cx).terminal.clone();
            terminal.update(cx, |terminal, _| terminal.discard_history());
        }
        self.terminal_subscriptions.remove(&tab_id);
        if let Some(record) = self.state.agent_mut(&agent_id) {
            record.terminals.retain(|t| t.id != tab_id);
        }
        if self
            .active_tabs
            .get(&agent_id)
            .map(|a| a == &tab_id)
            .unwrap_or(false)
        {
            self.active_tabs.insert(agent_id.clone(), agent_id.clone());
        }
        self.persist(cx);
        cx.notify();
    }

    /// Spawn an agent session terminal. Agent commands — and only agent
    /// commands — are configured as a command *line*, so they are word-split
    /// here and can be replaced wholesale by the test/debug override.
    fn start_agent_terminal(
        &mut self,
        id: &str,
        command: String,
        extra_args: Vec<String>,
        workdir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // Test/debug override: replaces the preset command entirely. Scoped
        // to agent terminals so it can't hijack shell tabs.
        let command = std::env::var("HARMONIUM_AGENT_BIN").unwrap_or(command);
        let mut parts = planner::split_command(&command);
        // Leading `KEY=value` words are environment for the child, as in a
        // shell — e.g. `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 claude`, which
        // keeps the agent's output in the scrollback where it can be scrolled
        // and selected.
        let mut env = Vec::new();
        while let Some(assignment) = parts.first().and_then(|word| split_assignment(word)) {
            env.push(assignment);
            parts.remove(0);
        }
        if parts.is_empty() {
            self.set_status(format!("Empty agent command: `{command}`"), true, cx);
            return;
        }
        let program = parts.remove(0);
        parts.extend(extra_args);
        let spawn = Spawn {
            program,
            args: parts,
            env,
        };
        self.start_terminal(id, spawn, workdir, false, cx);
    }

    /// Spawn `program args...` — already resolved, never re-split — and wire
    /// up the view and event subscription for terminal `id`.
    fn start_terminal(
        &mut self,
        id: &str,
        spawn: Spawn,
        workdir: PathBuf,
        persist_history: bool,
        cx: &mut Context<Self>,
    ) {
        match Terminal::create(
            id.to_string(),
            spawn.program,
            spawn.args,
            spawn.env,
            workdir,
            persist_history,
            cx,
        ) {
            Ok(terminal) => {
                let terminal_id = id.to_string();
                let subscription = cx.subscribe(
                    &terminal,
                    move |this, _terminal, event: &TerminalEvent, cx| {
                        // Any event means this terminal has produced output,
                        // which retires the outgoing snapshot for its id.
                        this.terminal_painted(&terminal_id, cx);
                        // Plain output already redraws the terminal view
                        // itself; only title/exit change what the workspace
                        // chrome renders.
                        if !matches!(event, TerminalEvent::Output) {
                            cx.notify();
                        }
                    },
                );
                let view = cx.new(|cx| TerminalView::new(terminal, cx));
                self.terminals.insert(id.to_string(), view);
                self.terminal_subscriptions
                    .insert(id.to_string(), subscription);
            }
            Err(error) => {
                self.set_status(format!("Failed to spawn terminal: {error}"), true, cx);
            }
        }
    }

    fn resume_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(record) = self.state.agent(&id).cloned() else {
            return;
        };

        // Keep the agent terminal around as a snapshot so the previous output
        // stays visible until the resumed process draws its first frame — but
        // shut the superseded process down rather than letting it run on
        // invisibly, and drop its subscription so only the *new* terminal's
        // events can retire the snapshot.
        if let Some(view) = self.terminals.remove(&id) {
            self.terminal_subscriptions.remove(&id);
            let terminal = view.read(cx).terminal.clone();
            terminal.update(cx, |terminal, _| {
                terminal.forget_history();
                terminal.shutdown();
            });
            self.outgoing_terminals.insert(id.clone(), view);
        }
        // Shell tabs are *not* snapshotted: live and outgoing would share one
        // history file, so the respawned tab would replay stale content and
        // the outgoing terminal's drop would later clobber the fresh file.
        // Instead, save each tab's scrollback now, shut it down and drop it;
        // `start_shell_tab` below replays what was just written.
        for tab in &record.terminals {
            if let Some(view) = self.terminals.remove(&tab.id) {
                self.terminal_subscriptions.remove(&tab.id);
                let terminal = view.read(cx).terminal.clone();
                terminal.update(cx, |terminal, _| {
                    terminal.save_history_now();
                    terminal.shutdown();
                });
            }
        }

        let workdir = record.workdir.clone();
        let resume = record
            .resume_command
            .clone()
            .unwrap_or_else(|| "claude --continue".into());
        self.start_agent_terminal(&id, resume, Vec::new(), workdir.clone(), cx);
        if !self.terminals.contains_key(&id) {
            // The respawn failed; without the snapshot dropped, the "No live
            // session / Resume" UI would be unreachable for a retry.
            self.outgoing_terminals.remove(&id);
        }
        // Restart persisted extra tabs, replaying the history just written.
        for tab in &record.terminals {
            self.start_shell_tab(&tab.id, &workdir, cx);
        }
        self.restored.insert(id.clone());
        self.active_tabs.insert(id.clone(), id.clone());
        cx.notify();
    }

    /// Called when a live terminal emits any event: its first output retires
    /// the outgoing snapshot shown in its place.
    fn terminal_painted(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.outgoing_terminals.remove(id).is_some() {
            cx.notify();
        }
    }

    /// Shut down and forget every terminal belonging to an agent, along with
    /// its per-agent UI state. History files are deleted: this is the "gone
    /// for good" path, used when an agent or its whole project is removed.
    fn teardown_agent(
        &mut self,
        id: &AgentId,
        tab_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        for terminal_id in std::iter::once(id.clone()).chain(tab_ids.iter().cloned()) {
            self.terminal_subscriptions.remove(&terminal_id);
            let views = [
                self.terminals.remove(&terminal_id),
                self.outgoing_terminals.remove(&terminal_id),
            ];
            for view in views.into_iter().flatten() {
                let terminal = view.read(cx).terminal.clone();
                terminal.update(cx, |terminal, _| terminal.discard_history());
            }
        }
        self.active_tabs.remove(id);
        self.restored.remove(id);
        if self.selected.as_ref() == Some(id) {
            self.selected = None;
        }
    }

    fn remove_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let tab_ids: Vec<String> = self
            .state
            .agent(&id)
            .map(|record| record.terminals.iter().map(|t| t.id.clone()).collect())
            .unwrap_or_default();
        self.teardown_agent(&id, &tab_ids, cx);
        for project in &mut self.state.projects {
            project.agents.retain(|a| a.id != id);
        }
        self.inline_edit = None;
        self.persist(cx);
        cx.notify();
    }

    fn active_terminal(&self, agent_id: &str) -> Option<&Entity<TerminalView>> {
        let tab_id = self.active_tabs.get(agent_id).cloned().unwrap_or(agent_id.to_string());
        self.terminals.get(&tab_id)
    }

    fn select_agent(&mut self, id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        self.inline_edit = None;
        // Saved agents get their processes spawned the first time they are
        // looked at, not all at once during startup.
        self.restore_agent(&id, cx);
        if let Some(view) = self.active_terminal(&id) {
            let handle = view.focus_handle(cx);
            window.focus(&handle);
        }
        cx.notify();
    }

    fn select_tab(
        &mut self,
        agent_id: AgentId,
        tab_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_tabs.insert(agent_id.clone(), tab_id);
        if let Some(view) = self.active_terminal(&agent_id) {
            let handle = view.focus_handle(cx);
            window.focus(&handle);
        }
        cx.notify();
    }

    fn start_inline_edit(&mut self, id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.state.agent(&id) else {
            return;
        };
        let name = record.name.clone();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Agent name", cx);
            input.set_text(name, cx);
            input
        });
        let id_for_sub = id.clone();
        let subscription = cx.subscribe(
            &input,
            move |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Submitted(text) => {
                    if let Some(record) = this.state.agent_mut(&id_for_sub) {
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            record.name = text;
                        }
                    }
                    this.inline_edit = None;
                    this.persist(cx);
                    cx.notify();
                }
                InputEvent::Cancelled => {
                    this.inline_edit = None;
                    cx.notify();
                }
            },
        );
        window.focus(&input.focus_handle(cx));
        self.inline_edit = Some(InlineEdit {
            id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    // ---- Render helpers ----

    fn render_sidebar(&self, cx: &Context<Self>) -> gpui::AnyElement {
        // Collapsed: a narrow strip with just an expand button.
        if self.state.settings.sidebar_collapsed {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .w(px(28.))
                .h_full()
                .flex_none()
                .bg(theme::panel_bg())
                .border_r_1()
                .border_color(theme::border())
                .child(div().flex_1())
                .child(
                    div()
                        .id("expand-sidebar")
                        .mb_2()
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(theme::fg_dim())
                        .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.settings.sidebar_collapsed = false;
                            this.persist(cx);
                            cx.notify();
                        }))
                        .child("▸"),
                )
                .into_any_element();
        }

        let mut list = div().flex().flex_col().gap_0p5().p_2();

        if self.state.projects.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_color(theme::fg_dim())
                    .text_sm()
                    .child("No projects yet — pick a git repository below."),
            );
        }

        for (project_index, project) in self.state.projects.iter().enumerate() {
            let expanded = project.expanded;
            let project_path = project.path.clone();

            list = list.child(
                div()
                    .id(("project", project_index))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this.state.projects.get_mut(project_index) {
                            p.expanded = !p.expanded;
                        }
                        this.persist(cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_color(theme::fg_dim())
                            .text_sm()
                            .w_4()
                            .child(if expanded { "▾" } else { "▸" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_color(theme::fg())
                                    .text_sm()
                                    .whitespace_nowrap()
                                    .child(SharedString::from(project.name.clone())),
                            ),
                    )
                    .child(
                        div()
                            .id(("new-agent", project_index))
                            .px_1()
                            .rounded_sm()
                            .text_color(theme::accent())
                            .hover(|s| s.bg(theme::selected_bg()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_new_agent_dialog(project_path.clone(), window, cx);
                            }))
                            .child("+"),
                    )
                    .child(
                        div()
                            .id(("remove-project", project_index))
                            .px_1()
                            .rounded_sm()
                            .text_color(theme::fg_dim())
                            .hover(|s| s.bg(theme::selected_bg()).text_color(theme::error()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_project(project_index, cx);
                            }))
                            .child("×"),
                    ),
            );

            if expanded {
                if project.agents.is_empty() {
                    list = list.child(
                        div()
                            .pl_8()
                            .py_0p5()
                            .text_color(theme::fg_dim())
                            .text_xs()
                            .child("no agents"),
                    );
                }
                for (agent_index, agent) in project.agents.iter().enumerate() {
                    // Inline description editing initiated from the sidebar
                    // replaces the whole row with the input.
                    if let Some(edit) = self
                        .inline_edit
                        .as_ref()
                        .filter(|edit| edit.id == agent.id)
                    {
                        list = list.child(
                            div()
                                .ml_4()
                                .my_0p5()
                                .child(edit.input.clone()),
                        );
                        continue;
                    }

                    let id = agent.id.clone();
                    let dbl_id = agent.id.clone();
                    let is_selected = self.selected.as_ref() == Some(&agent.id);
                    let dot_color = match self.terminals.get(&agent.id) {
                        Some(view) => {
                            if view.read(cx).terminal.read(cx).exited {
                                theme::error()
                            } else {
                                theme::ok()
                            }
                        }
                        None => theme::fg_dim(),
                    };
                    let remove_id = agent.id.clone();
                    let edit_id = agent.id.clone();

                    list = list.child(
                        div()
                            .id(("agent", project_index * 1000 + agent_index))
                            .flex()
                            .items_center()
                            .gap_2()
                            .ml_4()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(is_selected, |s| s.bg(theme::selected_bg()))
                            .hover(|s| s.bg(theme::hover_bg()))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                if event.click_count() >= 2 {
                                    this.start_inline_edit(dbl_id.clone(), window, cx);
                                } else {
                                    this.select_agent(id.clone(), window, cx);
                                }
                            }))
                            .child(div().text_color(dot_color).text_xs().child("●"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    // Name only: the branch used to be shown
                                    // on a second line, which doubled the row
                                    // height for information the agent name
                                    // already implies.
                                    .child(
                                        div()
                                            .text_color(theme::fg())
                                            .text_sm()
                                            .whitespace_nowrap()
                                            .child(SharedString::from(agent.name.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("edit-agent", project_index * 1000 + agent_index))
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(theme::fg_dim())
                                    .hover(|s| {
                                        s.bg(theme::selected_bg()).text_color(theme::accent())
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.start_inline_edit(edit_id.clone(), window, cx);
                                    }))
                                    .child(
                                        svg()
                                            .path("icons/pencil.svg")
                                            .size_3()
                                            .text_color(theme::fg_dim()),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("remove-agent", project_index * 1000 + agent_index))
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(theme::fg_dim())
                                    .hover(|s| {
                                        s.bg(theme::selected_bg()).text_color(theme::error())
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.remove_agent(remove_id.clone(), cx);
                                    }))
                                    .child("×"),
                            ),
                    );
                }
            }
        }

        // Adding a project is part of the list rather than a separate button:
        // the row lines up with the project headers, with a `+` where their
        // expand/collapse triangle goes.
        list = list.child(
            div()
                .id("new-project")
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.add_project_via_picker(cx);
                }))
                .child(
                    div()
                        .text_color(theme::accent())
                        .text_sm()
                        .w_4()
                        .child("+"),
                )
                .child(
                    div().flex_1().overflow_hidden().child(
                        div()
                            .text_color(theme::fg_dim())
                            .text_sm()
                            .whitespace_nowrap()
                            .child("New project"),
                    ),
                ),
        );

        div()
            .flex()
            .flex_col()
            .w(px(self.state.settings.sidebar_width.clamp(
                MIN_SIDEBAR_WIDTH,
                MAX_SIDEBAR_WIDTH,
            )))
            .h_full()
            .flex_none()
            .bg(theme::panel_bg())
            .border_r_1()
            .border_color(theme::border())
            .child(div().flex_1().overflow_hidden().child(list))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_2()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("collapse-sidebar")
                                    .px_1()
                                    .py_0p5()
                                    .rounded_sm()
                                    .text_color(theme::fg_dim())
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.settings.sidebar_collapsed = true;
                                        this.persist(cx);
                                        cx.notify();
                                    }))
                                    .child("◂"),
                            )
                    )
                    .child(
                        div()
                            .id("open-settings")
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .text_color(theme::fg_dim())
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings_dialog(window, cx);
                            }))
                            .child("⚙"),
                    ),
            )
            .into_any_element()
    }

    fn render_tabs(
        &self,
        agent: &AgentRecord,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let active_id = self
            .active_tabs
            .get(&agent.id)
            .cloned()
            .unwrap_or_else(|| agent.id.clone());

        let tab =
            |id: &str,
             label: String,
             active: bool,
             closable: bool,
             agent_id: AgentId,
             on_click: Box<dyn Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static>,
             cx: &Context<Self>| {
                let tab_id: SharedString = format!("tab-{id}").into();
                let tab_id_for_close = id.to_string();
                let owner_id = agent_id.clone();
                let mut tab = div()
                    .id(tab_id)
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(theme::border())
                    .text_color(if active {
                        theme::accent()
                    } else {
                        theme::fg()
                    })
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(on_click))
                    .child(
                        div()
                            .max_w(px(140.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_sm()
                            .child(SharedString::from(label)),
                    );

                if closable {
                    tab = tab.child(
                        div()
                            .id(format!("close-{tab_id_for_close}"))
                            .ml_1()
                            .text_color(theme::fg_dim())
                            .hover(|s| s.text_color(theme::error()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_terminal_tab(owner_id.clone(), tab_id_for_close.clone(), cx);
                            }))
                            .child("×"),
                    );
                }

                tab.into_any_element()
            };

        let mut row = div()
            .flex()
            .flex_none()
            .items_center()
            .border_b_1()
            .border_color(theme::border());

        // Agent tab (always first).
        let agent_id = agent.id.clone();
        row = row.child(tab(
            &agent.id,
            "Agent".to_string(),
            active_id == agent.id,
            false,
            agent.id.clone(),
            Box::new(move |this, _: &ClickEvent, window, cx| {
                this.select_tab(agent_id.clone(), agent_id.clone(), window, cx);
            }),
            cx,
        ));

        // Extra terminal tabs.
        for tab_record in &agent.terminals {
            let tab_id = tab_record.id.clone();
            let owner_id = agent.id.clone();
            row = row.child(tab(
                &tab_record.id,
                tab_record.name.clone(),
                active_id == tab_record.id,
                true,
                agent.id.clone(),
                Box::new(move |this, _: &ClickEvent, window, cx| {
                    this.select_tab(owner_id.clone(), tab_id.clone(), window, cx);
                }),
                cx,
            ));
        }

        // Add new terminal tab.
        let add_agent_id = agent.id.clone();
        row = row.child(
            div()
                .id("add-terminal-tab")
                .px_2()
                .py_1()
                .cursor_pointer()
                .text_color(theme::accent())
                .hover(|s| s.bg(theme::hover_bg()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let next = this
                        .state
                        .agent(&add_agent_id)
                        .map(|r| r.terminals.len() + 1)
                        .unwrap_or(1);
                    this.add_terminal_tab(
                        add_agent_id.clone(),
                        format!("Shell {next}"),
                        window,
                        cx,
                    );
                }))
                .child("+"),
        );

        row.into_any_element()
    }

    fn render_main_pane(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = self
            .selected
            .as_ref()
            .and_then(|id| self.state.agent(id).cloned());

        let mut pane = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::bg());

        match selected {
            None => {
                pane = pane.child(
                    div()
                        .flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(theme::fg_dim())
                        .child("Select an agent, or spawn one with + next to a project."),
                );
            }
            Some(agent) => {
                let id = agent.id.clone();
                let resume_id = id.clone();
                pane = pane
                    .child(self.render_tabs(&agent, cx))
                    .child(div().h_px().bg(theme::border()));
                let active_tab_id = self
                    .active_tabs
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| id.clone());
                if let Some(outgoing) = self.outgoing_terminals.get(&active_tab_id) {
                    // A resume is in progress: show the superseded terminal's
                    // last screen. The live terminal is still laid out (so it
                    // gets a size) but invisible until its first output
                    // removes this entry.
                    let mut stack = div()
                        .flex_1()
                        .overflow_hidden()
                        .relative()
                        .child(outgoing.clone());
                    if let Some(view) = self.active_terminal(&id) {
                        stack = stack.child(
                            div()
                                .absolute()
                                .inset_0()
                                .overflow_hidden()
                                .invisible()
                                .child(view.clone()),
                        );
                    }
                    pane = pane.child(stack);
                } else if let Some(view) = self.active_terminal(&id) {
                    pane = pane.child(div().flex_1().overflow_hidden().child(view.clone()));
                } else {
                    pane = pane.child(
                        div()
                            .flex()
                            .flex_1()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(theme::fg_dim())
                                    .child("No live session for this agent."),
                            )
                            .child(
                                div()
                                    .id("resume-agent")
                                    .px_3()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(theme::selected_bg())
                                    .text_color(theme::accent())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.resume_agent(resume_id.clone(), cx);
                                    }))
                                    .child("Resume session (claude --continue)"),
                            ),
                    );
                }
            }
        }

        if let Some((message, is_error)) = &self.status {
            pane = pane.child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(if *is_error {
                        theme::error()
                    } else {
                        theme::fg_dim()
                    })
                    .child(SharedString::from(message.clone())),
            );
        }

        pane
    }

    fn dialog_buttons(&self, submit_label: &'static str, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .justify_end()
            .child(
                div()
                    .id("dialog-cancel")
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .text_color(theme::fg_dim())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_dialog(window, cx);
                    }))
                    .child("Cancel"),
            )
            .child(
                div()
                    .id("dialog-submit")
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .bg(theme::accent())
                    .text_color(theme::panel_bg())
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.submit_dialog(window, cx);
                    }))
                    .child(submit_label),
            )
    }

    fn render_font_size_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let base = theme::base_font_size(cx);
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme::fg_dim())
                    .text_sm()
                    .child("Base font size"),
            )
            .child(
                div()
                    .id("font-size-dec")
                    .px_2()
                    .rounded_sm()
                    .bg(theme::selected_bg())
                    .text_color(theme::fg())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_font_size(-1., cx);
                    }))
                    .child("-"),
            )
            .child(
                div()
                    .w_8()
                    .text_center()
                    .text_color(theme::fg())
                    .child(SharedString::from(format!("{base:.0}"))),
            )
            .child(
                div()
                    .id("font-size-inc")
                    .px_2()
                    .rounded_sm()
                    .bg(theme::selected_bg())
                    .text_color(theme::fg())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_font_size(1., cx);
                    }))
                    .child("+"),
            )
    }

    fn render_theme_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = theme::mode();
        let chip = |label: &'static str,
                    mode: theme::ThemeMode,
                    cx: &Context<Self>| {
            let selected = current == mode;
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_sm()
                .cursor_pointer()
                .bg(if selected {
                    theme::accent()
                } else {
                    theme::selected_bg()
                })
                .text_color(if selected {
                    theme::panel_bg()
                } else {
                    theme::fg()
                })
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_theme(mode, cx);
                }))
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_color(theme::fg_dim()).text_sm().child("Theme"))
            .child(chip("Light", theme::ThemeMode::Light, cx))
            .child(chip("Dark", theme::ThemeMode::Dark, cx))
    }

    fn render_dialog(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.dialog.as_ref()?;

        let panel = match dialog {
            Dialog::NewAgent {
                input,
                planning,
                preset,
                error,
                ..
            } => {
                let mut panel = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(560.))
                    .p_4()
                    .rounded_sm()
                    .bg(theme::panel_bg())
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_color(theme::fg())
                            .child("New agent — describe the task"),
                    )
                    .child(input.clone());

                // Preset selector.
                let selected = *preset;
                let mut chips = div().flex().flex_wrap().items_center().gap_1().child(
                    div()
                        .text_color(theme::fg_dim())
                        .text_sm()
                        .child("Preset:"),
                );
                for (index, preset_record) in self.state.settings.presets.iter().enumerate() {
                    let is_selected = index == selected;
                    chips = chips.child(
                        div()
                            .id(("preset-chip", index))
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .text_sm()
                            .cursor_pointer()
                            .bg(if is_selected {
                                theme::accent()
                            } else {
                                theme::selected_bg()
                            })
                            .text_color(if is_selected {
                                theme::panel_bg()
                            } else {
                                theme::fg()
                            })
                            .hover(|s| s.opacity(0.85))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Dialog::NewAgent { preset, .. }) = &mut this.dialog {
                                    *preset = index;
                                }
                                cx.notify();
                            }))
                            .child(SharedString::from(preset_record.name.clone())),
                    );
                }
                panel = panel.child(chips);

                if let Some(error) = error {
                    panel = panel.child(
                        div()
                            .text_color(theme::error())
                            .text_sm()
                            .child(SharedString::from(error.clone())),
                    );
                }

                if *planning {
                    panel.child(
                        div()
                            .text_color(theme::warn())
                            .text_sm()
                            .child("Planning workspace with LLM…"),
                    )
                } else {
                    panel.child(self.dialog_buttons("Spawn", cx))
                }
                .into_any_element()
            }

            Dialog::Settings {
                focus_handle,
                planner_command,
                planner_model,
                terminal_font,
                ui_font,
                preset_inputs,
                ..
            } => {
                let section = |text: &'static str| {
                    div()
                        .text_color(theme::fg())
                        .text_sm()
                        .mt_2()
                        .child(text)
                };
                let field = |text: &'static str, input: Entity<TextInput>| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w_24()
                                .flex_none()
                                .text_color(theme::fg_dim())
                                .text_xs()
                                .child(text),
                        )
                        .child(div().flex_1().child(input))
                };
                let mut preset_list = div().flex().flex_col().gap_2();
                for (index, inputs) in preset_inputs.iter().enumerate() {
                    let label = |text: &'static str| {
                        div()
                            .w_24()
                            .flex_none()
                            .text_color(theme::fg_dim())
                            .text_xs()
                            .child(text)
                    };
                    preset_list = preset_list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::border())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(label("Name"))
                                    .child(div().flex_1().child(inputs.name.clone()))
                                    .child(
                                        div()
                                            .id(("preset-remove", index))
                                            .px_1()
                                            .rounded_sm()
                                            .text_color(theme::fg_dim())
                                            .cursor_pointer()
                                            .hover(|s| {
                                                s.bg(theme::selected_bg())
                                                    .text_color(theme::error())
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.remove_preset_row(index, cx);
                                            }))
                                            .child("×"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(label("Command"))
                                    .child(div().flex_1().child(inputs.command.clone())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(label("Resume"))
                                    .child(
                                        div().flex_1().child(inputs.resume_command.clone()),
                                    ),
                            ),
                    );
                }

                // Title and buttons stay fixed; everything in between
                // scrolls, so the dialog never outgrows the window.
                div()
                    // Focus target of last resort: with no preset rows there
                    // is no input to hold the keyboard, and it would fall
                    // back to the terminal behind the dialog.
                    .track_focus(focus_handle)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(640.))
                    .max_h_full()
                    .p_4()
                    .rounded_sm()
                    .bg(theme::panel_bg())
                    .border_1()
                    .border_color(theme::border())
                    .child(div().text_color(theme::fg()).child("Settings"))
                    .child(
                        div()
                            .id("settings-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(self.render_font_size_row(cx))
                            .child(self.render_theme_row(cx))
                            .child(section("Fonts"))
                            .child(field("Terminal", terminal_font.clone()))
                            .child(field("UI", ui_font.clone()))
                            .child(section("Planner"))
                            .child(
                                div()
                                    .text_color(theme::fg_dim())
                                    .text_xs()
                                    .child("Derives the branch and agent name from the task description."),
                            )
                            .child(field("Command", planner_command.clone()))
                            .child(field("Model", planner_model.clone()))
                            .when(
                                !planner_command.read(cx).text().trim().is_empty(),
                                |panel| {
                                    panel.child(
                                        div()
                                            .pl_24()
                                            .text_color(theme::fg_dim())
                                            .text_xs()
                                            .child("The model is unused while a command is set."),
                                    )
                                },
                            )
                            .child(
                                div()
                                    .text_color(theme::fg())
                                    .text_sm()
                                    .mt_2()
                                    .child("Agent presets"),
                            )
                            .child(
                                div()
                                    .text_color(theme::fg_dim())
                                    .text_xs()
                                    .child("The task description is appended to the command. Use claude-isol for sandboxed runs (github.com/tehrengruber/claude-container-isolation)."),
                            )
                            .child(preset_list)
                            .child(
                                div()
                                    .id("preset-add")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .text_color(theme::accent())
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_preset_row(window, cx);
                                    }))
                                    .child("+ Add preset"),
                            ),
                    )
                    .child(self.dialog_buttons("Save", cx))
                    .into_any_element()
            }
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .bg(gpui::hsla(0., 0., 0., 0.5))
                // A modal has to swallow the mouse: without this the backdrop
                // has no hitbox, so clicks and drags fall straight through to
                // the terminal underneath, which then starts a selection or
                // forwards the drag to the program behind the dialog.
                .occlude()
                // Escape belongs to the dialog as a whole, not just to its
                // text fields: the `escape` key binding is scoped to the
                // TextInput context, so it stops working the moment focus
                // moves to a button, a chip or the panel itself. This sits on
                // an ancestor of everything in the dialog, so it sees the key
                // whatever inside has focus.
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_dialog(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(panel)
                .into_any_element(),
        )
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::NewAgent {
                project_path,
                input,
                planning,
                ..
            }) => {
                if !*planning {
                    let task = input.read(cx).text().trim().to_string();
                    if !task.is_empty() {
                        self.spawn_agent(project_path.clone(), task, window, cx);
                    }
                }
            }
            Some(Dialog::Settings { .. }) => self.save_settings(window, cx),
            None => {}
        }
    }
}

/// Split a shell-style `KEY=value` word, as accepted at the front of a
/// configured agent command. Only names that look like environment variables
/// qualify, so a program path containing `=` isn't mistaken for one.
fn split_assignment(word: &str) -> Option<(String, String)> {
    let (name, value) = word.split_once('=')?;
    let valid = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then(|| (name.to_string(), value.to_string()))
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scale rem-based sizes (text_sm/text_xs etc.) with the base font
        // size; at the default size this yields the standard 16px rem.
        window.set_rem_size(px(
            theme::base_font_size(cx) * 16. / theme::DEFAULT_FONT_SIZE
        ));

        let mut root = div()
            .id("workspace")
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .size_full()
            .bg(theme::bg())
            .text_size(theme::ui_font_size(cx))
            .font_family(theme::ui_font().family.clone())
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.resizing_sidebar {
                    if event.pressed_button == Some(gpui::MouseButton::Left) {
                        this.state.settings.sidebar_width = f32::from(event.position.x)
                            .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
                        cx.notify();
                    } else {
                        // Button released outside the window.
                        this.resizing_sidebar = false;
                        this.persist(cx);
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing_sidebar {
                        this.resizing_sidebar = false;
                        this.persist(cx);
                        cx.notify();
                    }
                }),
            )
            .child(self.render_sidebar(cx));

        if !self.state.settings.sidebar_collapsed {
            root = root.child(
                div()
                    .id("sidebar-resize-handle")
                    .w(px(5.))
                    .h_full()
                    .flex_none()
                    .ml(px(-3.))
                    .cursor_col_resize()
                    .hover(|s| s.bg(theme::accent()))
                    .when(self.resizing_sidebar, |s| s.bg(theme::accent()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.resizing_sidebar = true;
                            cx.notify();
                        }),
                    ),
            );
        }

        root = root.child(self.render_main_pane(cx));

        if let Some(dialog) = self.render_dialog(cx) {
            root = root.child(dialog);
        }

        root
    }
}
