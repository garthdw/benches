mod font;
mod input;
mod render;
mod session;
mod sprite;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libghostty_vt::mouse;
use rune::tok;
use rune::{
    App, AppSpec, ClipboardContent, Color, Cursor, Cx, Drag, El, FileDialogOptions, Ime, Key,
    KeyEvent, Mods, Phase, PointerButton, PointerEvent, PopupItem, PopupMenu, TextEdit, Tx, Waker,
    WheelEvent, col, effect, el, row, text, text_input,
};

use session::{Event, Grid, PointerInput, PointerPhase, Session, WheelInput};
use workspace::{Project, Tab, Workspace};

const TERMINAL_MARK: u32 = 1;
/// Wider than the traffic lights, which sit over the sidebar.
const SIDEBAR_WIDTH: f32 = 200.0;
/// The sidebar's top padding too, so the traffic lights clear the first project.
const TAB_STRIP_HEIGHT: f32 = 36.0;
/// Title and command of each tab a new project opens with; `None` runs the launch shell.
const PROJECT_TABS: [(&str, Option<&str>); 2] = [("claude", Some("claude")), ("shell", None)];
const SCENE_FIXTURE: &[u8] = b"\x1b[1;32magentd\x1b[0m fixture\r\n\x1b[31mred\x1b[0m \x1b[42;30m on green \x1b[0m \x1b[7minverse\x1b[0m\r\n$ ";
const SCENE_UNICODE: &str = concat!(
    "wide \u{4e16}\u{754c} kana \u{30ab}\u{30bf}\u{30ab}\u{30ca}|\r\n",
    "emoji \u{1F600}\u{1F680} zwj \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} vs16 \u{2764}\u{FE0F}|\r\n",
    "marks e\u{301}a\u{308} symbols \u{23FA} \u{23BF} \u{2713}|\r\n",
    "\u{250C}\u{2500}\u{252C}\u{2500}\u{2510} \u{256D}\u{2500}\u{256E} \u{2554}\u{2550}\u{2557} \u{2588}\u{2593}\u{2592}\u{2591} \u{2580}\u{2584}\u{258C}\u{2590}\r\n",
    "\u{2502} \u{253C} \u{2502} \u{2570}\u{2500}\u{256F} \u{255A}\u{2550}\u{255D} \u{E0B0}\u{E0B1}\u{E0B2}\u{E0B3}\r\n",
    "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[1;3mboth\x1b[0m \x1b[2mfaint\x1b[0m \x1b[9mstrike\x1b[0m \x1b[53mover\x1b[0m\r\n",
    "\x1b[4msingle\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:3;58;2;255;80;80mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m \x1b[4:5mdashed\x1b[0m\r\n",
    "\x1b[41m\u{1F41F}\x1b[31;43m\u{E0C6}\x1b[30m ~ \x1b[33;42m\u{E0C6}\x1b[32;49m\u{E0C6}\x1b[0m\r\n",
    "\x1b[32m\u{F432}\x1b[0m $ "
);

#[derive(Default)]
pub struct State {
    waker: Option<Waker>,
    workspace: Workspace,
    started: bool,
    /// Session id and generation last drawn.
    drawn: (u64, u64),
    pending_paste: Option<String>,
    preedit: String,
    option_held: bool,
    caret: Option<[f32; 4]>,
    command: TextEdit,
    command_open: bool,
}

impl State {
    fn session(&self) -> Option<&Session> {
        self.workspace.active_session()
    }

    fn grid(&self) -> Arc<Grid> {
        self.session().map_or_else(Arc::default, Session::grid)
    }

    fn spawn_tab(&self, title: &str, command: Option<&str>, cwd: &Path) -> Tab {
        let waker = self.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
        let session = Session::spawn(command, cwd, waker).unwrap_or_else(|error| {
            Session::fixture(Grid::message(&format!(
                "failed to start {title}: {error:#}"
            )))
        });
        Tab {
            title: title.to_owned(),
            session,
        }
    }
}

fn open_tab(state: &mut State, title: &str, command: Option<&str>) {
    let Some(root) = state
        .workspace
        .project()
        .map(|project| project.root.clone())
    else {
        return;
    };
    let tab = state.spawn_tab(title, command, &root);
    state.workspace.add_tab(tab);
}

