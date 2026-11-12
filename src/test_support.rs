use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use libghostty_vt::style::RgbColor;
use libghostty_vt::{Terminal, key, mouse};
use rune::{Env, Key, KeyEvent, Mods, Runtime, Waker};

use crate::app::{State, on_key, poll};
use crate::grid::new_terminal;
use crate::grid::{Metrics, Surface};
use crate::scenes::show_workspace_scene;
use crate::terminal_input::{PointerInput, PointerPhase, WheelInput};
use crate::view::view;
use crate::windows::{Fleet, WindowId};
use crate::workspace::{Project, Workspace};

pub(crate) fn runtime_with(state: State) -> Runtime<State> {
    let env = Env {
        system_dark: true,
        scale: 2.0,
    };
    let mut runtime = Runtime::new(state, view, (960.0, 640.0), env);
    runtime.frame(0.0);
    runtime
}

pub(crate) fn workspace_state() -> State {
    let mut state = State::default();
    show_workspace_scene(&mut state);
    state
}

pub(crate) fn workspace_runtime() -> Runtime<State> {
    let mut runtime = runtime_with(workspace_state());
    runtime.on_key = Some(on_key);
    runtime
}

pub(crate) fn palette_runtime() -> Runtime<State> {
    let mut runtime = workspace_runtime();
    press(&mut runtime, Key::Command('p'), command());
    runtime.frame(2.0 / 60.0);
    runtime
}

pub(crate) fn press(runtime: &mut Runtime<State>, key: Key, mods: Mods) {
    runtime.key(KeyEvent { key, mods });
    runtime.frame(1.0 / 60.0);
}

pub(crate) fn click(runtime: &mut Runtime<State>, key: &str) {
    let (x, y) = runtime.find(key).unwrap();
    runtime.pointer_down(x, y);
    runtime.pointer_up(x, y);
    runtime.frame(1.0 / 60.0);
}

pub(crate) fn hover(runtime: &mut Runtime<State>, key: &str) {
    let (x, y) = runtime.find(key).unwrap();
    runtime.pointer_move(x, y);
    runtime.frame(1.0 / 60.0);
}

pub(crate) fn settle(runtime: &mut Runtime<State>) {
    for frame in 2..120 {
        runtime.frame(frame as f64 / 60.0);
    }
}

pub(crate) fn command() -> Mods {
    Mods {
        cmd: true,
        logo: true,
        ..Mods::default()
    }
}

pub(crate) fn command_shift() -> Mods {
    Mods {
        shift: true,
        ..command()
    }
}

pub(crate) fn control() -> Mods {
    Mods {
        ctrl: true,
        ..Mods::default()
    }
}

pub(crate) fn counting_waker() -> (Waker, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let waker: Waker = {
        let count = count.clone();
        Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        })
    };
    (waker, count)
}

pub(crate) fn second_window(fleet: &Fleet, window: WindowId) -> Runtime<State> {
    let state = State {
        fleet: fleet.clone(),
        window,
        started: true,
        ..State::default()
    };
    state.fleet.register(window, Arc::new(|| {}));
    let mut runtime = runtime_with(state);
    runtime.poll = Some(poll);
    runtime
}

/// Stands in for the login shell, so no real `wt` runs. It records its arguments and runs `body`.
pub(crate) fn stub_shell(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("args");
    let shell = directory.path().join("stub-shell");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{body}\n",
        record.display()
    );
    std::fs::write(&shell, script).unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    (directory, shell, record)
}

pub(crate) fn wait_until<T>(
    timeout: Duration,
    describe: impl Fn() -> String,
    mut found: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(Instant::now() < deadline, "{}", describe());
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn one_project() -> Workspace {
    let mut workspace = Workspace::default();
    workspace.add_project(Project::new("/dev/agentd".into(), Vec::new()));
    workspace
}

pub(crate) fn scrolled_terminal<'cb>() -> Terminal<'static, 'cb> {
    let mut terminal = new_terminal(20, 5).unwrap();
    for line in 0..30 {
        terminal.vt_write(format!("line{line}\r\n").as_bytes());
    }
    terminal
}

pub(crate) fn fractional_metrics(scale: f32) -> Metrics {
    Metrics {
        cell_width: 9,
        cell_height: 19,
        padding: 12.0 * scale,
        scale,
    }
}

pub(crate) const THEME_RED: RgbColor = RgbColor {
    r: 0xff,
    g: 0,
    b: 0,
};
pub(crate) const THEME_BLUE: RgbColor = RgbColor {
    r: 0,
    g: 0,
    b: 0xff,
};

pub(crate) const TEST_SURFACE: Surface = Surface {
    cols: 20,
    rows: 5,
    metrics: Metrics {
        cell_width: 10,
        cell_height: 20,
        padding: 0.0,
        scale: 1.0,
    },
};

pub(crate) fn pointer_at(phase: PointerPhase, column: u16, row: u16) -> PointerInput {
    PointerInput {
        phase,
        button: Some(mouse::Button::Left),
        x: f32::from(column) * 10.0 + 5.0,
        y: f32::from(row) * 20.0 + 10.0,
        mods: key::Mods::empty(),
    }
}

pub(crate) fn wheel_by(dy: f32) -> WheelInput {
    WheelInput {
        dy,
        x: 5.0,
        y: 10.0,
        mods: key::Mods::empty(),
    }
}
