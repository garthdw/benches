mod font;
mod input;
mod render;
mod session;
mod sprite;

use std::path::PathBuf;
use std::sync::Arc;

use libghostty_vt::mouse;
use rune::tok;
use rune::{
    App, AppSpec, ClipboardContent, Cursor, Cx, Drag, El, Ime, Key, KeyEvent, Mods, Phase,
    PointerButton, PointerEvent, Tx, Waker, WheelEvent, col, effect, el, row, text,
};

use session::{Grid, PointerInput, PointerPhase, Session, WheelInput};

const TERMINAL_MARK: u32 = 1;
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
    session: Option<Session>,
    drawn_generation: u64,
    pending_paste: Option<String>,
    preedit: String,
    option_held: bool,
    caret: Option<[f32; 4]>,
}

impl State {
    fn grid(&self) -> Arc<Grid> {
        self.session
            .as_ref()
            .map_or_else(Arc::default, Session::grid)
    }
}

fn send_pointer(
    state: &State,
    phase: PointerPhase,
    button: Option<mouse::Button>,
    x: f32,
    y: f32,
    mods: Mods,
) {
    if let Some(session) = &state.session {
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
        && let Some(session) = &state.session
    {
        session.paste(text, true);
    }
}

fn view(state: &State, cx: &mut Cx<State>) -> El {
    let theme = tok::theme(true);
    let chrome = el()
        .key("chrome")
        .role("button")
        .label("shell")
        .px(10.0)
        .py(4.0)
        .radius(6.0)
        .fill(theme.control_off)
        .child(text("shell").color(theme.text));
    let mut overlays = Vec::new();
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
    col()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .child(
            row()
                .w_full()
                .pl(cx.titlebar_inset() + 8.0)
                .pr(8.0)
                .py(6.0)
                .child(chrome),
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
                .on_wheel(|state: &mut State, event: WheelEvent, _: &mut Tx| {
                    if let Some(session) = &state.session {
                        session.wheel(WheelInput {
                            dy: event.dy,
                            x: event.x,
                            y: event.y,
                            mods: input::vt_mods(event.mods),
                        });
                    }
                    true
                })
                .children(overlays),
        )
}

fn poll(state: &mut State, _: f64, tx: &mut Tx) -> bool {
    if state.session.is_none() {
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let waker = state.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
        state.session = Some(Session::spawn(None, &cwd, waker).unwrap_or_else(|error| {
            Session::fixture(Grid::message(&format!("failed to start shell: {error:#}")))
        }));
    }
    let mut changed = false;
    if let Some(session) = &state.session {
        if let Some(text) = session.take_copied() {
            tx.copy(text);
        }
        if let Some(text) = session.take_rejected_paste() {
            state.pending_paste = Some(text);
            changed = true;
        }
    }
    let grid = state.grid();
    // Kept while programs hide the cursor, as dropping the caret cancels a composition.
    if let Some(caret) = caret_rect(&grid) {
        state.caret = Some(caret);
    }
    changed |= grid.generation != state.drawn_generation;
    state.drawn_generation = grid.generation;
    changed
}

fn on_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if state.pending_paste.is_some() && matches!(event.key, Key::Enter | Key::Escape) {
        resolve_paste(state, event.key == Key::Enter);
        return true;
    }
    let plain_command = !event.mods.shift && !event.mods.alt && !event.mods.ctrl;
    match event.key {
        Key::Command('c') if plain_command => {
            if let Some(session) = &state.session {
                session.copy();
            }
            return true;
        }
        Key::Command('v') if plain_command => {
            tx.read_clipboard(
                |state: &mut State, content: &ClipboardContent, _: &mut Tx| {
                    if let Some(session) = &state.session
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
        &state.session,
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
            if let Some(session) = &state.session {
                session.key(input::map_text(committed, Mods::default(), None, None));
            }
            true
        }
        _ => false,
    }
}

fn show_scene(state: &mut State, bytes: &[u8]) {
    let grid = Grid::from_vt(bytes, 60, 12)
        .unwrap_or_else(|error| Grid::message(&format!("failed to build fixture: {error:#}")));
    state.session = Some(Session::fixture(grid));
}

fn main() {
    let spec = AppSpec::<State> {
        scenes: &[
            ("fixture", |state| show_scene(state, SCENE_FIXTURE)),
            ("unicode", |state| {
                show_scene(state, SCENE_UNICODE.as_bytes())
            }),
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
        .on_focus(|state, focused, _| {
            if let Some(session) = &state.session {
                session.focus(focused);
            }
        })
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

    #[test]
    fn tab_and_escape_reach_the_window_after_clicking_chrome() {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut runtime = Runtime::new(State::default(), view, (960.0, 640.0), env);
        runtime.on_key = Some(|_, _, _| {
            WINDOW_KEYS.fetch_add(1, Ordering::SeqCst);
            true
        });
        runtime.frame(0.0);
        let (x, y) = runtime.find("chrome").unwrap();
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