fn open_project(state: &mut State, root: PathBuf) {
    if let Some(index) = state.workspace.project_index(&root) {
        state.workspace.select_project(index);
        return;
    }
    let tabs = PROJECT_TABS
        .iter()
        .map(|(title, command)| state.spawn_tab(title, *command, &root))
        .collect();
    state.workspace.add_project(Project::new(root), tabs);
}

fn pick_folder(tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Open project".into(),
        ..FileDialogOptions::default()
    };
    tx.open_folder(options, |state: &mut State, result, _| match result {
        Ok(Some(root)) => open_project(state, root),
        Ok(None) => {}
        Err(error) => eprintln!("agentd: open folder: {error}"),
    });
}

fn pick_new_tab(state: &mut State, picked: Option<&str>, tx: &mut Tx) {
    match picked {
        Some("shell") => open_tab(state, "shell", None),
        Some("lazygit") => open_tab(state, "lazygit", Some("lazygit")),
        Some("custom") => {
            state.command = TextEdit::new("");
            state.command_open = true;
            tx.focus("command", false);
        }
        _ => {}
    }
}

fn submit_command(state: &mut State) {
    state.command_open = false;
    let command = state.command.text.trim().to_owned();
    if !command.is_empty() {
        open_tab(state, &command, Some(&command));
    }
}

/// Workspace shortcuts. These keys never reach the terminal.
fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let mods = event.mods;
    let plain_command = !mods.shift && !mods.alt && !mods.ctrl;
    let digit_index = |digit: char| digit.to_digit(10).map(|digit| digit as usize - 1);
    match &event.key {
        Key::Command('n') if plain_command => pick_folder(tx),
        Key::Command('t') if plain_command => open_tab(state, "shell", None),
        Key::Command('w') if plain_command => state.workspace.close_tab(),
        Key::Command(digit @ '1'..='9') if plain_command => {
            state
                .workspace
                .select_project(digit_index(*digit).unwrap_or(0));
        }
        Key::Command('[' | '{') if mods.shift => state.workspace.cycle_tab(-1),
        Key::Command(']' | '}') if mods.shift => state.workspace.cycle_tab(1),
        Key::Text(text) if mods.ctrl && !mods.cmd && !mods.alt && !mods.shift => {
            match tx.key_base().or_else(|| text.chars().next()) {
                Some(digit @ '1'..='9') => {
                    state.workspace.select_tab(digit_index(digit).unwrap_or(0));
                }
                _ => return false,
            }
        }
        _ => return false,
    }
    true
}

fn send_pointer(
    state: &State,
    phase: PointerPhase,
    button: Option<mouse::Button>,
    x: f32,
    y: f32,
    mods: Mods,
) {
    if let Some(session) = state.session() {
        session.pointer(PointerInput {
            phase,
            button,
            x,
            y,
            mods: input::vt_mods(mods),
        });
    }
}

/// The cursor cell in points within the terminal surface, or `None` before the first layout.
fn caret_rect(grid: &Grid) -> Option<[f32; 4]> {
    let (x, y) = grid.cursor?;
    let metrics = grid.metrics;
    if metrics.cell_width == 0 {
        return None;
    }
    let width = f32::from(metrics.cell_width) / metrics.scale;
    let height = f32::from(metrics.cell_height) / metrics.scale;
    let padding = metrics.padding / metrics.scale;
    Some([
        padding + f32::from(x) * width,
        padding + f32::from(y) * height,
        width,
        height,
    ])
}

fn resolve_paste(state: &mut State, confirmed: bool) {
    if let Some(text) = state.pending_paste.take()
        && confirmed
        && let Some(session) = state.session()
    {
        session.paste(text, true);
    }
}

