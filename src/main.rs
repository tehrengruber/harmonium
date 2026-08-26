mod assets;
mod fatal;
mod log;
mod planner;
mod state;
mod terminal;
mod theme;
mod workspace;

use gpui::{
    actions, px, size, App, AppContext as _, Application, Bounds, KeyBinding, TitlebarOptions,
    WindowBounds, WindowOptions,
};

actions!(harmonium, [Quit, NewTerminalTab, SearchTerminal]);

/// The saved session, claimed for this process — or a message and a non-zero
/// exit. Both failures end the same way, because both would end in one session
/// being written over another: a state file that can't be read would look like
/// a fresh install and be saved over, and a second harmonium would race the
/// first one's saves.
fn load_state_or_exit() -> (state::StateFile, state::AppState) {
    let error = match state::StateFile::load() {
        Ok(loaded) => return loaded,
        Err(error) => error,
    };
    let advice = match error {
        state::LoadError::Locked { .. } => "Not starting: the two would each save their own \
             session over the other's. Close the running one first."
            .to_string(),
        state::LoadError::Unreadable(_) => format!(
            "Not starting: continuing would overwrite this file with a fresh state and lose the \
             projects and presets in it. Move {} aside (or repair it) and start harmonium again.",
            state::state_file().display()
        ),
    };
    // The terminal first, and unconditionally: it costs nothing, it is what a
    // launch from a shell wants, and it is still there if the window below
    // can't be opened.
    eprintln!("harmonium: {error}");
    eprintln!("{advice}");
    // Launched from a desktop file or a launcher there is no terminal to read
    // that in, and an app that exits silently looks broken rather than refused.
    fatal::show(
        "Harmonium can't start",
        vec![error.to_string().into(), advice.into()],
    );
    std::process::exit(1);
}

fn main() {
    env_logger::init();
    // Debug helper: `harmonium plan <repo> <task…>` prints the derived plan and
    // exits, without starting the UI.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "plan" {
        let repo = std::path::PathBuf::from(&args[2]);
        let task = args[3..].join(" ");
        // Reads without claiming the session: this helper never saves, and
        // should work while harmonium itself is running.
        let settings = match state::read_state() {
            Ok(state) => state.settings,
            Err(error) => {
                eprintln!("harmonium: {error:#}");
                std::process::exit(1);
            }
        };
        // Same resolution as the UI: an explicit command, else the selected
        // agent preset, else the default. Preset environments are a UI concern
        // and stay out of this debug helper.
        let preset = settings
            .planner_preset
            .as_deref()
            .and_then(|name| settings.presets.iter().find(|p| p.name == name));
        let planner_settings = planner::PlannerSettings {
            command: settings.planner_command,
            preset_argv: preset
                .map(|p| workspace::planner_argv(p, &settings.planner_model, &repo))
                .unwrap_or_default(),
            env: Vec::new(),
            model: settings.planner_model,
        };
        match planner::plan_task(&repo, &task, &planner_settings) {
            Ok(plan) => println!("{plan:#?}"),
            Err(error) => {
                eprintln!("planner failed ({error}), fallback:");
                println!("{:#?}", planner::fallback_plan(&repo, &task));
            }
        }
        return;
    }

    // Before anything is opened: a state file we can't read is fatal, and the
    // message for it belongs on the terminal, not behind a window.
    let (state_file, state) = load_state_or_exit();

    // gpui never quits on its own when the last window closes, so that is
    // ours to do. It must not happen inside gpui's window teardown: on X11
    // that runs while the platform client's RefCell is borrowed for the
    // event being handled, and `App::quit` re-enters it ("RefCell already
    // borrowed"). Hence the deferred quit one tick later, below.
    let app = Application::new().with_assets(assets::Assets);

    app.run(|cx: &mut App| {
        // Component library: must be initialised before any of its widgets
        // are built, and it owns the window's root element (see `Root`).
        gpui_component::init(cx);
        theme::sync_component_theme(cx);

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.spawn(async move |cx| {
                    cx.update(|cx| cx.quit()).ok();
                })
                .detach();
            }
        })
        .detach();

        cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        // New shell tab for the selected agent, handled by the workspace.
        // Key bindings are matched before any `on_key_down` listener, so this
        // wins over the focused terminal's keyboard passthrough; excluding
        // the Input context keeps it inert while a dialog field has the keyboard.
        cx.bind_keys([KeyBinding::new(
            "ctrl-shift-t",
            NewTerminalTab,
            Some("!Input"),
        )]);

        // Search the visible terminal's scrollback. Bound everywhere *except*
        // a focused text field, so it can't fire while the search box itself
        // has the keyboard.
        cx.bind_keys([KeyBinding::new(
            "ctrl-shift-f",
            SearchTerminal,
            Some("!Input"),
        )]);

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Harmonium".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let workspace = cx.new(|cx| workspace::Workspace::new(state, state_file, cx));
                // A window-manager close (e.g. i3 `kill` sending
                // WM_DELETE_WINDOW) only reaches us through this hook; gpui
                // removes the window — and drops the workspace — before any
                // app-quit observer runs, so the session is saved here.
                window.on_window_should_close(cx, {
                    let workspace = workspace.clone();
                    move |_, cx| {
                        workspace.update(cx, |workspace, cx| workspace.save_session(cx));
                        true
                    }
                });
                // gpui-component renders dialogs, notifications and popups
                // into layers owned by `Root`, so it has to be the window's
                // root element with our workspace inside it.
                cx.new(|cx| gpui_component::Root::new(gpui::AnyView::from(workspace), window, cx))
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
