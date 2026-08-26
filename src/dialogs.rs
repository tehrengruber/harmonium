//! The modal dialogs: each one is a gpui view that owns its state and renders
//! its own body.
//!
//! gpui-component's `Root` is the *only* owner of "a dialog is open" — its
//! dialog layer holds the sole strong handle to the open view, so there is no
//! second record anywhere that could fall out of sync with what is on screen.
//! The workspace hands a view to the layer, subscribes to the events it emits
//! (spawn / save / search / cancel), and does the work that touches app state
//! in those handlers; the dialog never reaches back into the workspace, so
//! drawing the layer (see `WindowRoot`) re-runs nothing of the workspace's.

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, App, AppContext as _, Context, Div, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::planner;
use crate::state::{PresetRecord, SettingsRecord, WorkspaceMode};
use crate::theme;
use crate::ui;

/// Rows the task field grows to before it scrolls instead. Without a cap a
/// long description pushes the Spawn button off the bottom of the window.
const MAX_TASK_ROWS: usize = 12;

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

/// One selector chip, styled the same in every dialog. Drawn on the shared
/// control metric (see [`crate::ui`]), so it is the same size as the field
/// above it and says what it is with colour alone.
fn chip(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    ui::control(id)
        .bg(if selected {
            theme::accent()
        } else {
            theme::selected_bg()
        })
        .border_color(if selected {
            theme::accent()
        } else {
            theme::border()
        })
        .text_color(if selected {
            theme::panel_bg()
        } else {
            theme::fg()
        })
        .hover(|s| s.opacity(0.85))
}

/// How much of the window's height a dialog may cover. gpui-component drops
/// its shell a tenth of the window down (`Dialog`'s default `margin_top`,
/// dialog.rs:367), and this leaves about as much free underneath.
const DIALOG_WINDOW_FRACTION: f32 = 0.8;

/// What the shell adds around a dialog's own frame: 24px of padding above and
/// below (`Edges::all(px(24.))`, dialog.rs:373), its 1px border, and a little
/// slack. Subtracted here because the frame has to measure itself against the
/// *window*: nothing between the two ever passes a height down. The shell's
/// panel is a plain `v_flex` with no height and no maximum of its own
/// (dialog.rs:423-475), and gpui's `AnyView` contributes no layout node at all
/// — its `request_layout` returns the child's own layout id (view.rs:179) — so
/// a view that renders taller than the window simply hangs off the bottom of
/// it, which is exactly what the settings dialog used to do.
const DIALOG_SHELL_CHROME: Pixels = px(56.);

/// Smallest frame worth showing, for a window too short for the fraction
/// above to leave anything usable.
const MIN_DIALOG_HEIGHT: Pixels = px(160.);

/// The height the dialog shell is capped at, applied where a dialog is
/// presented. Each dialog's own [`dialog_frame`] already keeps itself below
/// this, so the cap only ever catches a body that measured itself wrong —
/// and then the shell's own scroll area takes over rather than the panel
/// growing off the screen.
pub fn max_shell_height(window: &Window) -> Pixels {
    (window.viewport_size().height * DIALOG_WINDOW_FRACTION).max(MIN_DIALOG_HEIGHT)
}

/// A dialog's outer frame: a column no taller than the window can show, with
/// the title above and the buttons below whatever [`dialog_body`] holds. The
/// cap is what makes the body's `flex_1` mean something — an uncapped column
/// grows to its content and nothing ever scrolls.
fn dialog_frame(window: &Window) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .max_h((max_shell_height(window) - DIALOG_SHELL_CHROME).max(MIN_DIALOG_HEIGHT))
}

/// The scrolling middle of a dialog: everything that may grow without bound —
/// a long task description, twenty presets, font size 24 — goes in here, so
/// the title and the buttons around it stay put.
///
/// `flex_1` alone would not scroll: a flex child's automatic minimum height is
/// its content, so it refuses to shrink below it however small the frame is.
/// `min_h_0` is what allows the box to be shorter than what's in it, which is
/// the point at which the overflow becomes scrollable.
fn dialog_body(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_2()
        .flex_1()
        .min_h_0()
        // Wrapping rows (chips) and inputs size themselves against this, and
        // a child that refuses to shrink would scroll sideways instead.
        .min_w_0()
        .overflow_x_hidden()
        .overflow_y_scroll()
}