fn view(state: &State, _: &mut Cx<State>) -> El {
    let theme = tok::theme(true);
    let pill = |key: String, label: &str, active: bool| {
        el().key(key)
            .role("button")
            .label(label)
            .px(10.0)
            .py(4.0)
            .radius(6.0)
            .fill(if active {
                theme.control_off
            } else {
                Color::TRANSPARENT
            })
            .child(text(label.to_owned()).color(theme.text).truncate())
    };
    let projects = state
        .workspace
        .projects
        .iter()
        .enumerate()
        .map(|(index, project)| {
            pill(
                format!("project-{index}"),
                &project.name,
                index == state.workspace.active,
            )
            .w_full()
            .on_press(move |state: &mut State| state.workspace.select_project(index))
        });
    let sidebar = col()
        .key("sidebar")
        .w(SIDEBAR_WIDTH)
        .h_full()
        .gap(2.0)
        .pt(TAB_STRIP_HEIGHT)
        .px(8.0)
        .fill(theme.card)
        .children(projects);
    let tabs = state.workspace.project().map_or_else(Vec::new, |project| {
        project
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                pill(
                    format!("tab-{index}"),
                    &tab.title,
                    index == project.active_tab,
                )
                .on_press(move |state: &mut State| state.workspace.select_tab(index))
            })
            .collect()
    });
    let new_tab =
        pill("new-tab".to_owned(), "+", false).on_press_tx(|_: &mut State, tx: &mut Tx| {
            let menu = PopupMenu::new()
                .item(PopupItem::new("shell", "Shell"))
                .item(PopupItem::new("lazygit", "lazygit"))
                .item(PopupItem::new("custom", "Custom command…"));
            tx.popup_menu(menu, "new-tab", pick_new_tab);
        });
    let mut overlays = Vec::new();
    if state.command_open {
        overlays.push(
            row()
                .key("command-bar")
                .absolute(12.0, 12.0)
                .gap(8.0)
                .items_center()
                .px(10.0)
                .py(6.0)
                .radius(6.0)
                .fill(theme.control_off)
                .child(text("Run").color(theme.text))
                .child(
                    text_input(&state.command, tok::type_::BODY)
                        .key("command")
                        .single_line()
                        .w(320.0)
                        .color(theme.text)
                        .on_edit(|state: &mut State| &mut state.command)
                        .on_submit(|state: &mut State, _: &mut Tx| submit_command(state))
                        .on_input_key(|state: &mut State, event: &KeyEvent, _: &mut Tx| {
                            let escape = event.key == Key::Escape;
                            if escape {
                                state.command_open = false;
                            }
                            escape
                        }),
                ),
        );
    }
    if let Some([x, y, width, height]) = state.caret {
        // winit routes Option through the input method while one is active, which eats Alt chords.
        if !state.option_held || !state.preedit.is_empty() {
            overlays.push(
                el().key("caret")
                    .absolute(x, y)
                    .size(width, height)
                    .ime_cursor(),
            );
        }
        if !state.preedit.is_empty() {
            overlays.push(
                el().key("preedit")
                    .absolute(x, y)
                    .fill(theme.bg)
                    .child(text(state.preedit.clone()).color(theme.text)),
            );
        }
    }
    if state.pending_paste.is_some() {
        let button = |key: &'static str, label: &'static str| {
            el().key(key)
                .role("button")
                .label(label)
                .px(10.0)
                .py(4.0)
                .radius(6.0)
                .fill(theme.bg)
                .child(text(label).color(theme.text))
        };
        overlays.push(
            row()
                .key("paste-confirm")
                .absolute(12.0, 12.0)
                .gap(8.0)
                .items_center()
                .px(10.0)
                .py(6.0)
                .radius(6.0)
                .fill(theme.control_off)
                .child(text("This paste could run commands").color(theme.text))
                .child(
                    button("paste-confirm-yes", "Paste")
                        .on_press(|state: &mut State| resolve_paste(state, true)),
                )
                .child(
                    button("paste-confirm-no", "Cancel")
                        .on_press(|state: &mut State| resolve_paste(state, false)),
                ),
        );
    }
    let terminal = col()
        .h_full()
        .grow(1.0)
        .min_w(0.0)
        .child(
            row()
                .key("tab-strip")
                .w_full()
                .h(TAB_STRIP_HEIGHT)
                .gap(4.0)
                .px(8.0)
                .items_center()
                .children(tabs)
                .child(new_tab),
        )
        .child(
            effect(TERMINAL_MARK)
                .key("terminal")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .cursor(Cursor::Text)
                .pointer_buttons([PointerButton::Middle])
                .on_drag(|state: &mut State, drag: &mut Drag, tx: &mut Tx| {
                    let phase = match drag.phase {
                        Phase::Down => PointerPhase::Press,
                        Phase::Move => PointerPhase::Drag,
                        Phase::Up => PointerPhase::Release,
                    };
                    let button = match tx.button() {
                        PointerButton::Middle => mouse::Button::Middle,
                        _ => mouse::Button::Left,
                    };
                    send_pointer(state, phase, Some(button), drag.x, drag.y, tx.mods());
                    tx.live_only();
                })
                .on_secondary_press_local(|state: &mut State, event: PointerEvent, tx: &mut Tx| {
                    // Rune reports the right button once, with no separate release.
                    for phase in [PointerPhase::Press, PointerPhase::Release] {
                        let button = Some(mouse::Button::Right);
                        send_pointer(state, phase, button, event.x, event.y, tx.mods());
                    }
                })
                .on_pointer_move_local(|state: &mut State, event: PointerEvent, tx: &mut Tx| {
                    send_pointer(state, PointerPhase::Move, None, event.x, event.y, tx.mods());
                    tx.live_only();
                })
                .on_wheel(|state: &mut State, event: WheelEvent, tx: &mut Tx| {
                    if let Some(session) = state.session() {
                        session.wheel(WheelInput {
                            dy: event.dy,
                            x: event.x,
                            y: event.y,
                            mods: input::vt_mods(event.mods),
                        });
                    }
                    tx.live_only();
                    true
                })
                .children(overlays),
        );
    row()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .on_drop_files(|state: &mut State, paths: &[PathBuf], _: &mut Tx| {
            for path in paths.iter().filter(|path| path.is_dir()) {
                open_project(state, path.clone());
            }
        })
        .child(sidebar)
        .child(terminal)
}

