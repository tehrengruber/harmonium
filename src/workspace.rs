//! Root view: project/agent sidebar on the left, terminal pane on the right,
//! modal dialogs for adding projects and spawning agents.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window,
};

use crate::input::{InputEvent, TextInput};
use crate::planner;
use crate::state::{load_state, save_state, AgentId, AgentRecord, AppState, ProjectRecord};
use crate::terminal::view::TerminalView;
use crate::terminal::{Terminal, TerminalEvent};
use crate::theme;

struct PresetInputs {
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    resume_command: Entity<TextInput>,
}

enum Dialog {
    AddProject {
        input: Entity<TextInput>,
        _subscription: Subscription,
    },
    NewAgent {
        project_path: PathBuf,
        input: Entity<TextInput>,
        planning: bool,
        preset: usize,
        error: Option<String>,
        _subscription: Subscription,
    },
    Settings {
        preset_inputs: Vec<PresetInputs>,
        _subscriptions: Vec<Subscription>,
    },
}

const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH: f32 = 600.;

/// Inline editing of an agent's name in the sidebar.
struct InlineEdit {
    id: AgentId,
    input: Entity<TextInput>,
    _subscription: Subscription,
}

pub struct Workspace {
    state: AppState,
    terminals: HashMap<AgentId, Entity<TerminalView>>,
    terminal_subscriptions: HashMap<AgentId, Subscription>,
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
        Self {
            state,
            terminals: HashMap::new(),
            terminal_subscriptions: HashMap::new(),
            selected: None,
            dialog: None,
            inline_edit: None,
            resizing_sidebar: false,
            status: None,
            focus_handle: cx.focus_handle(),
        }
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