/// The Cancel/submit pair at the foot of a dialog. The handlers emit the
/// dialog's own events — what submitting *means* is the subscriber's business.
fn dialog_buttons<V: 'static>(
    submit_label: &'static str,
    cx: &Context<V>,
    cancel: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    submit: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        // Wrapped for the same reason the chip rows are: a row justified to
        // the end overflows off the left of the panel, where the buttons
        // cannot be clicked.
        .flex_wrap()
        .gap_2()
        .justify_end()
        // Tagged so a test can assert the buttons are still on screen when
        // the body above them is far taller than the window.
        .debug_selector(|| "dialog-buttons".into())
        .child(
            // Wider than a chip — a button's label wants room around it —
            // but the same height, so the button row and the chip rows above
            // it are the same run of controls.
            ui::control("dialog-cancel")
                .px_3()
                .text_color(theme::fg_dim())
                .hover(|s| s.bg(theme::hover_bg()))
                .on_click(cx.listener(move |this, _, window, cx| cancel(this, window, cx)))
                .child("Cancel"),
        )
        .child(
            ui::control("dialog-submit")
                .px_3()
                .bg(theme::accent())
                .border_color(theme::accent())
                .text_color(theme::panel_bg())
                .hover(|s| s.opacity(0.9))
                .on_click(cx.listener(move |this, _, window, cx| submit(this, window, cx)))
                .child(submit_label),
        )
}

// ---- New agent ----

pub enum NewAgentEvent {
    /// Spawn the task described in the input — the button, or ctrl-enter.
    Spawn,
    Cancel,
}

/// Task entry for spawning an agent into a project (and optionally a group).
pub struct NewAgentDialog {
    pub project_path: PathBuf,
    /// Group the new agent lands in, when the spawn was started from a
    /// group header rather than the project row.
    pub group: Option<String>,
    pub input: Entity<InputState>,
    pub planning: bool,
    pub preset: usize,
    pub workspace_mode: WorkspaceMode,
    pub error: Option<String>,
    /// Chip labels, snapshotted at open: presets can only change through the
    /// settings dialog, which can't be open at the same time as this one.
    preset_names: Vec<String>,
    _subscription: Subscription,
}

impl EventEmitter<NewAgentEvent> for NewAgentDialog {}

impl NewAgentDialog {
    pub fn new(
        project_path: PathBuf,
        group: Option<String>,
        settings: &SettingsRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Describe the task… (ctrl-enter to spawn)")
                .multi_line(true)
                .auto_grow(1, MAX_TASK_ROWS)
        });
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            // Multi-line: plain enter inserts a newline, the secondary chord
            // (ctrl-enter here) spawns. Escape is handled by the dialog
            // layer, which owns it for every dialog.
            if let InputEvent::PressEnter { secondary: true } = event {
                cx.emit(NewAgentEvent::Spawn);
            }
        });
        Self {
            project_path,
            group,
            input,
            planning: false,
            preset: settings
                .last_preset
                .min(settings.presets.len().saturating_sub(1)),
            // Always Auto, never the last choice: forcing a workspace is a
            // decision about *one* task, and inheriting it silently would put
            // the next task somewhere it wasn't meant to go.
            workspace_mode: WorkspaceMode::Auto,
            error: None,
            preset_names: settings.presets.iter().map(|p| p.name.clone()).collect(),
            _subscription: subscription,
        }
    }

    /// The field the keyboard belongs in when the dialog opens.
    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    pub fn task(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }
}

impl Render for NewAgentDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = dialog_body("new-agent-body").child(ui::field(&self.input));

        // Preset selector.
        let mut chips = div().flex().flex_wrap().items_center().gap_1().child(
            div()
                .text_color(theme::fg_dim())
                .text_sm()
                .child("Preset:"),
        );
        for (index, name) in self.preset_names.iter().enumerate() {
            chips = chips.child(
                chip(("preset-chip", index), index == self.preset)
                    // Long preset names must not widen the dialog.
                    .max_w_full()
                    .truncate()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.preset = index;
                        cx.notify();
                    }))
                    .child(SharedString::from(ellipsize(name, CHIP_LABEL_CHARS))),
            );
        }
        panel = panel.child(chips);

        // Workspace selector. The task description still names the agent in
        // every mode; this only picks where it works.
        let mut modes = div().flex().flex_wrap().items_center().gap_1().child(
            div()
                .text_color(theme::fg_dim())
                .text_sm()
                .child("Workspace:"),
        );
        for (index, mode) in WorkspaceMode::ALL.into_iter().enumerate() {
            modes = modes.child(
                chip(("workspace-mode-chip", index), mode == self.workspace_mode)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_mode = mode;
                        cx.notify();
                    }))
                    .child(mode.label()),
            );
        }
        panel = panel.child(modes);

        if let Some(error) = &self.error {
            panel = panel.child(
                div()
                    .text_color(theme::error())
                    .text_sm()
                    .child(SharedString::from(error.clone())),
            );
        }

        // Title above, buttons below, the fields between them scrolling: the
        // task field alone grows to twelve rows, and at a large font size
        // that is more than a short window has.
        let frame = dialog_frame(window)
            .child(
                div()
                    .text_color(theme::fg())
                    .child("New agent — describe the task"),
            )
            .child(panel);

        if self.planning {
            frame.child(
                div()
                    .text_color(theme::warn())
                    .text_sm()
                    .child("Planning workspace with LLM…"),
            )
        } else {
            frame.child(dialog_buttons(
                "Spawn",
                cx,
                |_, _, cx| cx.emit(NewAgentEvent::Cancel),
                |_, _, cx| cx.emit(NewAgentEvent::Spawn),
            ))
        }
    }
}

