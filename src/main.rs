mod font;
mod input;
mod render;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use rune::tok;
use rune::{App, AppSpec, Cx, El, KeyEvent, Tx, Waker, col, effect, el, row, text};

use session::{Grid, Session};

const TERMINAL_MARK: u32 = 1;
const SCENE_FIXTURE: &[u8] = b"\x1b[1;32magentd\x1b[0m fixture\r\n\x1b[31mred\x1b[0m \x1b[42;30m on green \x1b[0m \x1b[7minverse\x1b[0m\r\n$ ";

#[derive(Default)]
pub struct State {
    waker: Option<Waker>,
    session: Option<Session>,
    drawn_generation: u64,
}

impl State {
    fn grid(&self) -> Arc<Grid> {
        self.session
            .as_ref()
            .map_or_else(Arc::default, Session::grid)
    }
}

fn view(_: &State, cx: &mut Cx<State>) -> El {
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
                .min_h(0.0),
        )
}

fn poll(state: &mut State, _: f64, _: &mut Tx) -> bool {
    if state.session.is_none() {
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let waker = state.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
        state.session = Some(Session::spawn(None, &cwd, waker).unwrap_or_else(|error| {
            Session::fixture(Grid::message(&format!("failed to start shell: {error:#}")))
        }));
    }
    let generation = state.grid().generation;
    let changed = generation != state.drawn_generation;
    state.drawn_generation = generation;
    changed
}

fn on_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
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

fn main() {
    let spec = AppSpec::<State> {
        scenes: &[("fixture", |state| {
            let grid = Grid::from_vt(SCENE_FIXTURE, 60, 12).unwrap_or_else(|error| {
                Grid::message(&format!("failed to build fixture: {error:#}"))
            });
            state.session = Some(Session::fixture(grid));
        })],
        setup: Some(|state, waker| state.waker = Some(waker)),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("agentd", (960.0, 640.0), State::default, view)
    };
    App::new(spec)
        .compositor(render::TerminalCompositor::default())
        .run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Key, Mods, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WINDOW_KEYS: AtomicUsize = AtomicUsize::new(0);

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
