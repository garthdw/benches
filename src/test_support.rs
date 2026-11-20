use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rune::{Chord, ChordKey, ChordMod, Env, Key, KeyEvent, KeyPlatform, Mods, Runtime, Waker};

use crate::app::{State, on_key, poll};
use crate::keymap::Command;
use crate::scenes::show_workspace_scene;
use crate::view::view;
use crate::windows::{Fleet, WindowId};
use crate::workspace::{Project, Workspace};

// `started` keeps the first poll from spawning a real shell.
pub(crate) fn runtime_with(mut state: State) -> Runtime<State> {
    let env = Env {
        system_dark: true,
        scale: 2.0,
    };
    state.started = true;
    let mut runtime = Runtime::new(state, view, (960.0, 640.0), env);
    runtime.poll = Some(poll);
    runtime.on_key = Some(on_key);
    runtime.frame(0.0);
    runtime
}

pub(crate) fn workspace_state() -> State {
    let mut state = State::default();
    show_workspace_scene(&mut state);
    state
}

pub(crate) fn workspace_runtime() -> Runtime<State> {
    runtime_with(workspace_state())
}

pub(crate) fn palette_runtime() -> Runtime<State> {
    let mut runtime = workspace_runtime();
    press_command(&mut runtime, Command::Palette);
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

/// The modifiers the keyboard reports for these keys here: Rune sets `cmd` with ⌘ on macOS and
/// with Ctrl elsewhere.
fn held(ctrl: bool, alt: bool, shift: bool, logo: bool) -> Mods {
    let cmd = if cfg!(target_os = "macos") {
        logo
    } else {
        ctrl
    };
    Mods {
        shift,
        cmd,
        alt,
        ctrl,
        logo,
    }
}

/// The primary modifier held alone: ⌘ on macOS, Ctrl elsewhere.
pub(crate) fn command() -> Mods {
    let mac = cfg!(target_os = "macos");
    held(!mac, false, false, mac)
}

/// The key event the keyboard sends for `chord` here, such as `mod+shift+]`.
pub(crate) fn chord_event(chord: &str) -> KeyEvent {
    let chord = Chord::parse(chord).unwrap();
    let platform = KeyPlatform::current();
    let has = |modifier| chord.mods_on(platform).contains(&modifier);
    let primary = has(ChordMod::Primary);
    let mac = platform == KeyPlatform::Mac;
    let mods = held(
        has(ChordMod::Ctrl) || (primary && !mac),
        has(ChordMod::Alt),
        has(ChordMod::Shift),
        has(ChordMod::Super) || (primary && mac),
    );
    let key = match chord.key() {
        ChordKey::Char(character) => Key::Command(character),
        ChordKey::Enter => Key::Enter,
        ChordKey::Up => Key::Up,
        ChordKey::Down => Key::Down,
        ChordKey::PageUp => Key::PageUp,
        ChordKey::PageDown => Key::PageDown,
        key => panic!("no test key for {key:?}"),
    };
    KeyEvent { key, mods }
}

pub(crate) fn press_chord(runtime: &mut Runtime<State>, chord: &str) {
    runtime.key(chord_event(chord));
    runtime.frame(1.0 / 60.0);
}

/// Presses `command`'s shortcut on this platform.
pub(crate) fn press_command(runtime: &mut Runtime<State>, command: Command) {
    press_chord(runtime, command.chord().unwrap());
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
        ..State::default()
    };
    state.fleet.register(window, Arc::new(|| {}));
    runtime_with(state)
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
    workspace.add_project(Project::new("/dev/benches".into(), Vec::new()));
    workspace
}
