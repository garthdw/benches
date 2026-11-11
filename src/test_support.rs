use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rune::{Env, Key, KeyEvent, Mods, Runtime, Waker};

use crate::workspace::WindowId;
use crate::{Fleet, State, on_key, poll, show_workspace_scene, view};

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
