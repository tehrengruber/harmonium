//! Root view: project/agent sidebar on the left, terminal pane on the right,
//! modal dialogs for adding projects and spawning agents.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, svg, App, AppContext as _, Bounds, ClickEvent, Context, Div, ElementId, Entity,
    FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    Point, Render, ScrollHandle, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window,
};

use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use gpui_component::WindowExt as _;
use crate::log;
use crate::planner;
use crate::state::{
    AgentGroup, AgentId, AgentRecord, AppState, PendingAgentMove, PresetRecord, ProjectRecord,
    SaveError, StateFile, TerminalTabRecord, WorkspaceMode,
};
use crate::state;
use crate::terminal::view::TerminalView;
use crate::terminal::{Terminal, TerminalEvent};
use crate::theme;

struct PresetInputs {
    name: Entity<InputState>,
    command: Entity<InputState>,
    env: Entity<InputState>,
}

enum Dialog {
    NewAgent {
        project_path: PathBuf,
        /// Group the new agent lands in, when the spawn was started from a
        /// group header rather than the project row.
        group: Option<String>,
        input: Entity<InputState>,
        planning: bool,
        preset: usize,
        workspace_mode: WorkspaceMode,
        error: Option<String>,
        _subscription: Subscription,
    },
    /// Search the visible terminal's scrollback. Matches are shown by
    /// selecting them, so the dialog stays out of the way of the terminal
    /// itself and the usual copy shortcut works on whatever it found.
    Search {
        input: Entity<InputState>,
        match_case: bool,
        wrap: bool,
        /// Result of the last search — "no matches", "wrapped", …
        status: Option<String>,
        _subscription: Subscription,
    },
    Settings {
        /// Focus target for the panel itself, so the modal always owns the
        /// keyboard even when it has no input to focus (no presets yet).
        focus_handle: FocusHandle,
        planner_command: Entity<InputState>,
        planner_model: Entity<InputState>,
        terminal_font: Entity<InputState>,
        ui_font: Entity<InputState>,
        preset_inputs: Vec<PresetInputs>,
        /// Which preset row the planner should run through, as an index into
        /// `preset_inputs` so a rename in the same visit still points at the
        /// row the user picked. `None` is the built-in `claude -p`.
        planner_preset_row: Option<usize>,
        _subscriptions: Vec<Subscription>,
    },
}

/// Rows the task field grows to before it scrolls instead. Without a cap a
/// long description pushes the Spawn button off the bottom of the window.
const MAX_TASK_ROWS: usize = 12;

const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH: f32 = 600.;

/// Sidebar/tab drag payloads. Each is its own type so gpui only offers a
/// drop to rows of the same kind.
#[derive(Clone)]
struct ProjectDrag {
    index: usize,
}

#[derive(Clone)]
struct AgentDrag {
    /// Reordering is within one project: an agent's worktree belongs to its
    /// repository, so a drop onto another project's rows is ignored.
    project: usize,
    id: AgentId,
}

#[derive(Clone)]
struct TabDrag {
    agent: AgentId,
    id: String,
}

/// gpui wants a view for the item under the pointer, but the drop is
/// previewed by reordering the list in place, so this renders nothing.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The move a hovering drag would make, applied to the rendered order so the
/// list shows its final state before the drop.
#[derive(Clone, PartialEq)]
enum DragTarget {
    Project {
        from: usize,
        to: usize,
    },
    Agent {
        project_index: usize,
        from: AgentId,
        /// Row the dragged agent takes the place of. `None` when the pointer
        /// is on a group header: there may be no row there to aim at.
        to: Option<AgentId>,
        /// Group the row lands in — the target row's, or the header's.
        group: Option<String>,
    },
    Tab {
        agent: AgentId,
        from: String,
        to: String,
    },
}

/// One row under an expanded project, in the order they are drawn.
enum SidebarItem {
    /// Index into the project's `agents`.
    Agent(usize),
    /// Index into the project's `groups`.
    Header(usize),
    NewGroup,
}

/// A resolved command line for a terminal: the program to exec, its
/// arguments, and any `KEY=value` prefixes that came with it.
struct Spawn {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

/// What an inline edit in the sidebar is renaming.
#[derive(Clone, PartialEq)]
enum EditTarget {
    Agent(AgentId),
    /// A group header. Groups have no id of their own, so this is positional;
    /// the edit is transient enough that nothing gets to reorder underneath
    /// it, and the paths that could — removing a project or a group — end it.
    Group { project: usize, group: usize },
}

/// Inline editing of an agent's or a group's name in the sidebar.
struct InlineEdit {
    target: EditTarget,
    input: Entity<InputState>,
    _subscription: Subscription,
}

/// A sidebar row that reveals its actions under the pointer.
#[derive(PartialEq, Eq, Clone, Debug)]
enum HoveredRow {
    Project(usize),
    Agent(AgentId),
    Group { project: usize, group: usize },
}

pub struct Workspace {
    state: AppState,
    /// Where `state` came from and where it goes back to.
    state_file: StateFile,
    /// Set once the state file has been found changed underneath us. Saving
    /// stays refused from then on; this only keeps the log from filling with
    /// the same complaint on every later save.
    save_conflict: bool,
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
    /// Pending drag, previewed in the list until the drop lands.
    drag_target: Option<DragTarget>,
    /// The sidebar row the pointer is over, if any. Kept in view state
    /// rather than left to gpui's `group_hover`, because these rows *reflow*
    /// on hover — the name or the rule gives up the width the actions take —
    /// and group styles are applied during paint, after layout has already
    /// run, so a width set there is silently ignored.
    hovered_row: Option<HoveredRow>,
    /// Scroll position of the log panel, so new output can pin to the bottom.
    log_scroll: ScrollHandle,
    /// Log version last rendered, and the task polling for more output while
    /// the panel is open.
    log_version: usize,
    log_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl Workspace {
    /// Takes the loaded state rather than reading it: a state file that can't
    /// be read stops harmonium before the window opens (see `main`), so by
    /// here there is always a session to show. The handle it was read through
    /// comes along, because saving needs to know what the file held then.
    pub fn new(state: AppState, state_file: StateFile, cx: &mut Context<Self>) -> Self {
        cx.set_global(theme::FontSettings {
            base: state.settings.font_size,
        });
        theme::set_mode(state.settings.theme);
        theme::set_fonts(&state.settings.ui_font, &state.settings.terminal_font);
        let mut workspace = Self {
            state,
            state_file,
            save_conflict: false,
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
            drag_target: None,
            hovered_row: None,
            log_scroll: ScrollHandle::new(),
            log_version: log::version(),
            log_task: None,
            focus_handle: cx.focus_handle(),
        };
        if workspace.state.settings.log_panel_open {
            workspace.start_log_polling(cx);
        }
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

    /// The environment for a saved agent's processes: the `HARMONIUM_TASK_*`
    /// variables followed by the preset's own `KEY=value` words. Resolved from
    /// state at spawn time so resume and lazy restore see the same values as
    /// the first spawn — including the owning project's path, which the agent
    /// record doesn't store.
    fn agent_task_env(&self, id: &AgentId) -> Vec<(String, String)> {
        let Some(project) = self.state.project_for_agent(id) else {
            return Vec::new();
        };
        let Some(record) = project.agents.iter().find(|a| &a.id == id) else {
            return Vec::new();
        };
        let mut env = task_env(&project.path, &record.workdir, record.branch.as_deref());
        // Appended, so a preset variable overrides a task variable of the same
        // name — the same precedence a `KEY=value` command prefix has.
        let preset_env = parse_env(record.env.as_deref().unwrap_or_default(), &env);
        env.extend(preset_env);
        env
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
        let task_env = self.agent_task_env(id);
        if !self.terminals.contains_key(id) {
            self.start_agent_terminal(
                id,
                resume,
                Vec::new(),
                record.workdir.clone(),
                task_env.clone(),
                cx,
            );
        }
        for tab in &record.terminals {
            // Guard against restoring a tab that was just spawned by hand.
            if !self.terminals.contains_key(&tab.id) {
                self.start_shell_tab(&tab.id, &record.workdir, task_env.clone(), cx);
            }
        }
        self.active_tabs
            .entry(id.clone())
            .or_insert_with(|| id.clone());
    }

    /// Spawn a shell tab, replaying any saved history file into the PTY first.
    /// Shell tabs get the same environment as the agent — the `HARMONIUM_TASK_*`
    /// variables and the preset's own, since a shell sitting in the agent's
    /// workdir wants the same mounts, paths and settings — but no `$VAR`
    /// expansion: their command line isn't user-configured.
    fn start_shell_tab(
        &mut self,
        id: &str,
        workdir: &Path,
        task_env: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
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
            env: task_env,
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
        theme::sync_component_theme(cx);
        self.state.settings.theme = mode;
        self.persist(cx);
        let views: Vec<_> = self.terminals.values().cloned().collect();
        for view in views {
            view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        match self.state_file.save(&self.state) {
            Ok(()) => {}
            // Somebody else wrote the state file. Nothing was lost — this
            // session is parked next to it — but there is no way back from
            // here in this process, so the banner is re-raised on every later
            // save while the log keeps just the first one.
            Err(error @ SaveError::Conflict { .. }) => {
                let message = error.to_string();
                if !self.save_conflict {
                    self.save_conflict = true;
                    log::error(message.clone());
                }
                self.status = Some((message, true));
                cx.notify();
            }
            Err(error) => self.set_status(format!("Failed to save state: {error}"), true, cx),
        }
    }

    fn set_status(&mut self, message: String, is_error: bool, cx: &mut Context<Self>) {
        if is_error {
            log::error(message.clone());
        } else {
            log::info(message.clone());
        }
        self.status = Some((message, is_error));
        cx.notify();
    }

    // ---- Log panel ----

    fn toggle_log_panel(&mut self, cx: &mut Context<Self>) {
        let open = !self.state.settings.log_panel_open;
        self.state.settings.log_panel_open = open;
        if open {
            self.log_scroll.scroll_to_bottom();
            self.start_log_polling(cx);
        } else {
            self.log_task = None;
        }
        self.persist(cx);
        cx.notify();
    }

    /// Writers to the log have no handle to the UI, so while the panel is
    /// open we poll its version and repaint when it changes.
    fn start_log_polling(&mut self, cx: &mut Context<Self>) {
        self.log_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(400))
                    .await;
                let keep_polling = this
                    .update(cx, |this, cx| {
                        if !this.state.settings.log_panel_open {
                            return false;
                        }
                        let version = log::version();
                        if version != this.log_version {
                            this.log_version = version;
                            this.log_scroll.scroll_to_bottom();
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_polling {
                    break;
                }
            }
        }));
    }

    fn render_log_panel(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let entries = log::entries();
        let mut list = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .font_family(theme::terminal_font().family.clone())
            .text_size(px(theme::base_font_size(cx) - 1.));
        if entries.is_empty() {
            list = list.child(
                div()
                    .text_color(theme::fg_dim())
                    .child("Nothing logged yet."),
            );
        }
        for entry in entries {
            let color = match entry.level {
                log::Level::Error => theme::error(),
                log::Level::Info => theme::fg(),
            };
            list = list.child(
                div()
                    .flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::fg_dim())
                            .child(SharedString::from(entry.time)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(color)
                            .child(SharedString::from(entry.message)),
                    ),
            );
        }

        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_xs()
                .text_color(theme::fg_dim())
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                .child(label)
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .h(px(180.))
            .bg(theme::panel_bg())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().text_color(theme::fg_dim()).text_xs().child("Log"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(button("log-clear", "Clear").on_click(cx.listener(
                                |this, _, _, cx| {
                                    log::clear();
                                    this.log_version = log::version();
                                    cx.notify();
                                },
                            )))
                            .child(button("log-close", "×").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.toggle_log_panel(cx);
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .id("log-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .child(list),
            )
            .into_any_element()
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
        log::info(format!("project added: {}", path.display()));
        self.state.projects.push(ProjectRecord::new(path));
        self.status = None;
        self.persist(cx);
        cx.notify();
    }

    fn remove_project(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.state.projects.len() {
            return;
        }
        // An inline edit is positional for groups: the indices behind this
        // project have just shifted out from under it.
        self.inline_edit = None;
        let project = self.state.projects.remove(index);
        for agent in &project.agents {
            let tab_ids: Vec<String> =
                agent.terminals.iter().map(|t| t.id.clone()).collect();
            self.teardown_agent(&agent.id, &tab_ids, cx);
        }
        self.persist(cx);
        cx.notify();
    }

    // ---- Reordering ----

    /// Move `from` to sit where `to` currently is, the usual list-drag
    /// behaviour. Returns the moved element's new index.
    fn reorder<T>(items: &mut Vec<T>, from: usize, to: usize) {
        if from == to || from >= items.len() || to >= items.len() {
            return;
        }
        let item = items.remove(from);
        items.insert(to, item);
    }

    /// A drag only tracks the axis it moves along: leaving the row sideways —
    /// out of the sidebar, or below the tab bar — should not stop the list
    /// from following the pointer.
    fn spans_row(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
        position.y >= bounds.origin.y && position.y < bounds.origin.y + bounds.size.height
    }

    fn spans_column(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
        position.x >= bounds.origin.x && position.x < bounds.origin.x + bounds.size.width
    }

    /// A row/tab claims the drag once the pointer is past its middle, rather
    /// than clear of it: half a row of travel is enough to land in the slot,
    /// and the remaining half is the hysteresis that stops it oscillating.
    fn past_middle(shown_before: bool, middle: Pixels, position: Pixels) -> bool {
        if shown_before {
            position <= middle
        } else {
            position >= middle
        }
    }

    /// Whether `hovered` currently renders before `dragged` — which half of
    /// the hovered row counts depends on the direction of travel, and the
    /// preview, not the stored order, is what the user sees.
    fn shown_before(
        len: usize,
        pending: Option<(usize, usize)>,
        hovered: usize,
        dragged: usize,
    ) -> bool {
        let order = Self::preview_order(len, pending.map(|p| p.0), pending.map(|p| p.1));
        let position = |item: usize| order.iter().position(|&i| i == item).unwrap_or(item);
        position(hovered) < position(dragged)
    }

    /// Display order for a list of `len` items with the pending move applied,
    /// as indices into the real list.
    fn preview_order(len: usize, from: Option<usize>, to: Option<usize>) -> Vec<usize> {
        let mut order: Vec<usize> = (0..len).collect();
        if let (Some(from), Some(to)) = (from, to) {
            Self::reorder(&mut order, from, to);
        }
        order
    }

    /// Commit the previewed move. The preview is authoritative: once the list
    /// has reordered on screen the row under the pointer is the dragged item
    /// itself, so recomputing from the drop target would be a no-op.
    fn apply_drag_target(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.drag_target.clone() else {
            return false;
        };
        match target {
            DragTarget::Project { from, to } => self.move_project(from, to, cx),
            DragTarget::Agent {
                project_index,
                from,
                to,
                group,
            } => self.move_agent(project_index, &from, to.as_ref(), group, cx),
            DragTarget::Tab { agent, from, to } => self.move_tab(&agent, &from, &to, cx),
        }
        true
    }

    /// The chrome every row in the sidebar shares — projects, agents, group
    /// headers and the two "new …" rows alike: one line high, its own hover
    /// highlight, and a click target spanning the panel. What tells the kinds
    /// apart is behaviour, not shape, so the caller adds the drag, drop and
    /// click handling and any indentation of its own.
    fn sidebar_row(id: impl Into<ElementId>) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .rounded_sm()
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_bg()))
    }