    fn open_add_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new("/path/to/git/repository", cx));
        let subscription = cx.subscribe(
            &input,
            |this, _input, event: &InputEvent, cx| match event {
                InputEvent::Submitted(text) => {
                    let text = text.trim().to_string();
                    this.add_project(text, cx);
                }
                InputEvent::Cancelled => {
                    this.dialog = None;
                    cx.notify();
                }
            },
        );
        window.focus(&input.focus_handle(cx));
        self.dialog = Some(Dialog::AddProject {
            input,
            _subscription: subscription,
        });
        cx.notify();
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
            self.terminals.remove(&agent.id);
            self.terminal_subscriptions.remove(&agent.id);
            if self.selected.as_ref() == Some(&agent.id) {
                self.selected = None;
            }
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
                InputEvent::Cancelled => {
                    this.dialog = None;
                    cx.notify();
                }
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

    fn make_preset_inputs(
        &mut self,
        name: &str,
        command: &str,
        resume_command: &str,
        subscriptions: &mut Vec<Subscription>,
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
            subscriptions.push(cx.subscribe(
                &input,
                |this, _input, event: &InputEvent, cx| match event {
                    InputEvent::Submitted(_) => this.save_settings(cx),
                    InputEvent::Cancelled => {
                        this.dialog = None;
                        cx.notify();
                    }
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

    fn open_settings_dialog(&mut self, cx: &mut Context<Self>) {
        let presets = self.state.settings.presets.clone();
        let mut subscriptions = Vec::new();
        let preset_inputs = presets
            .iter()
            .map(|p| {
                self.make_preset_inputs(
                    &p.name,
                    &p.command,
                    &p.resume_command,
                    &mut subscriptions,
                    cx,
                )
            })
            .collect();
        self.dialog = Some(Dialog::Settings {
            preset_inputs,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn add_preset_row(&mut self, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let inputs = self.make_preset_inputs("", "", "", &mut subscriptions, cx);
        if let Some(Dialog::Settings {
            preset_inputs,
            _subscriptions,
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

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::Settings { preset_inputs, .. }) = &self.dialog else {
            return;
        };
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
        self.dialog = None;
        self.persist(cx);
        cx.notify();
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
        cx.spawn_in(window, async move |this, cx| {
            let repo_for_bg = repo.clone();
            let task_for_bg = task.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    // No fallback: if the planner fails (e.g. usage limit
                    // reached), report it instead of inventing a branch.
                    let plan = planner::plan_task(&repo_for_bg, &task_for_bg)?;
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

        self.start_terminal(&id, command, vec![task], workspace.workdir, cx);
        self.status = None;
        self.select_agent(id, window, cx);
    }

    fn start_terminal(
        &mut self,
        id: &str,
        command: String,
        extra_args: Vec<String>,
        workdir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // Test/debug override: replaces the preset command entirely.
        let command = std::env::var("HARMONIUM_AGENT_BIN").unwrap_or(command);
        let mut parts = planner::split_command(&command);
        if parts.is_empty() {
            self.set_status(format!("Empty agent command: `{command}`"), true, cx);
            return;
        }
        let program = parts.remove(0);
        parts.extend(extra_args);
        match Terminal::create(program, parts, workdir, cx) {
            Ok(terminal) => {
                let subscription = cx.subscribe(
                    &terminal,
                    |_this, _terminal, _event: &TerminalEvent, cx| cx.notify(),
                );
                let view = cx.new(|cx| TerminalView::new(terminal, cx));
                self.terminals.insert(id.to_string(), view);
                self.terminal_subscriptions
                    .insert(id.to_string(), subscription);
            }
            Err(error) => {
                self.set_status(format!("Failed to spawn agent: {error}"), true, cx);
            }
        }
    }

    fn resume_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(record) = self.state.agent(&id).cloned() else {
            return;
        };
        self.terminals.remove(&id);
        self.terminal_subscriptions.remove(&id);
        let resume = record
            .resume_command
            .unwrap_or_else(|| "claude --continue".into());
        self.start_terminal(&id, resume, Vec::new(), record.workdir, cx);
        cx.notify();
    }

    fn remove_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        self.terminals.remove(&id);
        self.terminal_subscriptions.remove(&id);
        if self.selected.as_ref() == Some(&id) {
            self.selected = None;
        }
        for project in &mut self.state.projects {
            project.agents.retain(|a| a.id != id);
        }
        self.inline_edit = None;
        self.persist(cx);
        cx.notify();
    }

    fn select_agent(&mut self, id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        self.inline_edit = None;
        if let Some(view) = self.terminals.get(&id) {
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
                .child(
                    div()
                        .id("expand-sidebar")
                        .mt_2()
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
                    .child("No projects yet. Add a git repository to get started."),
            );
        }

        for (project_index, project) in self.state.projects.iter().enumerate() {
            let expanded = project.expanded;
            let project_path = project.path.clone();
            let path_label: SharedString = project.path.to_string_lossy().into_owned().into();

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
                            )
                            .child(
                                div()
                                    .text_color(theme::fg_dim())
                                    .text_xs()
                                    .whitespace_nowrap()
                                    .child(path_label),
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
                                    .child(
                                        div()
                                            .text_color(theme::fg())
                                            .text_sm()
                                            .whitespace_nowrap()
                                            .child(SharedString::from(agent.name.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme::fg_dim())
                                            .text_xs()
                                            .whitespace_nowrap()
                                            .child(SharedString::from(
                                                agent.branch.clone().unwrap_or_else(|| {
                                                    "base checkout".to_string()
                                                }),
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("edit-agent", project_index * 1000 + agent_index))
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(theme::fg_dim())
                                    .text_xs()
                                    .hover(|s| {
                                        s.bg(theme::selected_bg()).text_color(theme::accent())
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.start_inline_edit(edit_id.clone(), window, cx);
                                    }))
                                    .child("edit"),
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_color(theme::fg())
                            .text_sm()
                            .child("Projects"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("add-project")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .text_color(theme::accent())
                                    .text_sm()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover_bg()))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_add_project_dialog(window, cx);
                                    }))
                                    .child("+ Add"),
                            )
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
                            ),
                    ),
            )
            .child(div().flex_1().overflow_hidden().child(list))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .p_2()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .id("open-settings")
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .text_color(theme::fg_dim())
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_settings_dialog(cx);
                            }))
                            .child("⚙"),
                    ),
            )
            .into_any_element()
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
                match self.terminals.get(&id) {
                    Some(view) => {
                        pane = pane.child(div().flex_1().overflow_hidden().child(view.clone()));
                    }
                    None => {
                        let resume_id = id.clone();
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
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dialog = None;
                        cx.notify();
                    }))
                    .child("Cancel (esc)"),
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
            Dialog::AddProject { input, .. } => div()
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
                        .child("Add project (path to a git repository)"),
                )
                .child(input.clone())
                .child(self.dialog_buttons("Add", cx))
                .into_any_element(),

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

            Dialog::Settings { preset_inputs, .. } => {
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
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.add_preset_row(cx);
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
                .child(panel)
                .into_any_element(),
        )
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::AddProject { input, .. }) => {
                let text = input.read(cx).text().trim().to_string();
                self.add_project(text, cx);
            }
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
            Some(Dialog::Settings { .. }) => self.save_settings(cx),
            None => {}
        }
    }
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
