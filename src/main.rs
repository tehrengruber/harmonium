mod assets;
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

fn main() {
    env_logger::init();
    // Debug helper: `harmonium plan <repo> <task…>` prints the derived plan and
    // exits, without starting the UI.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "plan" {
        let repo = std::path::PathBuf::from(&args[2]);
        let task = args[3..].join(" ");
        let settings = state::load_state().settings;
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
                let workspace = cx.new(workspace::Workspace::new);
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