fn poll(state: &mut State, _: f64, tx: &mut Tx) -> bool {
    if !state.started {
        state.started = true;
        if state.workspace.projects.is_empty() {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
            let tab = state.spawn_tab("shell", None, &home);
            state.workspace.add_project(Project::new(home), vec![tab]);
        }
    }
    let active = state.session().map(Session::id);
    let mut changed = false;
    for session in state.workspace.sessions() {
        while let Some(event) = session.next_event() {
            match event {
                Event::Copied(text) => tx.copy(text),
                Event::PasteRejected(text) if Some(session.id()) == active => {
                    state.pending_paste = Some(text);
                    changed = true;
                }
                Event::PasteRejected(_) => {}
            }
        }
    }
    let grid = state.grid();
    // Kept while programs hide the cursor, as dropping the caret cancels a composition.
    if let Some(caret) = caret_rect(&grid) {
        state.caret = Some(caret);
    }
    let drawn = (active.unwrap_or(0), grid.generation);
    changed |= drawn != state.drawn;
    state.drawn = drawn;
    changed
}

fn on_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if state.pending_paste.is_some() && matches!(event.key, Key::Enter | Key::Escape) {
        resolve_paste(state, event.key == Key::Enter);
        return true;
    }
    if shortcut(state, event, tx) {
        return true;
    }
    // A focused field, such as the command bar, keeps the keys its editor didn't take.
    if tx.focused().is_some() {
        return false;
    }
    let plain_command = !event.mods.shift && !event.mods.alt && !event.mods.ctrl;
    match event.key {
        Key::Command('c') if plain_command => {
            if let Some(session) = state.session() {
                session.copy();
            }
            return true;
        }
        Key::Command('v') if plain_command => {
            tx.read_clipboard(
                |state: &mut State, content: &ClipboardContent, _: &mut Tx| {
                    if let Some(session) = state.session()
                        && !content.text().is_empty()
                    {
                        session.paste(content.text().to_owned(), false);
                    }
                },
            );
            return true;
        }
        _ => {}
    }
    match (
        state.session(),
        input::map_key(event, tx.physical_key(), tx.key_base()),
    ) {
        (Some(session), Some(key)) => {
            session.key(key);
            true
        }
        _ => false,
    }
}