    /// An action at the right end of a sidebar row. Rendered only while its
    /// row is hovered — see [`Workspace::hovered_row`] — so a row at rest is
    /// just its name, with the full width to show it in.
    ///
    /// The tints reach a text glyph through the inherited text style. An
    /// `svg` icon is not text: gpui paints one from its *own* computed
    /// `text.color` and inherits nothing, so an svg passed here has to carry
    /// its colour, and the hover tint reaches only the background behind it.
    fn row_action(
        id: impl Into<ElementId>,
        tint: Hsla,
        hover_tint: Hsla,
        icon: impl IntoElement,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex_none()
            .px_1()
            .rounded_sm()
            // The same size as the label beside it, so a glyph action sits
            // on the row's text metrics instead of the panel's default.
            .text_sm()
            .text_color(tint)
            .hover(move |s| s.bg(theme::selected_bg()).text_color(hover_tint))
            .on_click(cx.listener(move |this, _, window, cx| {
                // Without this the row underneath also selects, folds, or
                // opens its own editor.
                cx.stop_propagation();
                action(this, window, cx);
            }))
            .child(icon)
    }

    /// An icon for a sidebar action. Sized in rems like the text beside it,
    /// so both grow together with the font-size setting and keep sharing the
    /// row's baseline — an icon needs no nudging to sit level, and a nudge is
    /// only ever right at the one size it was measured at.
    fn row_icon(path: &'static str) -> gpui::Svg {
        svg()
            .path(path)
            .size_3()
            .text_color(theme::fg_dim())
    }

