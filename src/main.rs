mod assets;
mod input;
mod planner;
mod state;
mod terminal;
mod theme;
mod workspace;

use gpui::{
    actions, px, size, App, AppContext as _, Application, Bounds, KeyBinding, TitlebarOptions,
    WindowBounds, WindowOptions,
};

actions!(harmonium, [Quit]);

fn main() {
    env_logger::init();
    // Debug helper: `harmonium plan <repo> <task…>` prints the derived plan and
    // exits, without starting the UI.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "plan" {
        let repo = std::path::PathBuf::from(&args[2]);
        let task = args[3..].join(" ");
        let settings = state::load_state().settings;
        let planner_settings = planner::PlannerSettings {
            command: settings.planner_command,
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

    Application::new().with_assets(assets::Assets).run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        // Text input editing keys, scoped to focused TextInput widgets.
        cx.bind_keys([
            KeyBinding::new("backspace", input::Backspace, Some("TextInput")),
            KeyBinding::new("delete", input::Delete, Some("TextInput")),
            KeyBinding::new("left", input::Left, Some("TextInput")),
            KeyBinding::new("right", input::Right, Some("TextInput")),
            KeyBinding::new("shift-left", input::SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", input::SelectRight, Some("TextInput")),
            KeyBinding::new("ctrl-a", input::SelectAll, Some("TextInput")),
            KeyBinding::new("ctrl-v", input::Paste, Some("TextInput")),
            KeyBinding::new("ctrl-c", input::Copy, Some("TextInput")),
            KeyBinding::new("ctrl-x", input::Cut, Some("TextInput")),
            KeyBinding::new("home", input::Home, Some("TextInput")),
            KeyBinding::new("end", input::End, Some("TextInput")),
            KeyBinding::new("escape", input::Cancel, Some("TextInput")),
            // Single-line: enter submits. Multiline: enter inserts a newline,
            // ctrl-enter submits, up/down move the cursor.
            KeyBinding::new("enter", input::Submit, Some("TextInput && !multiline")),
            KeyBinding::new("enter", input::Newline, Some("TextInput && multiline")),
            KeyBinding::new("ctrl-enter", input::Submit, Some("TextInput && multiline")),
            KeyBinding::new("up", input::Up, Some("TextInput && multiline")),
            KeyBinding::new("down", input::Down, Some("TextInput && multiline")),
        ]);

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
                workspace
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