fn on_ime(state: &mut State, ime: &Ime, _: &mut Tx) -> bool {
    match ime {
        Ime::Preedit(preedit, _) => {
            state.preedit.clone_from(preedit);
            true
        }
        Ime::Commit(committed) => {
            state.preedit.clear();
            if let Some(session) = state.session() {
                session.key(input::map_text(committed, Mods::default(), None, None));
            }
            true
        }
        _ => false,
    }
}

fn fixture_tab(title: &str, bytes: &[u8]) -> Tab {
    let grid = Grid::from_vt(bytes, 60, 12)
        .unwrap_or_else(|error| Grid::message(&format!("failed to build fixture: {error:#}")));
    Tab {
        title: title.to_owned(),
        session: Session::fixture(grid),
    }
}

fn show_scene(state: &mut State, bytes: &[u8]) {
    let tab = fixture_tab("shell", bytes);
    state
        .workspace
        .add_project(Project::new("/fixture".into()), vec![tab]);
}

fn show_workspace_scene(state: &mut State) {
    let agentd = vec![
        fixture_tab("claude", SCENE_FIXTURE),
        fixture_tab("shell", b"$ "),
        fixture_tab("lazygit", b""),
    ];
    state
        .workspace
        .add_project(Project::new("/dev/agentd".into()), agentd);
    let site = vec![fixture_tab("claude", b""), fixture_tab("shell", b"$ ")];
    state
        .workspace
        .add_project(Project::new("/dev/site".into()), site);
    state.workspace.select_project(0);
}