    /// The name a sidebar row carries: one line, taking whatever width the
    /// actions leave it and clipping rather than wrapping.
    fn row_label(text: impl Into<SharedString>, color: Hsla) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(
                div()
                    .text_color(color)
                    .text_sm()
                    .whitespace_nowrap()
                    .child(text.into()),
            )
    }

    /// Record (or clear) the row under the pointer. A row's leave arrives
    /// after the next row's enter, so a leave only clears the state when it
    /// still names the row that set it.
    fn set_hovered_row(&mut self, row: HoveredRow, hovered: bool, cx: &mut Context<Self>) {
        let next = if hovered {
            Some(row)
        } else if self.hovered_row.as_ref() == Some(&row) {
            None
        } else {
            return;
        };
        if self.hovered_row != next {
            self.hovered_row = next;
            cx.notify();
        }
    }

    fn set_drag_target(&mut self, target: DragTarget, cx: &mut Context<Self>) {
        if self.drag_target.as_ref() != Some(&target) {
            self.drag_target = Some(target);
            cx.notify();
        }
    }

    fn move_project(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.drag_target = None;
        Self::reorder(&mut self.state.projects, from, to);
        self.persist(cx);
        cx.notify();
    }

    /// Drop an agent onto `to` — or, when `to` is `None`, at the top of
    /// `group`, which is what dropping on a header means. The row adopts the
    /// group it lands in, so one gesture both reorders and regroups.
    fn move_agent(
        &mut self,
        project_index: usize,
        from: &AgentId,
        to: Option<&AgentId>,
        group: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.drag_target = None;
        let Some(project) = self.state.projects.get_mut(project_index) else {
            return;
        };
        let index = |agents: &[AgentRecord], id: &AgentId| {
            agents.iter().position(|agent| &agent.id == id)
        };
        let Some(from) = index(&project.agents, from) else {
            return;
        };
        // Before any move: setting the group doesn't shift indices.
        project.agents[from].group = group.clone();
        match to {
            Some(to) => {
                let Some(to) = index(&project.agents, to) else {
                    return;
                };
                Self::reorder(&mut project.agents, from, to);
            }
            None => {
                let record = project.agents.remove(from);
                let head = project
                    .agents
                    .iter()
                    .position(|agent| agent.group == group)
                    .unwrap_or(project.agents.len());
                project.agents.insert(head, record);
                // Landing in a folded group would put the row somewhere the
                // user can't see it.
                if let Some(index) = group.as_deref().and_then(|g| project.group_index(g)) {
                    project.groups[index].expanded = true;
                }
            }
        }
        // A drop at a run's edge is ambiguous about which side it lands on;
        // this settles it without disturbing the order inside the run.
        project.normalize_agent_order();
        self.persist(cx);
        cx.notify();
    }

    /// The drag currently hovering `project_index`'s agent rows, in the shape
    /// [`ProjectRecord::previewed_agents`] wants.
    fn pending_agent_move(&self, project_index: usize) -> Option<PendingAgentMove<'_>> {
        match &self.drag_target {
            Some(DragTarget::Agent {
                project_index: project,
                from,
                to,
                group,
            }) if *project == project_index => Some(PendingAgentMove {
                from,
                to: to.as_ref(),
                group: group.as_deref(),
            }),
            _ => None,
        }
    }

    fn move_tab(&mut self, agent: &AgentId, from: &str, to: &str, cx: &mut Context<Self>) {
        self.drag_target = None;
        let Some(record) = self.state.agent_mut(agent) else {
            return;
        };
        let index = |id: &str| record.terminals.iter().position(|tab| tab.id == id);
        let (Some(from), Some(to)) = (index(from), index(to)) else {
            return;
        };
        Self::reorder(&mut record.terminals, from, to);
        self.persist(cx);
        cx.notify();
    }

    // ---- Agents ----

    fn open_new_agent_dialog(
        &mut self,
        project_path: PathBuf,
        group: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Describe the task… (ctrl-enter to spawn)")
                .multi_line(true)
                .auto_grow(1, MAX_TASK_ROWS)
        });
        let path = project_path.clone();
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, window, cx| {
                // Multi-line: plain enter inserts a newline, the secondary
                // chord (ctrl-enter here) spawns. Escape is handled by the
                // dialog overlay, which owns it for every dialog.
                if let InputEvent::PressEnter { secondary: true } = event {
                    let task = input.read(cx).value().trim().to_string();
                    if !task.is_empty() {
                        this.spawn_agent(path.clone(), task, window, cx);
                    }
                }
            },
        );
        let focus = input.focus_handle(cx);
        let preset = self
            .state
            .settings
            .last_preset
            .min(self.state.settings.presets.len().saturating_sub(1));
        self.dialog = Some(Dialog::NewAgent {
            project_path,
            group,
            input,
            planning: false,
            preset,
            // Always Auto, never the last choice: forcing a workspace is a
            // decision about *one* task, and inheriting it silently would put
            // the next task somewhere it wasn't meant to go.
            workspace_mode: WorkspaceMode::Auto,
            error: None,
            _subscription: subscription,
        });
        // After presenting, never before: opening the dialog layer focuses
        // the dialog itself, which would take the keyboard off the field.
        self.present_dialog(px(560.), window, cx);
        window.focus(&focus);
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
    ) -> Entity<InputState> {
        let value = value.to_string();
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(placeholder);
            if !value.is_empty() {
                state.set_value(value, window, cx);
            }
            state
        });
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_settings(window, cx);
                }
            },
        ));
        input
    }

    fn make_preset_inputs(
        &mut self,
        preset: &PresetRecord,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PresetInputs {
        let mut make = |placeholder: &'static str, value: &str| {
            let value = value.to_string();
            let input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                if !value.is_empty() {
                    state.set_value(value, window, cx);
                }
                state
            });
            // `subscribe_in` rather than `subscribe`: closing the dialog has
            // to hand the keyboard back to the terminal, which needs a window.
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save_settings(window, cx);
                    }
                },
            ));
            input
        };
        PresetInputs {
            name: make("Preset name", &preset.name),
            command: make("Command (task, --continue or planner flags appended)", &preset.command),
            env: make("KEY=value KEY2=value2", &preset.env),
        }
    }

    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let presets = self.state.settings.presets.clone();
        let mut subscriptions = Vec::new();
        let preset_inputs: Vec<PresetInputs> = presets
            .iter()
            .map(|p| self.make_preset_inputs(p, &mut subscriptions, window, cx))
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
        let focus = match preset_inputs.first() {
            Some(first) => first.name.focus_handle(cx),
            None => focus_handle.clone(),
        };
        let planner_preset_row = self.state.settings.planner_preset.as_deref().and_then(|name| {
            self.state
                .settings
                .presets
                .iter()
                .position(|preset| preset.name == name)
        });
        self.dialog = Some(Dialog::Settings {
            focus_handle,
            planner_command,
            planner_model,
            terminal_font,
            ui_font,
            preset_inputs,
            planner_preset_row,
            _subscriptions: subscriptions,
        });
        self.present_dialog(px(620.), window, cx);
        window.focus(&focus);
    }

    fn add_preset_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let inputs =
            self.make_preset_inputs(&PresetRecord::default(), &mut subscriptions, window, cx);
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
        window.close_dialog(cx);
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
            planner_preset_row,
            ..
        }) = &self.dialog
        else {
            return;
        };
        let planner_preset_row = *planner_preset_row;
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let planner_command = text(planner_command);
        let planner_model = text(planner_model);
        let terminal_font = text(terminal_font);
        let ui_font = text(ui_font);
        let mut presets: Vec<crate::state::PresetRecord> = Vec::new();
        // Follow the planner's row through: empty rows are dropped here, so
        // the saved name has to be looked up while the mapping is still known.
        let mut planner_preset = None;
        for (row, inputs) in preset_inputs.iter().enumerate() {
            let name = inputs.name.read(cx).value().trim().to_string();
            let command = inputs.command.read(cx).value().trim().to_string();
            let env = inputs.env.read(cx).value().trim().to_string();
            if name.is_empty() && command.is_empty() {
                continue;
            }
            let name = if name.is_empty() { command.clone() } else { name };
            if planner_preset_row == Some(row) {
                planner_preset = Some(name.clone());
            }
            presets.push(crate::state::PresetRecord {
                name,
                command,
                resume_command: None,
                env,
            });
        }
        self.state.settings.presets = presets;
        self.state.settings.planner_preset = planner_preset;
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
        let workspace_mode;
        let group;
        if let Some(Dialog::NewAgent {
            planning,
            preset,
            workspace_mode: mode,
            group: into_group,
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
            workspace_mode = *mode;
            group = into_group.clone();
        } else {
            preset_index = self.state.settings.last_preset;
            workspace_mode = WorkspaceMode::Auto;
            group = None;
        }
        let preset = self
            .state
            .settings
            .presets
            .get(preset_index)
            .cloned()
            .unwrap_or_else(|| PresetRecord {
                name: "claude".into(),
                command: "claude".into(),
                resume_command: None,
                env: String::new(),
            });
        self.state.settings.last_preset = preset_index;
        self.persist(cx);
        cx.notify();

        let repo = project_path.clone();
        let planner_settings = self.planner_settings(&project_path);
        cx.spawn_in(window, async move |this, cx| {
            let repo_for_bg = repo.clone();
            let task_for_bg = task.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    // No fallback: if the planner fails (e.g. usage limit
                    // reached), report it instead of inventing a branch. It
                    // runs even when the workspace is already decided — the
                    // agent's name comes from the task in every mode.
                    let mut plan =
                        planner::plan_task(&repo_for_bg, &task_for_bg, &planner_settings)?;
                    planner::apply_workspace_mode(&mut plan, workspace_mode, &task_for_bg);
                    planner::resolve_workspace(&repo_for_bg, &plan, &task_for_bg)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                // One task per worktree: git allows one worktree per branch,
                // so a plan naming a branch that is already checked out lands
                // in the directory another agent is working in. Refused here
                // rather than in the planner, because "taken" is a fact about
                // harmonium's agents, not about the repository.
                let outcome = match result {
                    Ok(workspace) => match this.occupant_of(&workspace) {
                        Some(existing) => {
                            // The way out differs by where the collision is.
                            // On a branch, "New worktree" is no help: it keeps
                            // the branch the planner named and would land here
                            // again, so the description has to change. On the
                            // main checkout, a worktree is exactly the fix.
                            let (place, advice) = match &workspace.branch {
                                Some(branch) => (
                                    format!("on `{branch}` in {}", workspace.workdir.display()),
                                    "describe this one so it lands on a different branch",
                                ),
                                None => (
                                    format!(
                                        "in the project checkout {}",
                                        workspace.workdir.display()
                                    ),
                                    "spawn this one with New worktree",
                                ),
                            };
                            Err(format!(
                                "`{}` is already working {place} — resume that task, or {advice}",
                                existing.name
                            ))
                        }
                        None => Ok(workspace),
                    },
                    Err(spawn_error) => Err(format!("{spawn_error:#}")),
                };
                match outcome {
                    Ok(workspace) => {
                        // Dismiss the dialog layer too, not just our state:
                        // the panel lives in Root now, so dropping the state
                        // alone would leave it on screen.
                        this.dialog = None;
                        window.close_dialog(cx);
                        this.finish_spawn(repo, group, task, workspace, preset, window, cx);
                    }
                    Err(message) => {
                        // Keep the dialog open so the task text isn't lost;
                        // show the error inline for a retry.
                        log::error(message.clone());
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

    /// The preset the planner runs through, if one is selected and still
    /// exists. Matched by name rather than index so reordering or editing the
    /// list around it doesn't silently repoint the planner at another agent.
    fn planner_preset(&self) -> Option<&PresetRecord> {
        let name = self.state.settings.planner_preset.as_deref()?;
        self.state
            .settings
            .presets
            .iter()
            .find(|preset| preset.name == name)
    }

    fn planner_settings(&self, repo: &Path) -> planner::PlannerSettings {
        let model = self.state.settings.planner_model.clone();
        let preset = self.planner_preset();
        planner::PlannerSettings {
            command: self.state.settings.planner_command.clone(),
            preset_argv: preset
                .map(|preset| planner_argv(preset, &model, repo))
                .unwrap_or_default(),
            // The planner is a one-shot run of the same agent, so it gets the
            // preset's environment too, expanded against the same variables.
            env: preset
                .map(|preset| parse_env(&preset.env, &planner_task_env(repo)))
                .unwrap_or_default(),
            model,
        }
    }

    /// The agent already working in `workspace`, if any. One task per
    /// directory, the project's own checkout included: two agents editing the
    /// same files is the hazard, and it doesn't get safer just because the
    /// directory happens to be the main checkout rather than a worktree.
    fn occupant_of(&self, workspace: &planner::Workspace) -> Option<&AgentRecord> {
        self.state
            .projects
            .iter()
            .flat_map(|project| project.agents.iter())
            .find(|agent| agent.workdir == workspace.workdir)
    }

    fn finish_spawn(
        &mut self,
        project_path: PathBuf,
        group: Option<String>,
        task: String,
        workspace: planner::Workspace,
        preset: PresetRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let command = preset.command.clone();
        let record = AgentRecord {
            id: id.clone(),
            name: workspace.agent_name.clone(),
            description: task.clone(),
            workdir: workspace.workdir.clone(),
            branch: workspace.branch.clone(),
            command: Some(preset.command.clone()),
            resume_command: Some(preset.resume_command()),
            env: Some(preset.env.clone()),
            terminals: Vec::new(),
            group: None,
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
        log::info(format!(
            "agent {}: created on {} in {}",
            workspace.agent_name,
            workspace.branch.as_deref().unwrap_or("the base checkout"),
            workspace.workdir.display()
        ));
        project.agents.push(record);
        project.expanded = true;
        // Spawned from a group header: land in that group, unless it was
        // renamed or deleted while the planner was running.
        if let Some(group) = group.filter(|name| project.group_index(name).is_some()) {
            if let Some(agent) = project.agents.last_mut() {
                agent.group = Some(group);
            }
            project.normalize_agent_order();
        }
        self.persist(cx);

        let task_env = self.agent_task_env(&id);
        self.start_agent_terminal(&id, command, vec![task], workspace.workdir, task_env, cx);
        // A brand-new agent has nothing saved to restore.
        self.restored.insert(id.clone());
        self.active_tabs.insert(id.clone(), id.clone());
        self.status = None;
        self.select_agent(id, window, cx);
    }

    /// The `+` in the tab bar, and its ctrl-shift-t binding: append a
    /// "Shell N" tab to an agent. No-op for an unknown agent.
    fn add_next_shell_tab(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = self
            .state
            .agent(&agent_id)
            .map(|record| record.terminals.len() + 1)
        else {
            return;
        };
        self.add_terminal_tab(agent_id, format!("Shell {next}"), window, cx);
    }

    /// ctrl-shift-t. Nothing is selected before the first agent is opened, and
    /// then there is no tab bar to add to either.
    fn new_terminal_tab(
        &mut self,
        _: &crate::NewTerminalTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent_id) = self.selected.clone() else {
            return;
        };
        self.add_next_shell_tab(agent_id, window, cx);
    }

    // ---- Terminal search ----

    fn search_terminal(
        &mut self,
        _: &crate::SearchTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_dialog(window, cx);
    }

    fn open_search_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing to search in without a live terminal on screen.
        if self.selected.as_ref().and_then(|id| self.active_terminal(id)).is_none() {
            self.set_status("No terminal to search".into(), true, cx);
            return;
        }
        // Reopening keeps the previous query and options; only the focus and
        // the stale result line are reset.
        if let Some(Dialog::Search { input, status, .. }) = &mut self.dialog {
            *status = None;
            let input = input.clone();
            // Select the old query so the next keystroke replaces it.
            window.focus(&input.focus_handle(cx));
            window.dispatch_action(Box::new(SelectAll), cx);
            cx.notify();
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find in terminal…"));
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, _input, event: &InputEvent, _window, cx| match event {
                // Enter walks forward through the matches.
                InputEvent::PressEnter { .. } => this.find_in_terminal(true, cx),
                _ => {}
            },
        );
        let focus = input.focus_handle(cx);
        self.dialog = Some(Dialog::Search {
            input,
            match_case: false,
            wrap: true,
            status: None,
            _subscription: subscription,
        });
        self.present_dialog(px(460.), window, cx);
        window.focus(&focus);
    }

    /// Run one search step against the terminal the user is looking at.
    fn find_in_terminal(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(Dialog::Search {
            input,
            match_case,
            wrap,
            ..
        }) = &self.dialog
        else {
            return;
        };
        let query = input.read(cx).value().to_string();
        let options = crate::terminal::SearchOptions {
            forward,
            match_case: *match_case,
            wrap: *wrap,
        };
        let Some(view) = self
            .selected
            .clone()
            .and_then(|id| self.active_terminal(&id))
            .cloned()
        else {
            return;
        };
        let terminal = view.read(cx).terminal.clone();
        let outcome = terminal.update(cx, |terminal, cx| terminal.search(&query, options, cx));
        let message = match outcome {
            Ok(crate::terminal::SearchOutcome::Found { wrapped: true }) => {
                Some(if forward {
                    "Wrapped to the top".to_string()
                } else {
                    "Wrapped to the bottom".to_string()
                })
            }
            Ok(crate::terminal::SearchOutcome::Found { wrapped: false }) => None,
            Ok(crate::terminal::SearchOutcome::NoMatch) => {
                (!query.is_empty()).then(|| format!("No matches for `{query}`"))
            }
            Ok(crate::terminal::SearchOutcome::EndOfBuffer) => Some(
                if forward {
                    "No more matches below — turn on Wrap around"
                } else {
                    "No more matches above — turn on Wrap around"
                }
                .to_string(),
            ),
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(Dialog::Search { status, .. }) = &mut self.dialog {
            *status = message;
        }
        cx.notify();
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
        let task_env = self.agent_task_env(&agent_id);
        self.start_shell_tab(&tab_id, &record.workdir, task_env, cx);
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
        task_env: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        // Test/debug override: replaces the preset command entirely. Scoped
        // to agent terminals so it can't hijack shell tabs.
        let command = std::env::var("HARMONIUM_AGENT_BIN").unwrap_or(command);
        let mut parts = planner::split_command(&command);
        // Leading `KEY=value` words are environment for the child, as in a
        // shell — e.g. `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1 claude`, which
        // keeps the agent's output in the scrollback where it can be scrolled
        // and selected. They are appended last, so an explicit assignment
        // overrides a task or preset variable of the same name — and unlike the
        // preset's env field, it reaches only this process, not the shell tabs.
        let mut env = task_env.clone();
        while let Some((name, value)) = parts.first().and_then(|word| split_assignment(word)) {
            env.push((name, expand_vars(&value, &task_env)));
            parts.remove(0);
        }
        // Expanded per spawn, word by word — the stored command keeps the
        // unexpanded `$VAR`, and a value containing spaces stays one argument.
        for part in &mut parts {
            *part = expand_vars(part, &task_env);
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
        log::info(format!(
            "terminal {id}: spawning `{} {}` in {}",
            spawn.program,
            spawn.args.join(" "),
            workdir.display()
        ));
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
                        if matches!(event, TerminalEvent::Exited) {
                            log::info(format!("terminal {terminal_id}: process exited"));
                        }
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
        log::info(format!("agent {}: resuming with `{resume}`", record.name));
        let task_env = self.agent_task_env(&id);
        self.start_agent_terminal(&id, resume, Vec::new(), workdir.clone(), task_env.clone(), cx);
        if !self.terminals.contains_key(&id) {
            // The respawn failed; without the snapshot dropped, the "No live
            // session / Resume" UI would be unreachable for a retry.
            self.outgoing_terminals.remove(&id);
        }
        // Restart persisted extra tabs, replaying the history just written.
        for tab in &record.terminals {
            self.start_shell_tab(&tab.id, &workdir, task_env.clone(), cx);
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

    /// The worktree an agent owns outright, as `(repository, worktree)` —
    /// i.e. the one that may be deleted with it. `None` for the cases where
    /// the directory isn't ours to delete:
    ///
    /// - base mode, where the agent works in the project's own checkout;
    /// - a worktree another agent is also using (git allows one worktree per
    ///   branch, so two tasks on one branch share a directory);
    /// - a workdir that is already gone.
    fn owned_worktree(&self, id: &AgentId, record: &AgentRecord) -> Option<(PathBuf, PathBuf)> {
        let repo = self.state.project_for_agent(id)?.path.clone();
        if record.branch.is_none() || record.workdir == repo || !record.workdir.is_dir() {
            return None;
        }
        let shared = self
            .state
            .projects
            .iter()
            .flat_map(|project| project.agents.iter())
            .any(|other| other.id != *id && other.workdir == record.workdir);
        if shared {
            log::info(format!(
                "agent {}: keeping worktree {}, another agent is using it",
                record.name,
                record.workdir.display()
            ));
            return None;
        }
        Some((repo, record.workdir.clone()))
    }

    /// Remove an agent and delete the worktree it was working in — but only
    /// once that worktree is clean. Deleting a task kills its terminals and
    /// its checkout with no undo, so uncommitted work blocks the whole
    /// operation rather than being thrown away. The branch survives either
    /// way, so anything committed is still reachable afterwards.
    fn remove_agent(&mut self, id: AgentId, cx: &mut Context<Self>) {
        let Some(record) = self.state.agent(&id).cloned() else {
            return;
        };
        if let Some((repo, workdir)) = self.owned_worktree(&id, &record) {
            match planner::is_dirty(&workdir) {
                Ok(true) => {
                    self.set_status(
                        format!(
                            "{}: worktree has uncommitted changes — commit or discard them in {} first",
                            record.name,
                            workdir.display()
                        ),
                        true,
                        cx,
                    );
                    return;
                }
                Err(error) => {
                    self.set_status(
                        format!("{}: cannot check the worktree: {error:#}", record.name),
                        true,
                        cx,
                    );
                    return;
                }
                Ok(false) => {}
            }
            // Before the teardown below: if git refuses, nothing has been
            // killed yet and the agent is left exactly as it was.
            if let Err(error) = planner::remove_worktree(&repo, &workdir) {
                self.set_status(
                    format!("{}: cannot remove the worktree: {error:#}", record.name),
                    true,
                    cx,
                );
                return;
            }
            log::info(format!(
                "agent {}: worktree {} removed, branch {} kept",
                record.name,
                workdir.display(),
                record.branch.as_deref().unwrap_or("(none)")
            ));
        }
        log::info(format!("agent {}: removed", record.name));
        // A refusal from an earlier attempt must not linger next to a delete
        // that just worked.
        self.status = None;
        let tab_ids: Vec<String> = record.terminals.iter().map(|t| t.id.clone()).collect();
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

    /// Unfold whatever is hiding an agent's row — its project, its group — so
    /// the agent on screen always has a row in the panel to match.
    fn reveal_agent(&mut self, id: &AgentId) -> bool {
        for project in &mut self.state.projects {
            let Some(agent) = project.agents.iter().find(|agent| &agent.id == id) else {
                continue;
            };
            let group = agent
                .group
                .clone()
                .and_then(|name| project.group_index(&name));
            let mut changed = !project.expanded;
            project.expanded = true;
            if let Some(group) = group.map(|index| &mut project.groups[index]) {
                changed |= !group.expanded;
                group.expanded = true;
            }
            return changed;
        }
        false
    }

    fn select_agent(&mut self, id: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        self.inline_edit = None;
        if self.reveal_agent(&id) {
            self.persist(cx);
        }
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
        self.begin_inline_edit(EditTarget::Agent(id), "Agent name", name, window, cx);
    }

    /// Rename a group in place. Also how a group gets its *first* name: a new
    /// one is created unnamed and opens straight into this.
    fn start_group_edit(
        &mut self,
        project: usize,
        group: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self
            .state
            .projects
            .get(project)
            .and_then(|p| p.groups.get(group))
        else {
            return;
        };
        let name = record.name.clone();
        self.begin_inline_edit(
            EditTarget::Group { project, group },
            "Group name",
            name,
            window,
            cx,
        );
    }

    fn begin_inline_edit(
        &mut self,
        target: EditTarget,
        placeholder: &'static str,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(placeholder);
            state.set_value(value, window, cx);
            state
        });
        let committed = target.clone();
        let subscription = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
            // Escape is handled on the row itself: unlike the dialogs, an
            // inline edit has no overlay to own the key.
            if let InputEvent::PressEnter { .. } = event {
                let text = input.read(cx).value().trim().to_string();
                this.commit_inline_edit(&committed, &text, cx);
            }
        });
        window.focus(&input.focus_handle(cx));
        self.inline_edit = Some(InlineEdit {
            target,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_inline_edit(&mut self, target: &EditTarget, text: &str, cx: &mut Context<Self>) {
        match target {
            EditTarget::Agent(id) => {
                if let Some(record) = self.state.agent_mut(id) {
                    if !text.is_empty() {
                        record.name = text.to_string();
                    }
                }
            }
            EditTarget::Group { project, group } => {
                let Some(project) = self.state.projects.get_mut(*project) else {
                    return;
                };
                match project.groups.get(*group) {
                    // Nothing is ever named nothing: a group created and left
                    // blank disappears again, and clearing an existing name
                    // keeps the one it had rather than losing its rows.
                    Some(existing) if text.is_empty() => {
                        if existing.name.is_empty() {
                            project.remove_group(*group);
                        }
                    }
                    Some(_) => project.rename_group(*group, text),
                    None => return,
                }
            }
        }
        self.inline_edit = None;
        self.persist(cx);
        cx.notify();
    }

    /// Escape. A group that never got a name goes with it — it only existed
    /// because the rename was open.
    fn cancel_inline_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(InlineEdit {
            target: EditTarget::Group { project, group },
            ..
        }) = &self.inline_edit
        {
            let (project, group) = (*project, *group);
            if let Some(record) = self.state.projects.get_mut(project) {
                if record.groups.get(group).is_some_and(|g| g.name.is_empty()) {
                    record.remove_group(group);
                }
            }
        }
        self.inline_edit = None;
        cx.notify();
    }

    // ---- Groups ----

    /// Add a group to a project and open its name for editing straight away.
    /// Unnamed groups are never persisted: the rename either names it or
    /// takes it away again.
    fn add_group(&mut self, project_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.state.projects.get_mut(project_index) else {
            return;
        };
        project.groups.push(AgentGroup::new(String::new()));
        let group = project.groups.len() - 1;
        self.start_group_edit(project_index, group, window, cx);
    }

    fn toggle_group(&mut self, project_index: usize, group: usize, cx: &mut Context<Self>) {
        let Some(record) = self
            .state
            .projects
            .get_mut(project_index)
            .and_then(|p| p.groups.get_mut(group))
        else {
            return;
        };
        record.expanded = !record.expanded;
        self.persist(cx);
        cx.notify();
    }

    fn remove_group(&mut self, project_index: usize, group: usize, cx: &mut Context<Self>) {
        let Some(project) = self.state.projects.get_mut(project_index) else {
            return;
        };
        project.remove_group(group);
        // Indices behind it have shifted; an edit still pointing at one would
        // rename the wrong group.
        self.inline_edit = None;
        self.persist(cx);
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

        let project_order = {
            let (from, to) = match &self.drag_target {
                Some(DragTarget::Project { from, to }) => (Some(*from), Some(*to)),
                _ => (None, None),
            };
            Self::preview_order(self.state.projects.len(), from, to)
        };
        for project_index in project_order {
            let project = &self.state.projects[project_index];
            let expanded = project.expanded;
            let project_path = project.path.clone();

            let hovered_row = HoveredRow::Project(project_index);
            let is_hovered = self.hovered_row.as_ref() == Some(&hovered_row);

            list = list.child(
                Self::sidebar_row(("project", project_index))
                    .py_1()
                    .on_hover(cx.listener({
                        let row = hovered_row.clone();
                        move |this, hovered: &bool, _window, cx| {
                            this.set_hovered_row(row.clone(), *hovered, cx)
                        }
                    }))
                    .on_drag(ProjectDrag { index: project_index }, |_, _, _, cx| {
                        cx.new(|_| DragGhost)
                    })
                    .on_drag_move(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<ProjectDrag>, _, cx| {
                            // gpui delivers drag moves to every listener of
                            // this type, hovered or not, so the row has to
                            // check for itself — on the drag axis only.
                            if !Self::spans_row(event.bounds, event.event.position) {
                                return;
                            }
                            let from = event.drag(cx).index;
                            // Hovering the dragged row means "leave the
                            // preview as it is"; treating it as a target
                            // would flip the list back and forth.
                            if from == project_index {
                                return;
                            }
                            let pending = match &this.drag_target {
                                Some(DragTarget::Project { from, to }) => Some((*from, *to)),
                                _ => None,
                            };
                            let shown_before = Self::shown_before(
                                this.state.projects.len(),
                                pending,
                                project_index,
                                from,
                            );
                            let middle =
                                event.bounds.origin.y + event.bounds.size.height / 2.;
                            if !Self::past_middle(shown_before, middle, event.event.position.y) {
                                return;
                            }
                            this.set_drag_target(
                                DragTarget::Project {
                                    from,
                                    to: project_index,
                                },
                                cx,
                            );
                        },
                    ))
                    .on_drop(cx.listener(move |this, drag: &ProjectDrag, _, cx| {
                        if !this.apply_drag_target(cx) {
                            this.move_project(drag.index, project_index, cx);
                        }
                    }))
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
                    .child(Self::row_label(project.name.clone(), theme::fg()))
                    .when(is_hovered, |row| {
                        row.child(Self::row_action(
                            ("new-agent", project_index),
                            theme::accent(),
                            theme::accent(),
                            "+",
                            move |this, window, cx| {
                                this.open_new_agent_dialog(project_path.clone(), None, window, cx);
                            },
                            cx,
                        ))
                        .child(Self::row_action(
                            ("remove-project", project_index),
                            theme::fg_dim(),
                            theme::error(),
                            "×",
                            move |this, _window, cx| this.remove_project(project_index, cx),
                            cx,
                        ))
                    }),
            );

            if expanded {
                if project.agents.is_empty() && project.groups.is_empty() {
                    list = list.child(
                        div()
                            .pl_8()
                            .py_0p5()
                            .text_color(theme::fg_dim())
                            .text_xs()
                            .child("no agents"),
                    );
                }
                let rows = project.previewed_agents(self.pending_agent_move(project_index));
                // The ungrouped run comes first and carries no header:
                // ungrouped is the default state, and naming it would make
                // every project pay for a label it never asked for.
                let mut items: Vec<SidebarItem> = rows
                    .iter()
                    .filter(|(_, group)| group.is_none())
                    .map(|(index, _)| SidebarItem::Agent(*index))
                    .collect();
                for (group, record) in project.groups.iter().enumerate() {
                    let members: Vec<usize> = rows
                        .iter()
                        .filter(|(_, name)| name.as_deref() == Some(record.name.as_str()))
                        .map(|(index, _)| *index)
                        .collect();
                    items.push(SidebarItem::Header(group));
                    // A folded group keeps its header: it is still a drop
                    // target, and the only way back to what it holds.
                    if record.expanded {
                        items.extend(members.into_iter().map(SidebarItem::Agent));
                    }
                }
                items.push(SidebarItem::NewGroup);

                for item in items {
                    let agent_index = match item {
                        SidebarItem::Agent(index) => index,
                        SidebarItem::Header(group) => {
                            list = list
                                .child(self.render_group_header(project_index, group, cx));
                            continue;
                        }
                        SidebarItem::NewGroup => {
                            list = list.child(self.render_new_group_row(project_index, cx));
                            continue;
                        }
                    };
                    let agent = &project.agents[agent_index];
                    // Inline description editing initiated from the sidebar
                    // replaces the whole row with the input.
                    if let Some(edit) = self.inline_edit.as_ref().filter(|edit| {
                        edit.target == EditTarget::Agent(agent.id.clone())
                    }) {
                        list = list.child(
                            div()
                                .ml_4()
                                .my_0p5()
                                // Escape abandons the rename. A dialog gets
                                // this from its overlay; an inline edit has
                                // no overlay, so the row owns the key.
                                .on_key_down(cx.listener(
                                    |this, event: &gpui::KeyDownEvent, _window, cx| {
                                        if event.keystroke.key == "escape" {
                                            this.cancel_inline_edit(cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .child(Input::new(&edit.input)),
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
                    let hovered_row = HoveredRow::Agent(agent.id.clone());
                    let is_hovered = self.hovered_row.as_ref() == Some(&hovered_row);

                    list = list.child(
                        Self::sidebar_row(("agent", project_index * 1000 + agent_index))
                            .gap_2()
                            .ml_4()
                            .py_1()
                            .when(is_selected, |s| s.bg(theme::selected_bg()))
                            .on_drag(
                                AgentDrag {
                                    project: project_index,
                                    id: agent.id.clone(),
                                },
                                |_, _, _, cx| cx.new(|_| DragGhost),
                            )
                            .on_drag_move(cx.listener({
                                let target = agent.id.clone();
                                move |this, event: &gpui::DragMoveEvent<AgentDrag>, _, cx| {
                                    if !Self::spans_row(event.bounds, event.event.position) {
                                        return;
                                    }
                                    let drag = event.drag(cx);
                                    if drag.project != project_index {
                                        return;
                                    }
                                    let from = drag.id.clone();
                                    if from == target {
                                        return;
                                    }
                                    let Some(project) =
                                        this.state.projects.get(project_index)
                                    else {
                                        return;
                                    };
                                    // Positions in the *previewed* list, not
                                    // the stored one: the preview is what the
                                    // pointer is travelling over, and with
                                    // groups a pending move can have changed
                                    // which run the row is in.
                                    let rows = project.previewed_agents(
                                        this.pending_agent_move(project_index),
                                    );
                                    let shown = |id: &AgentId| {
                                        rows.iter().position(|(index, _)| {
                                            &project.agents[*index].id == id
                                        })
                                    };
                                    let (Some(hovered), Some(dragged)) =
                                        (shown(&target), shown(&from))
                                    else {
                                        return;
                                    };
                                    let middle =
                                        event.bounds.origin.y + event.bounds.size.height / 2.;
                                    if !Self::past_middle(
                                        hovered < dragged,
                                        middle,
                                        event.event.position.y,
                                    ) {
                                        return;
                                    }
                                    // The row adopts the group of whatever it
                                    // is displacing, so one gesture reorders
                                    // and regroups.
                                    let group = rows[hovered].1.clone();
                                    this.set_drag_target(
                                        DragTarget::Agent {
                                            project_index,
                                            from,
                                            to: Some(target.clone()),
                                            group,
                                        },
                                        cx,
                                    );
                                }
                            }))
                            .on_drop(cx.listener({
                                let target = agent.id.clone();
                                move |this, drag: &AgentDrag, _, cx| {
                                    if this.apply_drag_target(cx) {
                                        return;
                                    }
                                    if drag.project != project_index {
                                        return;
                                    }
                                    let group = this
                                        .state
                                        .projects
                                        .get(project_index)
                                        .and_then(|project| {
                                            let agent = project
                                                .agents
                                                .iter()
                                                .find(|agent| agent.id == target)?;
                                            project.group_of(agent).map(str::to_string)
                                        });
                                    this.move_agent(
                                        project_index,
                                        &drag.id,
                                        Some(&target),
                                        group,
                                        cx,
                                    );
                                }
                            }))
                            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                if event.click_count() >= 2 {
                                    this.start_inline_edit(dbl_id.clone(), window, cx);
                                } else {
                                    this.select_agent(id.clone(), window, cx);
                                }
                            }))
                            .on_hover(cx.listener({
                                let row = hovered_row.clone();
                                move |this, hovered: &bool, _window, cx| {
                                    this.set_hovered_row(row.clone(), *hovered, cx)
                                }
                            }))
                            .child(div().text_color(dot_color).text_xs().child("●"))
                            // Name only: the branch used to be shown on a
                            // second line, which doubled the row height for
                            // information the agent name already implies.
                            .child(Self::row_label(agent.name.clone(), theme::fg()))
                            .when(is_hovered, |row| {
                                row.child(Self::row_action(
                                    ("edit-agent", project_index * 1000 + agent_index),
                                    theme::fg_dim(),
                                    theme::accent(),
                                    Self::row_icon("icons/pencil.svg"),
                                    move |this, window, cx| {
                                        this.start_inline_edit(edit_id.clone(), window, cx)
                                    },
                                    cx,
                                ))
                                .child(Self::row_action(
                                    ("remove-agent", project_index * 1000 + agent_index),
                                    theme::fg_dim(),
                                    theme::error(),
                                    "×",
                                    move |this, _window, cx| {
                                        this.remove_agent(remove_id.clone(), cx)
                                    },
                                    cx,
                                ))
                            }),
                    );
                }
            }
        }

        // Adding a project is part of the list rather than a separate button:
        // the row lines up with the project headers, with a `+` where their
        // expand/collapse triangle goes.
        list = list.child(
            Self::sidebar_row("new-project")
                .py_1()
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
                .child(Self::row_label("New project", theme::fg_dim())),
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
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("toggle-log")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .text_sm()
                                    .cursor_pointer()
                                    .text_color(if self.state.settings.log_panel_open {
                                        theme::accent()
                                    } else {
                                        theme::fg_dim()
                                    })
                                    .hover(|s| s.bg(theme::hover_bg()).text_color(theme::fg()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_log_panel(cx);
                                    }))
                                    .child("Log"),
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
                    ),
            )
            .into_any_element()
    }

    /// A group header: fold triangle, label, a rule running out to the end
    /// of the row, and actions that appear on hover. The rule is the one from
    /// the sketch, earning its keep as an underline that carries on past the
    /// word rather than adding a second full-width division to a panel that
    /// already has one per project.
    fn render_group_header(
        &self,
        project_index: usize,
        group_index: usize,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let target = EditTarget::Group {
            project: project_index,
            group: group_index,
        };
        let key = project_index * 1000 + group_index;

        // Renaming replaces the header, the way it replaces an agent row.
        if let Some(edit) = self
            .inline_edit
            .as_ref()
            .filter(|edit| edit.target == target)
        {
            return div()
                .ml_4()
                .my_0p5()
                .on_key_down(cx.listener(
                    |this, event: &gpui::KeyDownEvent, _window, cx| {
                        if event.keystroke.key == "escape" {
                            this.cancel_inline_edit(cx);
                            cx.stop_propagation();
                        }
                    },
                ))
                .child(Input::new(&edit.input))
                .into_any_element();
        }

        let Some(project) = self.state.projects.get(project_index) else {
            return div().into_any_element();
        };
        let Some(group) = project.groups.get(group_index) else {
            return div().into_any_element();
        };
        let expanded = group.expanded;
        let name = group.name.clone();
        let project_path = project.path.clone();
        // At rest the header is just label and rule; the actions appear
        // under the pointer, and the rule gives up the width.
        let row = HoveredRow::Group {
            project: project_index,
            group: group_index,
        };
        let hovered = self.hovered_row.as_ref() == Some(&row);

        Self::sidebar_row(("group", key))
            .ml_4()
            .py_0p5()
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                this.set_hovered_row(row.clone(), *hovered, cx)
            }))
            // A header is an unambiguous target — there is no "which half"
            // to resolve — so it claims the drag as soon as it is hovered.
            .on_drag_move(cx.listener({
                let name = name.clone();
                move |this, event: &gpui::DragMoveEvent<AgentDrag>, _, cx| {
                    if !Self::spans_row(event.bounds, event.event.position) {
                        return;
                    }
                    let drag = event.drag(cx);
                    if drag.project != project_index {
                        return;
                    }
                    this.set_drag_target(
                        DragTarget::Agent {
                            project_index,
                            from: drag.id.clone(),
                            to: None,
                            group: Some(name.clone()),
                        },
                        cx,
                    );
                }
            }))
            .on_drop(cx.listener({
                let name = name.clone();
                move |this, drag: &AgentDrag, _, cx| {
                    if this.apply_drag_target(cx) {
                        return;
                    }
                    if drag.project == project_index {
                        this.move_agent(project_index, &drag.id, None, Some(name.clone()), cx);
                    }
                }
            }))
            .on_click(cx.listener(
                move |this, event: &gpui::ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        this.start_group_edit(project_index, group_index, window, cx);
                    } else {
                        this.toggle_group(project_index, group_index, cx);
                    }
                },
            ))
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .text_color(theme::fg_dim())
                    .text_xs()
                    .child(if expanded { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(theme::fg_dim())
                    .text_size(theme::label_font_size(cx))
                    .whitespace_nowrap()
                    // Uppercase so it reads as a label rather than as another
                    // row. gpui has no letter-spacing, which is what a label
                    // this small would otherwise want.
                    .child(SharedString::from(name.to_uppercase())),
            )
            // On the label's baseline, not through its middle, so it reads
            // as an underline running off the end of the word. The row centres
            // its children, and centring includes the margin box — so a top
            // margin of 2n drops the line by n. (`items_baseline` lands near
            // the x-height instead, and inset is ignored for a flex child.)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .mt(px(f32::from(theme::label_font_size(cx)) * 0.6))
                    .h(px(1.))
                    .bg(theme::rule()),
            )
            // Only under the pointer, and taking width only then, so at rest
            // the rule runs the full width of the header instead of stopping
            // short at space held for something invisible.
            .when(hovered, |row| {
                row.child(
                    Self::row_action(
                        ("new-group-agent", key),
                        theme::accent(),
                        theme::accent(),
                        "+",
                        {
                            let name = name.clone();
                            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                                this.open_new_agent_dialog(
                                    project_path.clone(),
                                    Some(name.clone()),
                                    window,
                                    cx,
                                );
                            }
                        },
                        cx,
                    )
                    // A header's actions sit on the smaller type its label
                    // uses, so they don't outweigh the group they name.
                    .text_xs(),
                )
                .child(
                    Self::row_action(
                        ("remove-group", key),
                        theme::fg_dim(),
                        theme::error(),
                        "×",
                        move |this, _window, cx| this.remove_group(project_index, group_index, cx),
                        cx,
                    )
                    .text_xs(),
                )
            })
            .into_any_element()
    }

    /// Creating a group is a row in the list rather than another button on
    /// the project header — the same call "New project" makes at the bottom
    /// of the panel. It lines up with the headers it creates, with a `+`
    /// where their fold triangle goes.
    fn render_new_group_row(&self, project_index: usize, cx: &Context<Self>) -> gpui::AnyElement {
        Self::sidebar_row(("new-group", project_index))
            .ml_4()
            .py_0p5()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.add_group(project_index, window, cx);
            }))
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .text_color(theme::accent())
                    .text_xs()
                    .child("+"),
            )
            .child(
                div()
                    .text_color(theme::fg_dim())
                    .text_size(theme::label_font_size(cx))
                    .whitespace_nowrap()
                    .child("NEW GROUP"),
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
             shell_tab: bool,
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

                if shell_tab {
                    // Only shell tabs move; the agent tab is the session
                    // itself and stays first.
                    let drag_agent = agent_id.clone();
                    let drag_id = id.to_string();
                    let drop_agent = agent_id.clone();
                    let drop_id = id.to_string();
                    let move_agent = agent_id.clone();
                    let move_id = id.to_string();
                    tab = tab
                        .on_drag(
                            TabDrag {
                                agent: drag_agent,
                                id: drag_id,
                            },
                            |_, _, _, cx| cx.new(|_| DragGhost),
                        )
                        .on_drag_move(cx.listener(
                            move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                                if !Self::spans_column(event.bounds, event.event.position) {
                                    return;
                                }
                                let drag = event.drag(cx);
                                if drag.agent != move_agent {
                                    return;
                                }
                                if drag.id == move_id {
                                    return;
                                }
                                let Some(record) = this.state.agent(&move_agent) else {
                                    return;
                                };
                                let index = |id: &str| {
                                    record.terminals.iter().position(|tab| tab.id == id)
                                };
                                let (Some(hovered), Some(dragged)) =
                                    (index(&move_id), index(&drag.id))
                                else {
                                    return;
                                };
                                let pending = match &this.drag_target {
                                    Some(DragTarget::Tab { from, to, .. }) => {
                                        index(from).zip(index(to))
                                    }
                                    _ => None,
                                };
                                let shown_before = Self::shown_before(
                                    record.terminals.len(),
                                    pending,
                                    hovered,
                                    dragged,
                                );
                                let middle = event.bounds.origin.x + event.bounds.size.width / 2.;
                                if !Self::past_middle(shown_before, middle, event.event.position.x)
                                {
                                    return;
                                }
                                this.set_drag_target(
                                    DragTarget::Tab {
                                        agent: move_agent.clone(),
                                        from: drag.id.clone(),
                                        to: move_id.clone(),
                                    },
                                    cx,
                                );
                            },
                        ))
                        .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                            if this.apply_drag_target(cx) {
                                return;
                            }
                            if drag.agent == drop_agent {
                                this.move_tab(&drop_agent, &drag.id, &drop_id, cx);
                            }
                        }));
                    tab = tab.child(
                        div()
                            .id(SharedString::from(format!("close-{tab_id_for_close}")))
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

        // Extra terminal tabs, in the order a hovering drag would leave them.
        let tab_order = {
            let (from, to) = match &self.drag_target {
                Some(DragTarget::Tab {
                    agent: dragged_agent,
                    from,
                    to,
                }) if *dragged_agent == agent.id => (
                    agent.terminals.iter().position(|tab| &tab.id == from),
                    agent.terminals.iter().position(|tab| &tab.id == to),
                ),
                _ => (None, None),
            };
            Self::preview_order(agent.terminals.len(), from, to)
        };
        for tab_index in tab_order {
            let tab_record = &agent.terminals[tab_index];
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
                    this.add_next_shell_tab(add_agent_id.clone(), window, cx);
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

        if self.state.settings.log_panel_open {
            pane = pane.child(self.render_log_panel(cx));
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

    /// Picks which agent preset the planner runs through. Reads the preset
    /// *rows* rather than the saved list, so a name edited in this same visit
    /// is what you choose from.
    fn render_planner_preset_row(
        &self,
        preset_inputs: &[PresetInputs],
        selected: Option<usize>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let chip = |label: SharedString, row: Option<usize>, cx: &Context<Self>| {
            let is_selected = row == selected;
            div()
                .id(("planner-preset", row.map(|r| r + 1).unwrap_or(0)))
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
                // A long preset name must not widen the dialog past the
                // window: the chip is capped and its label ellipsized.
                .max_w_full()
                .truncate()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(Dialog::Settings {
                        planner_preset_row, ..
                    }) = &mut this.dialog
                    {
                        *planner_preset_row = row;
                    }
                    cx.notify();
                }))
                .child(label)
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w_24()
                    .flex_none()
                    .text_color(theme::fg_dim())
                    .child("Preset"),
            )
            .child(chip("Default (claude)".into(), None, cx));
        for (index, inputs) in preset_inputs.iter().enumerate() {
            let name = inputs.name.read(cx).value().trim().to_string();
            let command = inputs.command.read(cx).value().trim().to_string();
            if name.is_empty() && command.is_empty() {
                continue;
            }
            let label = if name.is_empty() { command } else { name };
            row = row.child(chip(ellipsize(&label, CHIP_LABEL_CHARS).into(), Some(index), cx));
        }
        row.into_any_element()
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

    /// The body of the open dialog. The surface around it — backdrop,
    /// panel, escape and the focus trap — belongs to gpui-component's
    /// dialog layer, which re-runs this on every frame, so the content can
    /// read live state (planning in progress, an error, a chip selection).
    fn render_dialog_content(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.dialog.as_ref()?;

        let panel = match dialog {
            Dialog::NewAgent {
                input,
                planning,
                preset,
                workspace_mode,
                error,
                ..
            } => {
                let mut panel = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme::fg())
                            .child("New agent — describe the task"),
                    )
                    .child(Input::new(&input));

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
                            // Long preset names must not widen the dialog.
                            .max_w_full()
                            .truncate()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Dialog::NewAgent { preset, .. }) = &mut this.dialog {
                                    *preset = index;
                                }
                                cx.notify();
                            }))
                            .child(SharedString::from(ellipsize(
                                &preset_record.name,
                                CHIP_LABEL_CHARS,
                            ))),
                    );
                }
                panel = panel.child(chips);

                // Workspace selector. The task description still names the
                // agent in every mode; this only picks where it works.
                let selected_mode = *workspace_mode;
                let mut modes = div().flex().flex_wrap().items_center().gap_1().child(
                    div()
                        .text_color(theme::fg_dim())
                        .text_sm()
                        .child("Workspace:"),
                );
                for (index, mode) in WorkspaceMode::ALL.into_iter().enumerate() {
                    let is_selected = mode == selected_mode;
                    modes = modes.child(
                        div()
                            .id(("workspace-mode-chip", index))
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
                                if let Some(Dialog::NewAgent { workspace_mode, .. }) =
                                    &mut this.dialog
                                {
                                    *workspace_mode = mode;
                                }
                                cx.notify();
                            }))
                            .child(mode.label()),
                    );
                }
                panel = panel.child(modes);

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

            Dialog::Search {
                input,
                match_case,
                wrap,
                status,
                ..
            } => {
                // One toggle chip, styled like the preset/workspace chips.
                let toggle = |id: &'static str, label: &'static str, on: bool| {
                    div()
                        .id(id)
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .text_sm()
                        .cursor_pointer()
                        .bg(if on {
                            theme::accent()
                        } else {
                            theme::selected_bg()
                        })
                        .text_color(if on { theme::panel_bg() } else { theme::fg() })
                        .hover(|s| s.opacity(0.85))
                        .child(label)
                };
                let mut panel = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(theme::fg()).child("Find in terminal"))
                    .child(Input::new(&input))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .child(
                                toggle("search-match-case", "Match case", *match_case).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if let Some(Dialog::Search { match_case, .. }) =
                                            &mut this.dialog
                                        {
                                            *match_case = !*match_case;
                                        }
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(
                                toggle("search-wrap", "Wrap around", *wrap).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if let Some(Dialog::Search { wrap, .. }) = &mut this.dialog {
                                            *wrap = !*wrap;
                                        }
                                        cx.notify();
                                    },
                                )),
                            ),
                    );

                if let Some(status) = status {
                    panel = panel.child(
                        div()
                            .text_color(theme::fg_dim())
                            .text_sm()
                            .child(SharedString::from(status.clone())),
                    );
                }

                let button = |id: &'static str, label: &'static str, primary: bool| {
                    div()
                        .id(id)
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(if primary {
                            theme::accent()
                        } else {
                            theme::selected_bg()
                        })
                        .text_color(if primary { theme::panel_bg() } else { theme::fg() })
                        .hover(|s| s.opacity(0.9))
                        .child(label)
                };
                panel
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("search-close")
                                    .px_3()
                                    .py_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_color(theme::fg_dim())
                                    .hover(|s| s.text_color(theme::fg()))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_dialog(window, cx);
                                    }))
                                    .child("Close"),
                            )
                            .child(button("search-prev", "Previous", false).on_click(
                                cx.listener(|this, _, _, cx| this.find_in_terminal(false, cx)),
                            ))
                            .child(button("search-next", "Next", true).on_click(cx.listener(
                                |this, _, _, cx| this.find_in_terminal(true, cx),
                            ))),
                    )
                    .into_any_element()
            }

            Dialog::Settings {
                focus_handle,
                planner_command,
                planner_model,
                terminal_font,
                ui_font,
                preset_inputs,
                planner_preset_row,
                ..
            } => {
                let section = |text: &'static str| {
                    div()
                        .text_color(theme::fg())
                        .text_sm()
                        .mt_2()
                        .child(text)
                };
                let field = |text: &'static str, input: Entity<InputState>| {
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
                        .child(div().flex_1().min_w_0().child(Input::new(&input)))
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
                                    .child(div().flex_1().min_w_0().child(Input::new(&inputs.name)))
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
                                    .child(div().flex_1().min_w_0().child(Input::new(&inputs.command))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(label("Env"))
                                    .child(div().flex_1().min_w_0().child(Input::new(&inputs.env))),
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
                    .max_h_full()
                    .child(div().text_color(theme::fg()).child("Settings"))
                    .child(
                        div()
                            .id("settings-body")
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .overflow_x_hidden()
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
                            .child(self.render_planner_preset_row(
                                preset_inputs,
                                *planner_preset_row,
                                cx,
                            ))
                            .child(field("Command", planner_command.clone()))
                            .child(field("Model", planner_model.clone()))
                            .when(
                                !planner_command.read(cx).value().trim().is_empty(),
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
                                    .child("The task description is appended to the command. Env holds KEY=value words given to the agent, its resumes and its shell tabs. Use claude-isol for sandboxed runs (github.com/tehrengruber/claude-container-isolation)."),
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

        Some(panel)
    }

    /// Hand the open dialog to the dialog layer. `Root` owns the backdrop,
    /// the focus trap and escape from here on; closing it there has to clear
    /// our own `dialog` state, hence the `on_close` hook.
    fn present_dialog(&mut self, width: gpui::Pixels, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            // `update` rather than `read`: the content wires up listeners,
            // which need our own context.
            let content = this.update(cx, |this, cx| this.render_dialog_content(cx));
            let dialog = dialog
                .w(width)
                // Never wider than the window; a long preset name used to
                // push the buttons off the edge.
                .max_w(px(920.))
                .close_button(false)
                .on_close({
                    let this = this.clone();
                    // Escape, the backdrop and the close button all end here,
                    // so this is where our own dialog state is cleared.
                    move |_, _window, cx| {
                        this.update(cx, |this, cx| {
                            this.dialog = None;
                            cx.notify();
                        });
                    }
                });
            match content {
                Some(content) => dialog.child(content),
                None => dialog,
            }
        });
        cx.notify();
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
                    let task = input.read(cx).value().trim().to_string();
                    if !task.is_empty() {
                        self.spawn_agent(project_path.clone(), task, window, cx);
                    }
                }
            }
            Some(Dialog::Search { .. }) => self.find_in_terminal(true, cx),
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
    is_var_name(name).then(|| (name.to_string(), value.to_string()))
}