// ---- Terminal search ----

pub enum SearchEvent {
    /// Run one search step through the terminal behind the dialog.
    Search { forward: bool },
    Close,
}

/// Search the visible terminal's scrollback. Matches are shown by selecting
/// them, so the dialog stays out of the way of the terminal itself and the
/// usual copy shortcut works on whatever it found. The search itself runs in
/// the workspace — the terminal is its state — which writes the result line
/// back into `status`.
pub struct SearchDialog {
    pub input: Entity<InputState>,
    pub match_case: bool,
    pub wrap: bool,
    /// Result of the last search — "no matches", "wrapped", …
    pub status: Option<String>,
    _subscription: Subscription,
}

impl EventEmitter<SearchEvent> for SearchDialog {}

impl SearchDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in terminal…"));
        let subscription = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            // Enter walks forward through the matches.
            if let InputEvent::PressEnter { .. } = event {
                cx.emit(SearchEvent::Search { forward: true });
            }
        });
        Self {
            input,
            match_case: false,
            wrap: true,
            status: None,
            _subscription: subscription,
        }
    }

    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for SearchDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = dialog_body("search-body")
            .child(ui::field(&self.input))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        chip("search-match-case", self.match_case)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.match_case = !this.match_case;
                                cx.notify();
                            }))
                            .child("Match case"),
                    )
                    .child(
                        chip("search-wrap", self.wrap)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.wrap = !this.wrap;
                                cx.notify();
                            }))
                            .child("Wrap around"),
                    ),
            );

        if let Some(status) = &self.status {
            panel = panel.child(
                div()
                    .text_color(theme::fg_dim())
                    .text_sm()
                    .child(SharedString::from(status.clone())),
            );
        }

        let button = |id: &'static str, label: &'static str, primary: bool| {
            ui::control(id)
                .px_3()
                .bg(if primary {
                    theme::accent()
                } else {
                    theme::selected_bg()
                })
                .border_color(if primary { theme::accent() } else { theme::border() })
                .text_color(if primary { theme::panel_bg() } else { theme::fg() })
                .hover(|s| s.opacity(0.9))
                .child(label)
        };
        dialog_frame(window)
            .child(div().text_color(theme::fg()).child("Find in terminal"))
            .child(panel)
            .child(
                div()
                    .flex()
                    // Three buttons at a large font are wider than a 460px
                    // dialog, and an unwrapped row overflows to the *left*
                    // (it is justified to the end), taking Close off the
                    // panel entirely. Wrapping keeps every button reachable.
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    // Same tag as `dialog_buttons`: this dialog has three buttons
                    // rather than two, but the row plays the same part.
                    .debug_selector(|| "dialog-buttons".into())
                    .child(
                        ui::control("search-close")
                            .px_3()
                            .text_color(theme::fg_dim())
                            .hover(|s| s.text_color(theme::fg()))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Close)))
                            .child("Close"),
                    )
                    .child(button("search-prev", "Previous", false).on_click(
                        cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Search { forward: false })),
                    ))
                    .child(button("search-next", "Next", true).on_click(
                        cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Search { forward: true })),
                    )),
            )
    }
}

// ---- Settings ----

pub enum SettingsEvent {
    /// Apply and persist what the fields hold (see [`SettingsDialog::values`]).
    Save,
    Cancel,
    /// The +/- steppers. Font size applies immediately, not on save, so the
    /// workspace adjusts it as soon as the chip is clicked.
    AdjustFontSize(f32),
    /// The Light/Dark chips — applied immediately, like the font size.
    SetTheme(theme::ThemeMode),
}

struct PresetInputs {
    name: Entity<InputState>,
    command: Entity<InputState>,
    env: Entity<InputState>,
}

