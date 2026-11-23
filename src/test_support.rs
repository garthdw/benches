use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::kill;
use nix::unistd::Pid;
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
    let cmd = if KeyPlatform::current() == KeyPlatform::Mac {
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
    let mac = KeyPlatform::current() == KeyPlatform::Mac;
    held(!mac, false, false, mac)
}

/// The key event a US keyboard sends for `chord` here: `mod+shift+]` is ⌘ with "}" on macOS, and
/// `alt+2` is Alt with the text "2" on Linux, as Rune only makes a `Command` key under `cmd`.
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
        ChordKey::Char(character) => {
            let typed = if mods.shift {
                us_shifted(character)
            } else {
                character
            };
            if mods.cmd {
                Key::Command(typed.to_ascii_lowercase())
            } else {
                Key::Text(typed.to_string())
            }
        }
        ChordKey::Enter => Key::Enter,
        ChordKey::Up => Key::Up,
        ChordKey::Down => Key::Down,
        ChordKey::PageUp => Key::PageUp,
        ChordKey::PageDown => Key::PageDown,
        key => panic!("no test key for {key:?}"),
    };
    KeyEvent { key, mods }
}

/// The character Shift and `character` type on a US layout.
fn us_shifted(character: char) -> char {
    let (plain, shifted) = ("`1234567890-=[]\\;',./", "~!@#$%^&*()_+{}|:\"<>?");
    plain
        .chars()
        .zip(shifted.chars())
        .find(|&(key, _)| key == character)
        .map_or(character.to_ascii_uppercase(), |(_, typed)| typed)
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
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("args");
    let shell = directory.path().join("stub-shell");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{body}\n",
        record.display()
    );
    // Written by another process: a child forked meanwhile by one of this process's threads
    // would hold its write fd until it execs, and Linux refuses to run a file open for writing
    // (ETXTBSY), even under another name.
    let written = std::process::Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$0" && chmod 755 "$0""#])
        .arg(&shell)
        .arg(script)
        .status()
        .unwrap();
    assert!(written.success(), "stub shell not written: {written}");
    (directory, shell, record)
}

/// `program` as found on `PATH`, which on NixOS is the only place a shell such as bash lives.
/// Says so when it's missing, as the caller then skips its checks.
pub(crate) fn installed(program: &str) -> Option<PathBuf> {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(program))
        .find(|path| {
            use std::os::unix::fs::PermissionsExt;
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        });
    if found.is_none() {
        eprintln!("skipped: no {program} on PATH");
    }
    found
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

/// The pid a test command wrote to `file`.
pub(crate) fn wait_for_pid(file: &Path) -> i32 {
    wait_until(
        Duration::from_secs(15),
        || format!("no pid in {}", file.display()),
        || std::fs::read_to_string(file).ok()?.trim().parse().ok(),
    )
}

/// Waits for `pid` to be reaped, as a zombie still answers `kill`.
pub(crate) fn wait_for_pid_exit(pid: i32, timeout: Duration, what: &str) {
    wait_until(
        timeout,
        || format!("{what} is still running"),
        || kill(Pid::from_raw(pid), None).is_err().then_some(()),
    );
}

/// Runs frames until `done`, so polls pick up what live sessions do, and returns the last
/// frame's number for a test that runs more after it.
pub(crate) fn frame_until(
    runtime: &mut Runtime<State>,
    waiting_for: &str,
    done: impl Fn(&Runtime<State>) -> bool,
) -> f64 {
    let mut frame = 1.0;
    wait_until(
        Duration::from_secs(15),
        || format!("timed out waiting for {waiting_for}"),
        || {
            if done(runtime) {
                return Some(frame);
            }
            frame += 1.0;
            runtime.frame(frame / 60.0);
            None
        },
    )
}

pub(crate) fn one_project() -> Workspace {
    let mut workspace = Workspace::default();
    workspace.add_project(Project::new("/dev/benches".into(), Vec::new()));
    workspace
}