/// Parse a preset's environment field — shell-style `KEY=value` words, quoted
/// like a command line so a value may contain spaces. Values are expanded
/// against `vars` and against everything assigned earlier in the same field,
/// so one entry can build on another.
///
/// Words that aren't assignments are dropped with a log line: the field has no
/// other meaning, and silently exec'ing something because of a stray word would
/// be worse than ignoring it.
fn parse_env(text: &str, vars: &[(String, String)]) -> Vec<(String, String)> {
    let mut parsed: Vec<(String, String)> = Vec::new();
    for word in planner::split_command(text) {
        match split_assignment(&word) {
            Some((name, value)) => {
                let mut scope = vars.to_vec();
                scope.extend(parsed.iter().cloned());
                parsed.push((name, expand_vars(&value, &scope)));
            }
            None => log::error(format!(
                "preset environment: ignoring `{word}`, expected KEY=value"
            )),
        }
    }
    parsed
}

/// How much of a preset name a chip shows before eliding the rest.
const CHIP_LABEL_CHARS: usize = 40;

/// Shorten a label for a chip. Chips are pickers, not displays: a preset with
/// a 200-character name would otherwise widen the row past the dialog, and
/// layout-level truncation needs a resolved width that a wrapping chip row
/// doesn't give it.
fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Environment harmonium provides for one agent's processes: the agent
/// session, its resumes, and its shell tabs. Preset commands can reference
/// these with `$VAR` (see `expand_vars`), which is how an isolation preset
/// learns what to mount: a worktree's `.git` is a *file* pointing into the
/// main repository's `.git/worktrees/`, so a container that sees only the
/// workdir has no working git.
fn task_env(project_path: &Path, workdir: &Path, branch: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "HARMONIUM_TASK_GIT_ROOT".to_string(),
            project_path.to_string_lossy().into_owned(),
        ),
        (
            "HARMONIUM_TASK_WORKDIR".to_string(),
            workdir.to_string_lossy().into_owned(),
        ),
    ];
    // Deliberately absent rather than empty when the agent works on whatever
    // the base checkout has out: `${HARMONIUM_TASK_BRANCH}` then stays
    // literal, which is visible, instead of silently vanishing.
    if let Some(branch) = branch {
        env.push(("HARMONIUM_TASK_BRANCH".to_string(), branch.to_string()));
    }
    env
}