/// Everything the settings dialog edits, read out of its fields on save.
pub struct SettingsValues {
    pub planner_command: String,
    pub planner_model: String,
    pub terminal_font: String,
    pub ui_font: String,
    pub presets: Vec<PresetRecord>,
    /// Name of the preset the planner runs through, resolved from the picked
    /// row after empty rows are dropped. `None` is the built-in `claude -p`.
    pub planner_preset: Option<String>,
}

pub struct SettingsDialog {
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
    subscriptions: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsDialog {}

impl SettingsDialog {
    pub fn new(settings: &SettingsRecord, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();
        let preset_inputs: Vec<PresetInputs> = settings
            .presets
            .iter()
            .map(|p| Self::make_preset_inputs(p, &mut subscriptions, window, cx))
            .collect();
        let planner_command = Self::make_input(
            "Full command; leave empty to use the model below",
            &settings.planner_command,
            &mut subscriptions,
            window,
            cx,
        );
        let planner_model = Self::make_input(
            planner::DEFAULT_MODEL,
            &settings.planner_model,
            &mut subscriptions,
            window,
            cx,
        );
        let terminal_font = Self::make_input(
            theme::DEFAULT_TERMINAL_FONT,
            &settings.terminal_font,
            &mut subscriptions,
            window,
            cx,
        );
        let ui_font = Self::make_input(
            theme::DEFAULT_UI_FONT,
            &settings.ui_font,
            &mut subscriptions,
            window,
            cx,
        );
        let planner_preset_row = settings.planner_preset.as_deref().and_then(|name| {
            settings
                .presets
                .iter()
                .position(|preset| preset.name == name)
        });
        Self {
            focus_handle: cx.focus_handle(),
            planner_command,
            planner_model,
            terminal_font,
            ui_font,
            preset_inputs,
            planner_preset_row,
            subscriptions,
        }
    }