fn main() {
    let spec = AppSpec::<State> {
        scenes: &[
            ("fixture", |state| show_scene(state, SCENE_FIXTURE)),
            ("unicode", |state| {
                show_scene(state, SCENE_UNICODE.as_bytes())
            }),
            ("workspace", show_workspace_scene),
        ],
        setup: Some(|state, waker| state.waker = Some(waker)),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("agentd", (960.0, 640.0), State::default, view)
    };
    App::new(spec)
        .compositor(render::TerminalCompositor::default())
        .on_ime(on_ime)
        .on_modifiers(|state, mods, _| state.option_held = mods.alt)
        .on_focus(|state, focused, _| state.workspace.set_window_focused(focused))
        .run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WINDOW_KEYS: AtomicUsize = AtomicUsize::new(0);
    static KEYS_AFTER_TERMINAL_CLICK: AtomicUsize = AtomicUsize::new(0);

    fn runtime_with(state: State) -> Runtime<State> {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut runtime = Runtime::new(state, view, (960.0, 640.0), env);
        runtime.frame(0.0);
        runtime
    }

    #[test]
    fn clicking_the_terminal_leaves_keys_with_the_window() {
        let mut runtime = runtime_with(State::default());
        runtime.on_key = Some(|_, _, _| {
            KEYS_AFTER_TERMINAL_CLICK.fetch_add(1, Ordering::SeqCst);
            true
        });
        let (x, y) = runtime.find("terminal").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.key(KeyEvent {
            key: Key::Tab,
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        assert_eq!(KEYS_AFTER_TERMINAL_CLICK.load(Ordering::SeqCst), 1);
    }

    fn with_pending_paste() -> Runtime<State> {
        runtime_with(State {
            pending_paste: Some("rm -rf x\n".to_owned()),
            ..State::default()
        })
    }

    #[test]
    fn cancel_dismisses_the_paste_bar() {
        let mut runtime = with_pending_paste();
        let (x, y) = runtime.find("paste-confirm-no").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_none());
    }

    #[test]
    fn escape_dismisses_the_paste_bar() {
        let mut runtime = with_pending_paste();
        runtime.on_key = Some(on_key);
        runtime.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_none());
    }

    #[test]
    fn holding_option_removes_the_ime_caret() {
        let caret = Some([1.0, 1.0, 8.0, 16.0]);
        let shown = runtime_with(State {
            caret,
            ..State::default()
        });
        assert!(shown.find("caret").is_some());
        let held = runtime_with(State {
            caret,
            option_held: true,
            ..State::default()
        });
        assert!(held.find("caret").is_none());
    }

    #[test]
    fn holding_option_keeps_the_caret_during_a_composition() {
        let composing = runtime_with(State {
            caret: Some([1.0, 1.0, 8.0, 16.0]),
            option_held: true,
            preedit: "に".to_owned(),
            ..State::default()
        });
        assert!(composing.find("caret").is_some());
    }

    #[test]
    fn the_caret_follows_the_cursor_in_points() {
        let mut grid = Grid::from_vt(b"ab", 10, 2).unwrap();
        grid.metrics = session::Metrics {
            cell_width: 16,
            cell_height: 32,
            padding: 12.0,
            scale: 2.0,
        };
        assert_eq!(caret_rect(&grid), Some([22.0, 6.0, 8.0, 16.0]));
    }

    fn workspace_state() -> State {
        let mut state = State::default();
        show_workspace_scene(&mut state);
        state
    }

    fn click(runtime: &mut Runtime<State>, key: &str) {
        let (x, y) = runtime.find(key).unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
    }

    #[test]
    fn clicking_a_project_and_a_tab_selects_them() {
        let mut runtime = runtime_with(workspace_state());
        click(&mut runtime, "project-1");
        click(&mut runtime, "tab-1");
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[1].active_tab, 1);
    }

    fn press(runtime: &mut Runtime<State>, key: Key, mods: Mods) {
        runtime.key(KeyEvent { key, mods });
        runtime.frame(1.0 / 60.0);
    }

    fn command() -> Mods {
        Mods {
            cmd: true,
            logo: true,
            ..Mods::default()
        }
    }

    fn workspace_runtime() -> Runtime<State> {
        let mut runtime = runtime_with(workspace_state());
        runtime.on_key = Some(on_key);
        runtime
    }

    #[test]
    fn command_digits_pick_projects_and_control_digits_pick_tabs() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('2'), command());
        let control = Mods {
            ctrl: true,
            ..Mods::default()
        };
        press(&mut runtime, Key::Text("2".into()), control);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[1].active_tab, 1);
    }

    #[test]
    fn command_shift_brackets_cycle_tabs() {
        let mut runtime = workspace_runtime();
        let shifted = Mods {
            shift: true,
            ..command()
        };
        press(&mut runtime, Key::Command('{'), shifted);
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 2);
        press(&mut runtime, Key::Command(']'), shifted);
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 0);
    }

    #[test]
    fn command_w_closes_the_active_tab() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('w'), command());
        let titles: Vec<_> = runtime.state.workspace.projects[0]
            .tabs
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, ["shell", "lazygit"]);
    }

    #[test]
    fn command_n_opening_a_known_folder_selects_its_project() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('n'), command());
        runtime
            .resolve_file_dialog(Some(PathBuf::from("/dev/site")))
            .unwrap();
        runtime.frame(2.0 / 60.0);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(workspace.active, 1);
    }

    #[test]
    fn escape_closes_the_command_bar() {
        let mut runtime = runtime_with(State {
            command_open: true,
            ..workspace_state()
        });
        runtime.on_key = Some(on_key);
        click(&mut runtime, "command");
        press(&mut runtime, Key::Escape, Mods::default());
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("command-bar").is_none());
    }

    #[test]
    fn an_empty_command_opens_no_tab() {
        let mut state = workspace_state();
        state.command_open = true;
        submit_command(&mut state);
        assert!(!state.command_open);
        assert_eq!(state.workspace.projects[0].tabs.len(), 3);
    }

    #[test]
    fn tab_and_escape_reach_the_window_after_clicking_a_tab() {
        let mut runtime = runtime_with(workspace_state());
        runtime.on_key = Some(|_, _, _| {
            WINDOW_KEYS.fetch_add(1, Ordering::SeqCst);
            true
        });
        let (x, y) = runtime.find("tab-0").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.key(KeyEvent {
            key: Key::Tab,
            mods: Mods::default(),
        });
        runtime.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        runtime.key(KeyEvent {
            key: Key::Text(" ".into()),
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        assert_eq!(WINDOW_KEYS.load(Ordering::SeqCst), 3);
    }
}