/// The task variables that exist before there is a task: the project is
/// known, a worktree is not. `WORKDIR` is the repository itself, which is
/// where the planner runs, so a preset that mounts either still resolves.
fn planner_task_env(repo: &Path) -> Vec<(String, String)> {
    task_env(repo, repo, None)
}

/// A planner preset's command, split and `$VAR`-expanded the same way an agent
/// spawn would do it. Without this a mounted preset reaches the wrapper with a
/// literal `$HARMONIUM_TASK_GIT_ROOT`, which it rejects as an invalid path.
pub fn planner_argv(preset: &PresetRecord, model: &str, repo: &Path) -> Vec<String> {
    let vars = planner_task_env(repo);
    planner::split_command(&preset.planner_command(model))
        .iter()
        .map(|word| expand_vars(word, &vars))
        .collect()
}

/// Expand `$NAME` and `${NAME}` in one command word against `vars`, falling
/// back to the process environment. Commands are exec'd without a shell, so
/// without this a preset's `-v $HARMONIUM_TASK_GIT_ROOT:/repo` would reach
/// the program literally.
///
/// Unknown names are left as written rather than expanded to nothing: an
/// empty expansion would turn a typo into a plausible-looking `-v :/repo`,
/// whereas the literal text points straight at the mistake. `$$` is a
/// literal `$`.
fn expand_vars(word: &str, vars: &[(String, String)]) -> String {
    let lookup = |name: &str| -> Option<String> {
        if !is_var_name(name) {
            return None;
        }
        vars.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    };
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                match lookup(&name).filter(|_| closed) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push_str("${");
                        out.push_str(&name);
                        if closed {
                            out.push('}');
                        }
                    }
                }
            }
            _ => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match lookup(&name) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('$');
                        out.push_str(&name);
                    }
                }
            }
        }
    }
    out
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
        // A release outside the list never reaches a drop handler, but the
        // preview showed where the item would land, so commit it rather than
        // snapping back.
        if self.drag_target.is_some() && !cx.has_active_drag() {
            self.apply_drag_target(cx);
        }

        // Scale rem-based sizes (text_sm/text_xs etc.) with the base font
        // size; at the default size this yields the standard 16px rem.
        window.set_rem_size(theme::rem_size(cx));

        let mut root = div()
            .id("workspace")
            // Key bindings with a context predicate only match against a
            // non-empty context stack, so the root names itself — this is what
            // makes `!TextInput` (ctrl-shift-t) resolvable at all.
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .size_full()
            .bg(theme::bg())
            .text_size(theme::ui_font_size(cx))
            .font_family(theme::ui_font().family.clone())
            .on_action(cx.listener(Self::new_terminal_tab))
            .on_action(cx.listener(Self::search_terminal))
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

        root
    }
}