    /// One text field, wired to the same save-on-enter handling as the
    /// preset fields.
    fn make_input(
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
        subscriptions.push(cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                cx.emit(SettingsEvent::Save);
            }
        }));
        input
    }

    fn make_preset_inputs(
        preset: &PresetRecord,
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PresetInputs {
        PresetInputs {
            name: Self::make_input("Preset name", &preset.name, subscriptions, window, cx),
            command: Self::make_input(
                "Command (task, --continue or planner flags appended)",
                &preset.command,
                subscriptions,
                window,
                cx,
            ),
            env: Self::make_input("KEY=value KEY2=value2", &preset.env, subscriptions, window, cx),
        }
    }

    /// The field the keyboard belongs in when the dialog opens: the first
    /// preset's name, or the panel itself when there are no presets.
    pub fn first_focus(&self, cx: &App) -> FocusHandle {
        match self.preset_inputs.first() {
            Some(first) => first.name.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }

    /// What the fields hold, ready for the workspace to apply and persist.
    pub fn values(&self, cx: &App) -> SettingsValues {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let mut presets: Vec<PresetRecord> = Vec::new();
        // Follow the planner's row through: empty rows are dropped here, so
        // the saved name has to be looked up while the mapping is still known.
        let mut planner_preset = None;
        for (row, inputs) in self.preset_inputs.iter().enumerate() {
            let name = text(&inputs.name);
            let command = text(&inputs.command);
            let env = text(&inputs.env);
            if name.is_empty() && command.is_empty() {
                continue;
            }
            let name = if name.is_empty() { command.clone() } else { name };
            if self.planner_preset_row == Some(row) {
                planner_preset = Some(name.clone());
            }
            presets.push(PresetRecord {
                name,
                command,
                resume_command: None,
                env,
            });
        }
        SettingsValues {
            planner_command: text(&self.planner_command),
            planner_model: text(&self.planner_model),
            terminal_font: text(&self.terminal_font),
            ui_font: text(&self.ui_font),
            presets,
            planner_preset,
        }
    }

    fn add_preset_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        let inputs =
            Self::make_preset_inputs(&PresetRecord::default(), &mut subscriptions, window, cx);
        // Typing should land in the row that was just added.
        window.focus(&inputs.name.focus_handle(cx));
        self.preset_inputs.push(inputs);
        self.subscriptions.extend(subscriptions);
        cx.notify();
    }

    fn remove_preset_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.preset_inputs.len() {
            self.preset_inputs.remove(index);
        }
        cx.notify();
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
                ui::control("font-size-dec")
                    .px_2()
                    .bg(theme::selected_bg())
                    .border_color(theme::border())
                    .text_color(theme::fg())
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::AdjustFontSize(-1.));
                        cx.notify();
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
                ui::control("font-size-inc")
                    .px_2()
                    .bg(theme::selected_bg())
                    .border_color(theme::border())
                    .text_color(theme::fg())
                    .hover(|s| s.bg(theme::hover_bg()))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::AdjustFontSize(1.));
                        cx.notify();
                    }))
                    .child("+"),
            )
    }

    fn render_theme_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let current = theme::mode();
        let theme_chip = |label: &'static str, mode: theme::ThemeMode, cx: &Context<Self>| {
            chip(label, current == mode)
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(SettingsEvent::SetTheme(mode));
                    cx.notify();
                }))
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_color(theme::fg_dim()).text_sm().child("Theme"))
            .child(theme_chip("Light", theme::ThemeMode::Light, cx))
            .child(theme_chip("Dark", theme::ThemeMode::Dark, cx))
    }

    /// Picks which agent preset the planner runs through. Reads the preset
    /// *rows* rather than the saved list, so a name edited in this same visit
    /// is what you choose from.
    fn render_planner_preset_row(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let selected = self.planner_preset_row;
        let preset_chip = |label: SharedString, row: Option<usize>, cx: &Context<Self>| {
            chip(
                ("planner-preset", row.map(|r| r + 1).unwrap_or(0)),
                row == selected,
            )
            // A long preset name must not widen the dialog past the
            // window: the chip is capped and its label ellipsized.
            .max_w_full()
            .truncate()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.planner_preset_row = row;
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
            .child(preset_chip("Default (claude)".into(), None, cx));
        for (index, inputs) in self.preset_inputs.iter().enumerate() {
            let name = inputs.name.read(cx).value().trim().to_string();
            let command = inputs.command.read(cx).value().trim().to_string();
            if name.is_empty() && command.is_empty() {
                continue;
            }
            let label = if name.is_empty() { command } else { name };
            row = row.child(preset_chip(
                ellipsize(&label, CHIP_LABEL_CHARS).into(),
                Some(index),
                cx,
            ));
        }
        row.into_any_element()
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = |text: &'static str| {
            div().text_color(theme::fg()).text_sm().mt_2().child(text)
        };
        let field = |text: &'static str, input: &Entity<InputState>| {
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
                .child(div().flex_1().min_w_0().child(ui::field(input)))
        };
        let mut preset_list = div().flex().flex_col().gap_2();
        for (index, inputs) in self.preset_inputs.iter().enumerate() {
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
                    .rounded(theme::CORNER_RADIUS)
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(label("Name"))
                            .child(div().flex_1().min_w_0().child(ui::field(&inputs.name)))
                            .child(
                                div()
                                    .id(("preset-remove", index))
                                    .px_1()
                                    .rounded(theme::CORNER_RADIUS)
                                    .text_color(theme::fg_dim())
                                    .cursor_pointer()
                                    .hover(|s| {
                                        s.bg(theme::selected_bg()).text_color(theme::error())
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
                            .child(div().flex_1().min_w_0().child(ui::field(&inputs.command))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(label("Env"))
                            .child(div().flex_1().min_w_0().child(ui::field(&inputs.env))),
                    ),
            );
        }

        // Title and buttons stay fixed; everything in between scrolls, so
        // the dialog never outgrows the window.
        dialog_frame(window)
            // Focus target of last resort: with no preset rows there is no
            // input to hold the keyboard, and it would fall back to the
            // terminal behind the dialog.
            .track_focus(&self.focus_handle)
            .child(div().text_color(theme::fg()).child("Settings"))
            .child(
                dialog_body("settings-body")
                    .child(self.render_font_size_row(cx))
                    .child(self.render_theme_row(cx))
                    .child(section("Fonts"))
                    .child(field("Terminal", &self.terminal_font))
                    .child(field("UI", &self.ui_font))
                    .child(section("Planner"))
                    .child(
                        div()
                            .text_color(theme::fg_dim())
                            .text_xs()
                            .child("Derives the branch and agent name from the task description."),
                    )
                    .child(self.render_planner_preset_row(cx))
                    .child(field("Command", &self.planner_command))
                    .child(field("Model", &self.planner_model))
                    .when(
                        !self.planner_command.read(cx).value().trim().is_empty(),
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
                        ui::control("preset-add")
                            .px_2()
                            .text_color(theme::accent())
                            .hover(|s| s.bg(theme::hover_bg()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_preset_row(window, cx);
                            }))
                            .child("+ Add preset"),
                    ),
            )
            .child(dialog_buttons(
                "Save",
                cx,
                |_, _, cx| cx.emit(SettingsEvent::Cancel),
                |_, _, cx| cx.emit(SettingsEvent::Save),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