/// The window's root view: the workspace, with gpui-component's dialog layer
/// over it.
///
/// `Root` owns the dialog state but renders only the view it is handed, so
/// placing the layers is the app's job — and they cannot be placed inside
/// [`Workspace::render`], because drawing a dialog calls back into the
/// workspace to build its content and gpui refuses to update an entity that
/// is already being updated. Hence a view of its own in between: it is being
/// updated while the layer draws, and the workspace is not.
pub struct WindowRoot {
    workspace: Entity<Workspace>,
}

impl WindowRoot {
    pub fn new(workspace: Entity<Workspace>) -> Self {
        Self { workspace }
    }
}

impl Render for WindowRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(self.workspace.clone())
            .children(gpui_component::Root::render_dialog_layer(window, cx).map(|layer| {
                // Tagged so a test can assert the layer is actually placed
                // here. `debug_selector` compiles away outside tests, and the
                // wrapper takes no space of its own — the layer inside it
                // positions itself against the root above.
                div().debug_selector(|| "dialog-layer".into()).child(layer)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vec<(String, String)> {
        task_env(
            Path::new("/repos/harmonium"),
            Path::new("/data/worktrees/fix-login"),
            Some("fix-login"),
        )
    }

    #[test]
    fn task_env_vars() {
        assert_eq!(
            vars(),
            vec![
                (
                    "HARMONIUM_TASK_GIT_ROOT".to_string(),
                    "/repos/harmonium".to_string()
                ),
                (
                    "HARMONIUM_TASK_WORKDIR".to_string(),
                    "/data/worktrees/fix-login".to_string()
                ),
                ("HARMONIUM_TASK_BRANCH".to_string(), "fix-login".to_string()),
            ]
        );
        // No branch: the variable is absent, not empty.
        let root = Path::new("/repos/harmonium");
        let base = task_env(root, root, None);
        assert!(base.iter().all(|(k, _)| k != "HARMONIUM_TASK_BRANCH"));
    }

    #[test]
    fn expands_task_vars() {
        let vars = vars();
        assert_eq!(
            expand_vars("$HARMONIUM_TASK_GIT_ROOT:/repo", &vars),
            "/repos/harmonium:/repo"
        );
        assert_eq!(
            expand_vars("${HARMONIUM_TASK_WORKDIR}/sub", &vars),
            "/data/worktrees/fix-login/sub"
        );
        assert_eq!(expand_vars("no-vars-here", &vars), "no-vars-here");
    }

    #[test]
    fn expansion_falls_back_to_process_env() {
        // SAFETY: single-threaded test process; no other thread reads the env.
        unsafe { std::env::set_var("HARMONIUM_TEST_EXPAND", "from-process") };
        assert_eq!(
            expand_vars("$HARMONIUM_TEST_EXPAND", &vars()),
            "from-process"
        );
        // Task vars win over the process environment.
        unsafe { std::env::set_var("HARMONIUM_TASK_GIT_ROOT", "/wrong") };
        assert_eq!(
            expand_vars("$HARMONIUM_TASK_GIT_ROOT", &vars()),
            "/repos/harmonium"
        );
        unsafe { std::env::remove_var("HARMONIUM_TASK_GIT_ROOT") };
        unsafe { std::env::remove_var("HARMONIUM_TEST_EXPAND") };
    }

    #[test]
    fn unknown_vars_stay_literal() {
        let vars = vars();
        assert_eq!(
            expand_vars("$HARMONIUM_TASK_NOPE:/repo", &vars),
            "$HARMONIUM_TASK_NOPE:/repo"
        );
        assert_eq!(expand_vars("${NOPE_UNSET}", &vars), "${NOPE_UNSET}");
        // Unterminated brace and a bare `$` are left alone too.
        assert_eq!(
            expand_vars("${HARMONIUM_TASK_BRANCH", &vars),
            "${HARMONIUM_TASK_BRANCH"
        );
        assert_eq!(expand_vars("cost: 5$", &vars), "cost: 5$");
        assert_eq!(expand_vars("$$HOME", &vars), "$HOME");
    }

    #[test]
    fn preset_env_is_parsed_and_expanded() {
        let vars = vars();
        assert_eq!(
            parse_env(
                "FOO=bar CLAUDE_CONFIG_DIR=$HARMONIUM_TASK_WORKDIR/.claude",
                &vars
            ),
            vec![
                ("FOO".to_string(), "bar".to_string()),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    "/data/worktrees/fix-login/.claude".to_string()
                ),
            ]
        );
        // Quoted values keep their spaces, and later entries see earlier ones.
        assert_eq!(
            parse_env("GREETING=\"hello there\" LOUD=$GREETING!", &vars),
            vec![
                ("GREETING".to_string(), "hello there".to_string()),
                ("LOUD".to_string(), "hello there!".to_string()),
            ]
        );
        // Non-assignments are dropped rather than silently becoming something.
        assert_eq!(parse_env("claude --continue", &vars), Vec::new());
        assert_eq!(parse_env("", &vars), Vec::new());
    }

    #[test]
    fn chip_labels_are_shortened_not_wide() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("exactly-10", 10), "exactly-10");
        assert_eq!(ellipsize("a-very-long-preset-name", 10), "a-very-lo…");
        // Multi-byte input must not be split mid-character.
        assert_eq!(ellipsize("äöüßäöüßäöüß", 5), "äöüß…");
        // A trailing space before the ellipsis reads as a typo.
        assert_eq!(ellipsize("word morewords", 6), "word…");
    }

    #[test]
    fn assignments_are_env_names_only() {
        assert_eq!(
            split_assignment("FOO=bar"),
            Some(("FOO".to_string(), "bar".to_string()))
        );
        assert_eq!(split_assignment("/usr/bin/a=b"), None);
        assert_eq!(split_assignment("2FOO=bar"), None);
        assert_eq!(split_assignment("claude"), None);
    }

    /// A window shaped like the real one: our workspace inside the `Root`
    /// that owns the dialog layer. The tests below only open and close
    /// dialogs, so nothing is saved — but a workspace owns a session file
    /// either way, and this gives each test one of its own under the
    /// temporary directory `data_dir()` resolves to in a test process. Two
    /// tests sharing one would be two harmoniums, and the second would find
    /// the first holding the lock.
    fn test_window(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Workspace>, &mut gpui::VisualTestContext) {
        cx.update(|cx| gpui_component::init(cx));
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let session = state::data_dir()
            .join(format!(
                "window-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))
            .join("state.json");
        let (state_file, state) =
            StateFile::load_from(session).expect("a state file of this test's own");
        let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
        let keep = holder.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(state, state_file, cx));
            *keep.borrow_mut() = Some(workspace.clone());
            // Built the same way the real window is, `WindowRoot` and all:
            // the dialog layer is placed there, so a test window without it
            // would pass on dialogs the app never draws.
            let root = cx.new(|_| WindowRoot::new(workspace));
            gpui_component::Root::new(gpui::AnyView::from(root), window, cx)
        });
        let workspace = holder.borrow().clone().expect("workspace built");
        (workspace, cx)
    }

    /// Opening a dialog has to reach the dialog layer, not just our own
    /// state: the panel is rendered by `Root` now, so a dialog that only
    /// existed in `self.dialog` would never appear.
    #[gpui::test]
    fn opening_a_dialog_puts_it_in_the_dialog_layer(cx: &mut gpui::TestAppContext) {
        let (workspace, cx) = test_window(cx);

        cx.update(|window, cx| {
            workspace.update(cx, |this, cx| this.open_settings_dialog(window, cx))
        });
        assert!(workspace.read_with(cx, |this, _| this.dialog.is_some()));
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
        // Holding the state is not the point: `Root` draws only the view it
        // is given, so a dialog whose layer nobody renders takes the keyboard
        // and stays invisible — indistinguishable from a button that does
        // nothing. Assert the layer reached the screen, not just the state.
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dialog-layer").is_some(),
            "the dialog layer was never drawn"
        );

        cx.update(|window, cx| workspace.update(cx, |this, cx| this.close_dialog(window, cx)));
        assert!(workspace.read_with(cx, |this, _| this.dialog.is_none()));
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    }

    /// Opening the dialog layer focuses the dialog itself, so a field
    /// focused before presenting silently loses the keyboard. The field the
    /// user is meant to type into has to end up focused.
    #[gpui::test]
    fn opening_a_dialog_focuses_its_first_field(cx: &mut gpui::TestAppContext) {
        let (workspace, cx) = test_window(cx);

        cx.update(|window, cx| {
            workspace.update(cx, |this, cx| this.open_settings_dialog(window, cx))
        });

        let wanted = workspace.read_with(cx, |this, cx| match &this.dialog {
            Some(Dialog::Settings {
                preset_inputs,
                focus_handle,
                ..
            }) => preset_inputs
                .first()
                .map(|first| first.name.focus_handle(cx))
                .unwrap_or_else(|| focus_handle.clone()),
            _ => panic!("settings dialog did not open"),
        });
        assert!(
            cx.update(|window, _| wanted.is_focused(window)),
            "the dialog took the keyboard off its own field"
        );
    }

    fn agent(name: &str, group: Option<&str>) -> AgentRecord {
        AgentRecord {
            id: name.to_string(),
            name: name.to_string(),
            description: String::new(),
            workdir: PathBuf::from("/repo"),
            branch: None,
            command: None,
            resume_command: None,
            env: None,
            terminals: Vec::new(),
            group: group.map(str::to_string),
        }
    }

    /// The sidebar draws groups from indices into two lists, so a mismatch
    /// between them is a panic rather than a wrong pixel — this renders a
    /// project with a folded group, an unfolded one and an empty one. It
    /// then checks the rule that makes folding safe: an agent that becomes
    /// the selection has its group opened, so what is on screen always has a
    /// row in the panel.
    #[gpui::test]
    fn a_folded_group_still_gives_up_its_agent_when_selected(cx: &mut gpui::TestAppContext) {
        let (workspace, cx) = test_window(cx);

        workspace.update(cx, |this, cx| {
            let mut project = ProjectRecord::new(PathBuf::from("/repo"));
            project.groups = vec![
                AgentGroup::new("review"),
                AgentGroup {
                    name: "later".into(),
                    expanded: false,
                },
                AgentGroup::new("empty"),
            ];
            project.agents = vec![
                agent("mine", None),
                agent("a-review", Some("review")),
                agent("hidden", Some("later")),
                // Membership no group answers for: an extra row in the
                // ungrouped run, never a row that isn't drawn at all.
                agent("orphan", Some("gone")),
            ];
            project.normalize_agent_order();
            this.state.projects = vec![project];
            cx.notify();
        });
        cx.run_until_parked();

        workspace.update(cx, |this, _| {
            assert!(
                this.reveal_agent(&"hidden".to_string()),
                "an agent inside a folded group had nothing to unfold"
            );
            assert!(this.state.projects[0].groups[1].expanded);
            // Already visible: nothing to change, and nothing to persist.
            assert!(!this.reveal_agent(&"mine".to_string()));
        });
    }

    /// Reopening the search dialog reuses the open one rather than stacking
    /// a second panel on top of it.
    #[gpui::test]
    fn reopening_search_does_not_stack_dialogs(cx: &mut gpui::TestAppContext) {
        let (workspace, cx) = test_window(cx);

        cx.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_search_dialog(window, cx);
                this.open_search_dialog(window, cx);
            })
        });

        // Without a terminal on screen there is nothing to search, so the
        // dialog must not have opened at all.
        assert!(workspace.read_with(cx, |this, _| this.dialog.is_none()));
        assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
