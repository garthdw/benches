use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use libghostty_vt::effect::ClipboardLocation;
use nix::pty::{Winsize, openpty};
use nix::sys::select::{FdSet, select};
use nix::sys::time::{TimeVal, TimeValLike};
use rune::Waker;

use crate::LogErr;
use crate::actor::{Actor, store, terminate};
use crate::config::Config;
use crate::grid::{Grid, Surface, bundled_terminfo, terminfo_name};
use crate::protocol::{Event, Msg, Visibility};
use crate::terminal_input::{KeyInput, PointerInput, PointerPhase, WheelInput};

const BATCH_GAP: Duration = Duration::from_micros(50);
const BATCH_LIMIT: Duration = Duration::from_millis(1);

/// macOS hands PTY output over about 1 KB at a time, so waiting up to `gap` for the next piece, for
/// at most `limit`, keeps the actor from waking for every kilobyte of a flood.
fn read_batch(
    reader: &mut File,
    buffer: &mut [u8],
    gap: Duration,
    limit: Duration,
) -> std::io::Result<usize> {
    let mut length = reader.read(buffer)?;
    let deadline = Instant::now() + limit;
    while length > 0 && length < buffer.len() && Instant::now() < deadline && readable(reader, gap)
    {
        match reader.read(&mut buffer[length..]) {
            Ok(read @ 1..) => length += read,
            // Ending the batch here leaves the next read to report the end or the error.
            Ok(0) | Err(_) => break,
        }
    }
    Ok(length)
}

fn readable(reader: &File, wait: Duration) -> bool {
    // `FdSet::insert` panics past `FD_SETSIZE`; such a descriptor just reads unbatched.
    if usize::try_from(reader.as_raw_fd()).is_ok_and(|fd| fd >= libc::FD_SETSIZE) {
        return false;
    }
    let mut fds = FdSet::new();
    fds.insert(reader.as_fd());
    let mut timeout = TimeVal::microseconds(i64::try_from(wait.as_micros()).unwrap_or(i64::MAX));
    select(None, &mut fds, None, None, &mut timeout).is_ok_and(|ready| ready > 0)
}

/// A waker whose target can change, so a session keeps waking the window that shows it after a move.
#[derive(Clone)]
pub struct Retarget(Arc<Mutex<Waker>>);

impl Retarget {
    pub fn new(waker: Waker) -> Self {
        Self(Arc::new(Mutex::new(waker)))
    }

    pub fn set(&self, waker: Waker) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = waker;
    }

    pub fn waker(&self) -> Waker {
        let target = self.clone();
        Arc::new(move || {
            let waker = target
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            waker();
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    #[default]
    None,
    /// A bell or desktop notification (OSC 9 or 777).
    Alert,
}

/// Apps launched from Finder get no locale, which leaves children in the C locale and mangles
/// UTF-8, so give them the user's macOS locale (`system`, such as `en_GB@rg=zazzzz`) in UTF-8.
fn missing_locale(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    system: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let has_locale = ["LANG", "LC_ALL", "LC_CTYPE"]
        .into_iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()));
    if has_locale {
        return None;
    }
    let candidate = system()
        .as_deref()
        .and_then(|identifier| identifier.split('@').next())
        .map(|language| format!("{language}.UTF-8"))
        .filter(|candidate| locale_exists(candidate));
    Some(candidate.unwrap_or_else(|| "en_US.UTF-8".to_owned()))
}

fn locale_exists(name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: `name` is a valid C string; a returned locale is freed at once.
    unsafe {
        let locale = libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            return false;
        }
        libc::freelocale(locale);
    }
    true
}

fn system_locale() -> Option<String> {
    let identifier = objc2_core_foundation::CFLocale::current()?.identifier()?;
    Some(identifier.to_string())
}

/// A process on a PTY. Its `Terminal` lives on an actor thread, as libghostty types are `!Send`.
pub struct Session {
    id: u64,
    /// Cleared by the actor before it reaps the process, after which the pid may be reused.
    pid: Arc<Mutex<Option<i32>>>,
    commands: Option<Sender<Msg>>,
    grid: Arc<Mutex<Arc<Grid>>>,
    /// Set by the actor when an alert arrives while not shown; cleared when it is shown.
    alerted: Arc<AtomicBool>,
    link_hovered: Arc<AtomicBool>,
    /// The directory the shell last reported with OSC 7, which is right inside ssh or a subshell.
    reported_cwd: Arc<Mutex<Option<PathBuf>>>,
    events: Receiver<Event>,
    sent_surface: std::cell::Cell<Option<Surface>>,
    visibility: std::cell::Cell<Visibility>,
    /// Starts as the actor does.
    focused: std::cell::Cell<bool>,
    /// Disconnects once the actor has ended the process, after the session is dropped.
    finished: Receiver<()>,
}

impl Session {
    pub fn spawn(command: Option<&str>, cwd: &Path, config: &Config, waker: Waker) -> Result<Self> {
        let (cols, rows) = (80, 24);
        let size = Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None)?;
        set_cloexec(&pty.master)?;
        set_cloexec(&pty.slave)?;

        let mut process = Command::new(&config.shell);
        process.args(["-l", "-i"]);
        if let Some(command) = command {
            process.arg("-c").arg(command);
        }
        let terminfo = std::env::current_exe()
            .ok()
            .and_then(|exe| bundled_terminfo(&exe));
        if let Some(dir) = &terminfo {
            process.env("TERMINFO", dir);
        }
        process
            .current_dir(cwd)
            .env("TERM", terminfo_name(terminfo.as_deref()))
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "agentd")
            .env("SHELL", &config.shell)
            .stdin(Stdio::from(pty.slave));
        if let Some(locale) = missing_locale(|name| std::env::var_os(name), system_locale) {
            process.env("LANG", locale);
        }
        // SAFETY: only async-signal-safe calls run between fork and exec.
        unsafe {
            process.pre_exec(|| {
                if libc::login_tty(0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Cloned before spawning so a failure can't leave a started child unreaped.
        let mut reader = File::from(pty.master.try_clone()?);
        let mut writer = File::from(pty.master.try_clone()?);
        let child = process.spawn()?;
        let pid = Arc::new(Mutex::new(Some(child.id() as i32)));

        let master = pty.master;
        let (output_tx, output_rx) = bounded(64);
        thread::spawn(move || {
            let mut buffer = [0; 64 * 1024];
            while let Ok(length @ 1..) =
                read_batch(&mut reader, &mut buffer, BATCH_GAP, BATCH_LIMIT)
            {
                if output_tx.send(buffer[..length].to_vec()).is_err() {
                    break;
                }
            }
        });
        let (input_tx, input_rx) = unbounded::<Vec<u8>>();
        thread::spawn(move || {
            for bytes in input_rx {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });

        let (commands, commands_rx) = unbounded();
        let grid = Arc::new(Mutex::new(Arc::new(Grid::default())));
        let alerted = Arc::new(AtomicBool::new(false));
        let link_hovered = Arc::new(AtomicBool::new(false));
        let reported_cwd = Arc::new(Mutex::new(None));
        let (events_tx, events) = unbounded();
        let (finished_tx, finished) = bounded::<()>(0);
        let mut actor = Actor {
            cols,
            rows,
            output: output_rx,
            commands: commands_rx,
            input: input_tx,
            child,
            pid: pid.clone(),
            master,
            slot: grid.clone(),
            alerted: alerted.clone(),
            link_hovered: link_hovered.clone(),
            reported_cwd: reported_cwd.clone(),
            events: events_tx,
            waker,
            settings: config.terminal(),
        };
        thread::spawn(move || {
            if let Err(error) = actor.run() {
                store(
                    &actor.slot,
                    Grid::message(&format!("terminal failed: {error:#}")),
                );
                (actor.waker)();
            }
            terminate(&mut actor.child, &actor.pid);
            drop(finished_tx);
        });
        Ok(Self {
            id: next_id(),
            pid,
            commands: Some(commands),
            grid,
            alerted,
            link_hovered,
            reported_cwd,
            events,
            sent_surface: std::cell::Cell::new(None),
            visibility: std::cell::Cell::new(Visibility::Shown),
            focused: std::cell::Cell::new(true),
            finished,
        })
    }

    /// A session with no process, showing `grid`. Input and resizes go nowhere.
    pub fn fixture(grid: Grid) -> Self {
        Self {
            id: next_id(),
            pid: Arc::default(),
            commands: None,
            grid: Arc::new(Mutex::new(Arc::new(grid))),
            alerted: Arc::default(),
            link_hovered: Arc::default(),
            reported_cwd: Arc::default(),
            events: unbounded().1,
            sent_surface: std::cell::Cell::new(None),
            visibility: std::cell::Cell::new(Visibility::Shown),
            focused: std::cell::Cell::new(true),
            finished: bounded(0).1,
        }
    }

    #[cfg(test)]
    pub fn publish_fixture(&self, grid: Grid) {
        store(&self.grid, grid);
    }

    #[cfg(test)]
    pub fn set_link_hovered(&self, hovered: bool) {
        self.link_hovered.store(hovered, Ordering::Release);
    }

    /// The process's pid and its `proc_pidinfo` of `flavor`, which must fill a `T`.
    fn pid_info<T>(&self, flavor: libc::c_int) -> Option<(i32, T)> {
        // Held through the lookup so the actor can't reap the process, freeing its pid, meanwhile.
        let pid = (*self.pid.lock().unwrap_or_else(PoisonError::into_inner))?;
        let mut info = std::mem::MaybeUninit::<T>::zeroed();
        let size = std::mem::size_of::<T>() as i32;
        // SAFETY: `info` is a writable buffer of `size` bytes.
        let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
        // SAFETY: the kernel filled the whole struct, and zeroed bytes are valid for it anyway.
        (written == size).then(|| (pid, unsafe { info.assume_init() }))
    }

    /// The shell's current directory, as it reports it, else the process's, which follows the
    /// shell's `cd`.
    pub fn cwd(&self) -> Option<PathBuf> {
        let reported = self
            .reported_cwd
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        reported.or_else(|| self.process_cwd())
    }

    fn process_cwd(&self) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let (_, info) = self.pid_info::<libc::proc_vnodepathinfo>(libc::PROC_PIDVNODEPATHINFO)?;
        let path = info.pvi_cdir.vip_path;
        // SAFETY: `vip_path` is a flat 1024-byte buffer split into rows by libc.
        let bytes: &[u8; 1024] = unsafe { &*path.as_ptr().cast() };
        let path = std::ffi::CStr::from_bytes_until_nul(bytes).ok()?;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
    }

    /// True while a program other than the shell holds the terminal's foreground.
    pub fn busy(&self) -> bool {
        self.pid_info::<libc::proc_bsdinfo>(libc::PROC_PIDTBSDINFO)
            .is_some_and(|(pid, info)| info.e_tpgid != pid as u32)
    }

    pub fn finished(&self) -> Receiver<()> {
        self.finished.clone()
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn grid(&self) -> Arc<Grid> {
        self.grid
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn key(&self, key: KeyInput) {
        self.send(Msg::Key(key));
    }

    pub fn focus(&self, focused: bool) {
        if self.focused.replace(focused) != focused {
            self.send(Msg::Focus(focused));
        }
    }

    pub fn pointer(&self, input: PointerInput) {
        self.send(Msg::Pointer(input));
    }

    pub fn wheel(&self, input: WheelInput) {
        self.send(Msg::Wheel(input));
    }

    /// `y` is in points from the top of the terminal surface.
    pub fn drag_scrollbar(&self, phase: PointerPhase, y: f32) {
        self.send(Msg::Scrollbar { phase, y });
    }

    /// Whether ⌘ is held over a link, which the terminal shows with a pointing hand.
    pub fn link_hovered(&self) -> bool {
        self.link_hovered.load(Ordering::Acquire)
    }

    pub fn command_held(&self, held: bool) {
        self.send(Msg::CommandHeld(held));
    }

    pub fn hover_scrollbar(&self, hovered: bool) {
        self.send(Msg::ScrollbarHover(hovered));
    }

    pub fn jump_prompt(&self, delta: isize) {
        self.send(Msg::PromptJump(delta));
    }

    pub fn select_last_output(&self) {
        self.send(Msg::SelectLastOutput);
    }

    /// Applies the theme, clipboard access and command-finish threshold of a reloaded config.
    pub fn configure(&self, config: &Config) {
        self.send(Msg::Configure(config.terminal()));
    }

    pub fn copy(&self) {
        self.send(Msg::Copy);
    }

    pub fn paste(&self, text: String, allow_unsafe: bool) {
        self.send(Msg::Paste { text, allow_unsafe });
    }

    pub fn search(&self, query: String) {
        self.send(Msg::Search(query));
    }

    pub fn search_step(&self, older: bool) {
        self.send(Msg::SearchStep { older });
    }

    pub fn clipboard_reply(&self, location: ClipboardLocation, text: Option<String>) {
        self.send(Msg::ClipboardReply { location, text });
    }

    /// Ends the process but keeps its last screen.
    pub fn stop(&self) {
        self.send(Msg::Stop);
    }

    pub fn next_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Sends `surface` unless it was the last one sent, so every tab can be sized each frame.
    pub fn resize(&self, surface: Surface) {
        if self.sent_surface.replace(Some(surface)) != Some(surface) {
            self.send(Msg::Resize(surface));
        }
    }

    /// A session that isn't shown keeps running but stops publishing grids. Sessions start shown.
    pub fn set_visibility(&self, visibility: Visibility) {
        if self.visibility.replace(visibility) != visibility {
            self.send(Msg::Visibility(visibility));
        }
    }

    /// A shown session has been seen, so it reads as none.
    pub fn attention(&self) -> Attention {
        if self.visibility.get() != Visibility::Shown && self.alerted.load(Ordering::Acquire) {
            Attention::Alert
        } else {
            Attention::None
        }
    }

    /// For fixtures, which have no actor to raise it.
    pub fn alert(&self) {
        self.alerted.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub fn visibility(&self) -> Visibility {
        self.visibility.get()
    }

    #[cfg(test)]
    pub fn focused(&self) -> bool {
        self.focused.get()
    }

    fn send(&self, message: Msg) {
        if let Some(commands) = &self.commands {
            commands.send(message).log_err("send to session");
        }
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn set_cloexec(fd: &OwnedFd) -> Result<()> {
    use nix::fcntl::{F_SETFD, FdFlag, fcntl};
    fcntl(fd, F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Theme;
    use crate::grid::{FRAME, GridBuilder, Presentation, Scrollbar, new_terminal};
    use crate::protocol::{Progress, ProgressKind};
    use crate::terminal_input::TerminalInput;
    use crate::test_support::{TEST_SURFACE, pointer_at, wait_until, wheel_by};
    use libghostty_vt::color::Rgb;
    use libghostty_vt::render::CursorVisualStyle;
    use nix::errno::Errno;
    use nix::unistd::Pid;
    use std::process::ExitStatus;
    use std::time::Duration;

    fn spawn_shell() -> Session {
        Session::spawn(None, Path::new("/tmp"), &Config::default(), Arc::new(|| {})).unwrap()
    }

    #[test]
    fn output_written_in_pieces_arrives_in_one_batch() {
        let pty = openpty(None, None).unwrap();
        let mut slave = File::from(pty.slave);
        let writer = thread::spawn(move || {
            for _ in 0..4 {
                slave.write_all(&[b'x'; 1000]).unwrap();
            }
            slave
        });
        let mut master = File::from(pty.master);
        let mut buffer = [0; 64 * 1024];
        let length = read_batch(
            &mut master,
            &mut buffer,
            Duration::from_millis(500),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(length, 4000);
        writer.join().unwrap();
    }

    fn type_line(session: &Session, line: &str) {
        for character in line.chars() {
            session.key(crate::input::map_text(
                &character.to_string(),
                rune::Mods::default(),
                None,
                None,
            ));
        }
        session.key(
            crate::input::map_key(
                &rune::KeyEvent {
                    key: rune::Key::Enter,
                    mods: rune::Mods::default(),
                },
                None,
                None,
            )
            .unwrap(),
        );
    }

    fn wait_for_line(session: &Session, want: &str) {
        wait_until(
            Duration::from_secs(15),
            || format!("{want} never arrived: {:#?}", session.grid().lines()),
            || {
                let lines = session.grid().lines();
                lines
                    .iter()
                    .any(|line| line.trim_end() == want)
                    .then_some(())
            },
        );
    }

    fn wait_for_busy(session: &Session, busy: bool) {
        wait_until(
            Duration::from_secs(15),
            || format!("never {}", if busy { "busy" } else { "idle" }),
            || (session.busy() == busy).then_some(()),
        );
    }

    #[test]
    fn a_paste_reaches_a_real_shell() {
        let session = spawn_shell();
        session.paste("echo pasted-ok".to_owned(), false);
        type_line(&session, "");
        wait_for_line(&session, "pasted-ok");
    }

    #[test]
    fn a_search_finds_output_in_a_real_shell_and_steps_between_matches() {
        let session = spawn_shell();
        type_line(&session, "printf 'nee%sle\\n' d d d");
        session.search("needle".to_owned());
        wait_until(
            Duration::from_secs(15),
            || "no matches".to_owned(),
            || (session.grid().found.map(|found| found.total) == Some(3)).then_some(()),
        );
        let selected = session.grid().found.and_then(|found| found.selected);
        session.search_step(true);
        wait_until(
            Duration::from_secs(15),
            || "never stepped".to_owned(),
            || (session.grid().found.and_then(|found| found.selected) != selected).then_some(()),
        );
        session.search(String::new());
        wait_until(
            Duration::from_secs(15),
            || "search never ended".to_owned(),
            || session.grid().found.is_none().then_some(()),
        );
    }

    #[test]
    fn a_refused_paste_comes_back_to_the_ui() {
        let session = spawn_shell();
        // Refused either way, as the shell may not have enabled bracketed paste yet.
        let text = "x\n\x1b[201~y";
        session.paste(text.to_owned(), false);
        let refused = wait_until(
            Duration::from_secs(5),
            || "the paste was never refused".to_owned(),
            || match session.next_event() {
                Some(Event::PasteRejected(text)) => Some(text),
                _ => None,
            },
        );
        assert_eq!(refused, text);
    }

    #[test]
    fn a_clipboard_read_is_answered_once_the_ui_replies() {
        // The tty echoes what `cat` is sent, so the reply shows on the grid.
        let session = Session::spawn(
            Some(r#"sh -c 'printf "\\033]52;c;?\\007"; cat'"#),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let location = wait_until(
            Duration::from_secs(5),
            || "the read never reached the UI".to_owned(),
            || match session.next_event() {
                Some(Event::ClipboardRead {
                    location,
                    ask: true,
                }) => Some(location),
                _ => None,
            },
        );
        session.clipboard_reply(location, Some("hi".to_owned()));
        wait_until(
            Duration::from_secs(5),
            || {
                format!(
                    "the reply never reached the program: {:#?}",
                    session.grid().lines()
                )
            },
            || {
                (session.grid().lines().iter())
                    .any(|line| line.contains("]52;c;aGk="))
                    .then_some(())
            },
        );
    }

    #[test]
    fn pointer_input_before_the_first_resize_is_ignored() {
        let terminal = new_terminal(20, 5).unwrap();
        let mut input = TerminalInput::new().unwrap();
        let bytes = input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 0, 0))
            .unwrap();
        assert!(bytes.is_empty() && !input.selection_changed);
    }

    #[test]
    fn a_burst_of_resizes_ends_at_the_last_size() {
        let session = spawn_shell();
        for cols in 40..60 {
            session.resize(Surface {
                cols,
                ..TEST_SURFACE
            });
        }
        wait_until(
            Duration::from_secs(5),
            || format!("cols: {}", session.grid().cols),
            || (session.grid().cols == 59).then_some(()),
        );
    }

    #[test]
    fn a_settled_resize_publishes_without_output() {
        let session = Session::spawn(
            Some("cat"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        session.resize(TEST_SURFACE);
        wait_until(
            Duration::from_secs(5),
            || format!("cols: {}", session.grid().cols),
            || (session.grid().cols == TEST_SURFACE.cols).then_some(()),
        );
        session.resize(Surface {
            cols: 30,
            ..TEST_SURFACE
        });
        wait_until(
            Duration::from_secs(5),
            || format!("cols: {}", session.grid().cols),
            || (session.grid().cols == 30).then_some(()),
        );
    }

    #[test]
    fn a_cursor_style_change_alone_publishes_a_new_grid() {
        let shown = Presentation::default();
        let mut builder = GridBuilder::new().unwrap();
        let mut terminal = new_terminal(10, 2).unwrap();
        builder.build(&terminal, 1, shown).unwrap();
        terminal.vt_write(b"\x1b[6 q");
        let grid = builder.build(&terminal, 2, shown).unwrap().unwrap();
        assert_eq!(grid.cursor_style, CursorVisualStyle::Bar);
    }

    #[test]
    fn a_selection_change_republishes_an_unchanged_terminal() {
        let shown = Presentation::default();
        let mut builder = GridBuilder::new().unwrap();
        let terminal = new_terminal(10, 2).unwrap();
        builder.build(&terminal, 1, shown).unwrap();
        builder.selection_changed();
        assert!(builder.build(&terminal, 2, shown).unwrap().is_some());
    }

    #[test]
    fn a_presentation_change_alone_publishes_a_new_grid() {
        let shown = Presentation::default();
        let mut builder = GridBuilder::new().unwrap();
        let terminal = new_terminal(10, 2).unwrap();
        builder.build(&terminal, 1, shown).unwrap();
        assert!(builder.build(&terminal, 2, shown).unwrap().is_none());
        let blurred = Presentation {
            focused: false,
            ..shown
        };
        let grid = builder.build(&terminal, 3, blurred).unwrap().unwrap();
        assert_eq!(grid.cursor_style, CursorVisualStyle::BlockHollow);
    }

    #[test]
    fn a_hidden_cursor_does_not_blink() {
        let grid = Grid::from_vt(b"\x1b[1 q\x1b[?25l", 8, 1).unwrap();
        assert!(!grid.cursor_blinking);
    }

    #[test]
    fn output_after_a_hold_that_never_ends_still_appears() {
        let session = spawn_shell();
        type_line(&session, "printf '\\033[?2026h'; echo held-2; sleep 30");
        wait_for_line(&session, "held-2");
    }

    fn next_title(session: &Session) -> Option<String> {
        std::iter::from_fn(|| session.next_event()).find_map(|event| match event {
            Event::Title(title) => Some(title),
            _ => None,
        })
    }

    #[test]
    fn a_hidden_session_holds_its_title_until_listed() {
        let session = spawn_shell();
        session.set_visibility(Visibility::Hidden);
        type_line(&session, "printf '\\033]2;held-title\\007'; sleep 5");
        thread::sleep(Duration::from_secs(1));
        assert_eq!(next_title(&session), None);
        session.set_visibility(Visibility::Listed);
        wait_until(
            Duration::from_secs(5),
            || "the held title never arrived".to_owned(),
            || next_title(&session).filter(|title| title == "held-title"),
        );
    }

    #[test]
    fn a_hidden_session_publishes_once_shown() {
        let session = spawn_shell();
        session.set_visibility(Visibility::Hidden);
        type_line(&session, "echo hidden-ok");
        thread::sleep(Duration::from_secs(1));
        let lines = session.grid().lines();
        assert!(!lines.iter().any(|line| line.trim_end() == "hidden-ok"));
        session.set_visibility(Visibility::Shown);
        wait_for_line(&session, "hidden-ok");
    }

    fn shell_with_history() -> Session {
        let session = spawn_shell();
        session.resize(TEST_SURFACE);
        type_line(&session, "seq 100");
        wait_for_line(&session, "100");
        session
    }

    fn wait_for_scrollbar(session: &Session, want: &str, shown: impl Fn(Scrollbar) -> bool) {
        wait_until(
            Duration::from_secs(5),
            || format!("scrollbar never {want}: {:?}", session.grid().scrollbar),
            || session.grid().scrollbar.filter(|bar| shown(*bar)),
        );
    }

    #[test]
    fn hiding_a_session_releases_its_scrollbar_hover() {
        let session = shell_with_history();
        session.hover_scrollbar(true);
        wait_for_scrollbar(&session, "widened", |bar| bar.wide);
        session.set_visibility(Visibility::Listed);
        session.set_visibility(Visibility::Shown);
        wait_for_scrollbar(&session, "narrowed", |bar| !bar.wide);
    }

    #[test]
    fn hiding_a_session_ends_its_scrollbar_drag() {
        let session = shell_with_history();
        session.drag_scrollbar(PointerPhase::Press, 10.0);
        wait_for_scrollbar(&session, "widened", |bar| bar.wide);
        session.set_visibility(Visibility::Listed);
        session.set_visibility(Visibility::Shown);
        wait_for_scrollbar(&session, "narrowed", |bar| !bar.wide);
    }

    #[test]
    fn a_settled_resize_leaves_the_scrollbar_hidden() {
        let session = spawn_shell();
        session.resize(TEST_SURFACE);
        // Lines that wrap, so the reflow moves the viewport through the scrollback.
        type_line(&session, "printf '%060d\\n' $(seq 30)");
        wait_for_scrollbar(&session, "long", |bar| bar.total > 90);
        session.resize(Surface {
            cols: 60,
            ..TEST_SURFACE
        });
        wait_until(
            Duration::from_secs(5),
            || "the resize never applied".to_owned(),
            || (session.grid().cols == 60).then_some(()),
        );
        thread::sleep(FRAME * 4);
        assert_eq!(session.grid().scrollbar.map(|bar| bar.opacity), Some(0.0));
    }

    #[test]
    fn a_wheel_past_the_bottom_still_flashes_the_scrollbar() {
        let session = shell_with_history();
        session.wheel(wheel_by(-60.0));
        wait_for_scrollbar(&session, "shown", |bar| bar.opacity > 0.0);
    }

    #[test]
    fn a_hidden_flood_leaves_another_session_responsive() {
        let flood = spawn_shell();
        flood.set_visibility(Visibility::Hidden);
        type_line(&flood, "yes");
        thread::sleep(Duration::from_millis(500));
        let generation = flood.grid().generation;
        let typing = spawn_shell();
        type_line(&typing, "echo still-responsive");
        wait_for_line(&typing, "still-responsive");
        assert_eq!(flood.grid().generation, generation);
    }

    #[test]
    fn a_program_title_comes_back_to_the_ui() {
        let session = spawn_shell();
        type_line(&session, "printf '\\033]2;agentd-title\\007'; sleep 5");
        wait_until(
            Duration::from_secs(15),
            || "the title never arrived".to_owned(),
            || match session.next_event() {
                Some(Event::Title(title)) if title == "agentd-title" => Some(()),
                _ => None,
            },
        );
    }

    #[test]
    fn a_repeated_title_is_sent_once() {
        let session = spawn_shell();
        type_line(
            &session,
            "printf '\\033]2;same\\007'; sleep 0.2; printf '\\033]2;same\\007'; sleep 0.2; printf '\\033]2;done\\007'",
        );
        let mut titles = Vec::new();
        wait_until(
            Duration::from_secs(15),
            || "the last title never arrived".to_owned(),
            || {
                while let Some(event) = session.next_event() {
                    if let Event::Title(title) = event {
                        titles.push(title);
                    }
                }
                titles.contains(&"done".to_owned()).then_some(())
            },
        );
        assert_eq!(titles.iter().filter(|title| *title == "same").count(), 1);
    }

    #[test]
    fn resizing_during_a_render_hold_survives() {
        let session = spawn_shell();
        // The hold starts after the line shows, and the resize lands before it times out.
        type_line(
            &session,
            "echo held-resize; sleep 0.3; printf '\\033[?2026h'; sleep 30",
        );
        wait_for_line(&session, "held-resize");
        thread::sleep(Duration::from_millis(600));
        session.resize(Surface {
            cols: 33,
            ..TEST_SURFACE
        });
        wait_until(
            Duration::from_secs(5),
            || format!("cols: {}", session.grid().cols),
            || (session.grid().cols == 33).then_some(()),
        );
    }

    fn wait_for_exit(session: &Session) -> ExitStatus {
        wait_until(
            Duration::from_secs(15),
            || "the process never exited".to_owned(),
            || match session.next_event() {
                Some(Event::Exited(status)) => Some(status),
                _ => None,
            },
        )
    }

    #[test]
    fn an_exit_status_comes_back_to_the_ui() {
        let session = Session::spawn(
            Some("sh -c 'exit 3'"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        assert_eq!(wait_for_exit(&session).code(), Some(3));
    }

    #[test]
    fn a_command_may_use_shell_syntax() {
        let session = Session::spawn(
            Some("cd /usr && echo a | tr a b; exit 4"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_line(&session, "b");
        assert_eq!(wait_for_exit(&session).code(), Some(4));
    }

    /// A reaped pid can be reused, so `cwd` would read another process's folder.
    #[test]
    fn exited_and_stopped_sessions_forget_their_pid() {
        let exited = Session::spawn(
            Some("sh -c 'exit 0'"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        wait_for_exit(&exited);
        assert_eq!(*exited.pid.lock().unwrap(), None);

        let stopped = Session::spawn(
            Some("sleep 100"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(500));
        stopped.stop();
        wait_for_exit(&stopped);
        assert_eq!(*stopped.pid.lock().unwrap(), None);
    }

    #[test]
    fn the_working_directory_follows_the_shell() {
        let session = spawn_shell();
        assert_eq!(session.cwd(), Some(PathBuf::from("/private/tmp")));
        type_line(&session, "cd /usr/bin");
        wait_until(
            Duration::from_secs(5),
            || format!("cwd is {:?}", session.cwd()),
            || (session.cwd() == Some(PathBuf::from("/usr/bin"))).then_some(()),
        );
        assert_eq!(Session::fixture(Grid::default()).cwd(), None);
    }

    #[test]
    fn the_reported_directory_wins_over_the_process_directory() {
        let fish = Path::new("/opt/homebrew/bin/fish");
        if !fish.exists() {
            return;
        }
        let config = Config {
            shell: fish.into(),
            ..Config::default()
        };
        let session = Session::spawn(None, Path::new("/tmp"), &config, Arc::new(|| {})).unwrap();
        // Fish reports the symlink `/tmp`, where the process's directory reads `/private/tmp`.
        type_line(&session, "cd /tmp");
        wait_until(
            Duration::from_secs(15),
            || format!("cwd is {:?}", session.cwd()),
            || (session.cwd() == Some(PathBuf::from("/tmp"))).then_some(()),
        );
    }

    #[test]
    fn stopping_ends_the_process_and_keeps_the_session() {
        let session = Session::spawn(
            Some("sleep 100"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(500));
        session.stop();
        assert!(!wait_for_exit(&session).success());
        wait_for_line(&session, "[process exited]");
    }

    fn wait_for_alert(session: &Session) {
        wait_until(
            Duration::from_secs(15),
            || "no alert while unseen".to_owned(),
            || (session.attention() == Attention::Alert).then_some(()),
        );
    }

    #[test]
    fn an_unseen_session_raises_attention_until_shown() {
        let session = spawn_shell();
        session.set_visibility(Visibility::Listed);
        type_line(&session, "printf '\\a'");
        wait_for_alert(&session);
        session.set_visibility(Visibility::Shown);
        assert_eq!(session.attention(), Attention::None);
        type_line(&session, "printf '\\a'; echo shown");
        wait_for_line(&session, "shown");
        assert_eq!(session.attention(), Attention::None);
        session.set_visibility(Visibility::Hidden);
        type_line(&session, "printf '\\a'");
        wait_for_alert(&session);
    }

    fn finished_events(session: &Session, count: usize) -> Vec<bool> {
        let mut failures = Vec::new();
        wait_until(
            Duration::from_secs(15),
            || format!("fewer than {count} commands finished"),
            || {
                while let Some(event) = session.next_event() {
                    if let Event::CommandFinished { failed } = event {
                        failures.push(failed);
                    }
                }
                (failures.len() >= count).then(|| failures.clone())
            },
        )
    }

    fn spawn_with_threshold(threshold: Duration) -> Session {
        let config = Config {
            command_finish_after: Some(threshold),
            ..Config::default()
        };
        Session::spawn(None, Path::new("/tmp"), &config, Arc::new(|| {})).unwrap()
    }

    // Marks are doubled, as when a shell and its integration both emit them.
    const SLOW_FAILURE: &str = "printf '\\033]133;C\\007\\033]133;C\\007'; sleep 0.4; \
                                printf '\\033]133;D;1\\007\\033]133;D;1\\007'";

    #[test]
    fn a_long_command_finishing_out_of_sight_raises_attention_once() {
        let session = spawn_with_threshold(Duration::from_millis(200));
        session.set_visibility(Visibility::Hidden);
        type_line(&session, SLOW_FAILURE);
        wait_for_alert(&session);
        assert_eq!(finished_events(&session, 1), [true]);
        assert_eq!(notify_events(&session), []);
    }

    #[test]
    fn configure_changes_when_a_running_session_counts_a_command_as_long() {
        let session = spawn_with_threshold(Duration::from_secs(3600));
        session.configure(&Config {
            command_finish_after: Some(Duration::from_millis(200)),
            ..Config::default()
        });
        session.set_visibility(Visibility::Hidden);
        type_line(&session, SLOW_FAILURE);
        wait_for_alert(&session);
    }

    #[test]
    fn configure_recolours_an_idle_session() {
        let session = spawn_shell();
        type_line(&session, "echo ready");
        wait_for_line(&session, "ready");
        let background = Rgb { r: 1, g: 2, b: 3 };
        session.configure(&Config {
            theme: Theme {
                background,
                ..Theme::default()
            },
            ..Config::default()
        });
        wait_until(
            Duration::from_secs(15),
            || format!("background is still {:?}", session.grid().bg),
            || (session.grid().bg == [1, 2, 3]).then_some(()),
        );
    }

    #[test]
    fn a_quick_command_finishing_out_of_sight_leaves_no_mark() {
        let session = spawn_with_threshold(Duration::from_secs(3600));
        session.set_visibility(Visibility::Listed);
        type_line(&session, SLOW_FAILURE);
        assert_eq!(finished_events(&session, 1), [true]);
        assert_eq!(session.attention(), Attention::None);
    }

    #[test]
    fn a_long_command_in_a_shown_tab_leaves_no_mark() {
        let session = spawn_with_threshold(Duration::from_millis(200));
        type_line(&session, SLOW_FAILURE);
        assert_eq!(finished_events(&session, 1), [true]);
        assert_eq!(session.attention(), Attention::None);
    }

    #[test]
    fn an_alert_landing_as_the_session_is_shown_leaves_no_dot() {
        let session = Session::fixture(Grid::default());
        session.set_visibility(Visibility::Listed);
        session.alert();
        assert_eq!(session.attention(), Attention::Alert);
        // The actor may raise the flag before it handles the visibility change.
        session.set_visibility(Visibility::Shown);
        session.alert();
        assert_eq!(session.attention(), Attention::None);
    }

    fn notify_events(session: &Session) -> Vec<(String, String)> {
        std::iter::from_fn(|| session.next_event())
            .filter_map(|event| match event {
                Event::Notify { title, body } => Some((title, body)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_session_without_keys_notifies_once_until_it_has_them() {
        let session = spawn_shell();
        session.focus(false);
        type_line(
            &session,
            "printf '\\033]777;notify;Done;Built\\a\\a'; echo one",
        );
        wait_for_line(&session, "one");
        assert_eq!(notify_events(&session), [("Done".into(), "Built".into())]);
        type_line(&session, "printf '\\a'; echo two");
        wait_for_line(&session, "two");
        assert_eq!(notify_events(&session), []);
        session.focus(true);
        type_line(&session, "printf '\\a'; echo three");
        wait_for_line(&session, "three");
        assert_eq!(notify_events(&session), []);
        session.focus(false);
        type_line(&session, "printf '\\a'; echo four");
        wait_for_line(&session, "four");
        assert_eq!(notify_events(&session), [(String::new(), String::new())]);
    }

    fn progress_events(session: &Session) -> Vec<Option<Progress>> {
        std::iter::from_fn(|| session.next_event())
            .filter_map(|event| match event {
                Event::Progress(progress) => Some(progress),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn osc_9_4_progress_comes_back_and_is_removed() {
        let session = spawn_shell();
        let reports = [
            ("1;50", Some((ProgressKind::Normal, 50))),
            ("2;70", Some((ProgressKind::Error, 70))),
            ("3", Some((ProgressKind::Indeterminate, 0))),
            ("4;20", Some((ProgressKind::Paused, 20))),
            ("0", None),
        ];
        for (index, (report, want)) in reports.into_iter().enumerate() {
            type_line(
                &session,
                &format!("printf '\\033]9;4;{report}\\a'; echo step{index}"),
            );
            wait_for_line(&session, &format!("step{index}"));
            let want = want.map(|(kind, percent)| Progress { kind, percent });
            assert_eq!(progress_events(&session), [want]);
        }
    }

    #[test]
    fn stale_progress_is_dropped() {
        let session = spawn_shell();
        type_line(&session, "printf '\\033]9;4;1;50\\a'");
        wait_until(
            Duration::from_secs(15),
            || "the progress never went stale".to_owned(),
            || {
                std::iter::from_fn(|| session.next_event())
                    .any(|event| matches!(event, Event::Progress(None)))
                    .then_some(())
            },
        );
    }

    #[test]
    fn progress_ends_when_the_process_exits() {
        let session = spawn_shell();
        type_line(&session, "printf '\\033]9;4;1;50\\a'; exit");
        let mut events = Vec::new();
        wait_until(
            Duration::from_secs(15),
            || "the process never exited".to_owned(),
            || {
                while let Some(event) = session.next_event() {
                    match event {
                        Event::Progress(progress) => events.push(progress),
                        Event::Exited(_) => return Some(()),
                        _ => {}
                    }
                }
                None
            },
        );
        assert_eq!(events.last(), Some(&None));
    }

    #[test]
    fn a_status_query_is_answered() {
        let session = spawn_shell();
        type_line(&session, "printf '\\033]7501;?\\033\\\\'; cat -v");
        type_line(&session, "end");
        wait_until(
            Duration::from_secs(15),
            || format!("no reply: {:#?}", session.grid().lines()),
            || {
                session
                    .grid()
                    .lines()
                    .iter()
                    .any(|line| {
                        line.contains(
                            "7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth",
                        )
                    })
                    .then_some(())
            },
        );
    }

    #[test]
    fn a_blocked_program_alerts_and_notifies_once_until_seen() {
        let session = spawn_shell();
        session.focus(false);
        session.set_visibility(Visibility::Listed);
        type_line(
            &session,
            "printf '\\033]7501;state=blocked:kind=permission\\033\\\\'",
        );
        wait_for_alert(&session);
        assert_eq!(
            notify_events(&session),
            [(String::new(), "Needs permission".into())]
        );
        type_line(
            &session,
            "printf '\\a\\033]7501;state=blocked:kind=question\\033\\\\'; echo two",
        );
        session.set_visibility(Visibility::Shown);
        wait_for_line(&session, "two");
        assert_eq!(notify_events(&session), []);
    }

    #[test]
    fn a_blocked_prompt_with_an_osc_notification_notifies_once() {
        let session = spawn_shell();
        session.focus(false);
        type_line(
            &session,
            "printf '\\033]777;notify;Claude;Allow edit?\\a\\033]7501;state=blocked:kind=permission\\033\\\\'; echo one",
        );
        wait_for_line(&session, "one");
        assert_eq!(
            notify_events(&session),
            [("Claude".into(), "Allow edit?".into())]
        );
    }

    #[test]
    fn other_program_states_do_not_alert() {
        let session = spawn_shell();
        session.focus(false);
        session.set_visibility(Visibility::Listed);
        type_line(
            &session,
            "printf '\\033]7501;state=working\\a'; printf '\\033]7501;state=done\\a'; echo one",
        );
        session.set_visibility(Visibility::Shown);
        wait_for_line(&session, "one");
        assert_eq!(notify_events(&session), []);
    }

    #[test]
    fn sessions_have_distinct_ids() {
        let first = Session::fixture(Grid::default());
        let second = Session::fixture(Grid::default());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn a_shell_is_busy_only_while_a_program_runs_in_the_foreground() {
        let session = spawn_shell();
        wait_for_busy(&session, false);
        type_line(&session, "sleep 5");
        wait_for_busy(&session, true);
    }

    #[test]
    fn a_dropped_session_reports_finished_once_its_process_is_gone() {
        let session = Session::spawn(
            Some("sleep 100"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        let pid = session.pid.lock().unwrap().unwrap();
        let finished = session.finished();
        drop(session);
        assert!(matches!(
            finished.recv_timeout(Duration::from_secs(5)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
        let alive = nix::sys::signal::kill(Pid::from_raw(pid), None);
        assert_eq!(alive, Err(Errno::ESRCH));
    }

    fn pgrep(pattern: &str) -> bool {
        Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .is_ok_and(|output| !output.stdout.is_empty())
    }

    #[test]
    fn a_dropped_session_takes_its_command_with_it() {
        let session = Session::spawn(
            Some("sleep 4243"),
            Path::new("/tmp"),
            &Config::default(),
            Arc::new(|| {}),
        )
        .unwrap();
        wait_until(
            Duration::from_secs(15),
            || "sleep never started".to_owned(),
            || pgrep("sleep 4243").then_some(()),
        );
        let finished = session.finished();
        drop(session);
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_err());
        let survivors = Command::new("pgrep")
            .args(["-f", "sleep 4243"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&survivors.stdout), "");
    }

    #[test]
    fn a_background_job_ends_with_its_dropped_shell() {
        let session = spawn_shell();
        type_line(&session, "sleep 4242 & echo started");
        wait_for_line(&session, "started");
        let finished = session.finished();
        drop(session);
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_err());
        let survivors = Command::new("pgrep")
            .args(["-f", "sleep 4242"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&survivors.stdout), "");
    }

    #[test]
    fn children_get_the_users_utf8_locale_only_when_the_app_has_none() {
        let unset = |_: &str| None;
        let locale = |system: Option<&str>| missing_locale(unset, || system.map(str::to_owned));
        assert_eq!(locale(Some("en_GB")).as_deref(), Some("en_GB.UTF-8"));
        assert_eq!(
            locale(Some("en_GB@rg=zazzzz")).as_deref(),
            Some("en_GB.UTF-8")
        );
        assert_eq!(locale(Some("xx_YY")).as_deref(), Some("en_US.UTF-8"));
        assert_eq!(locale(None).as_deref(), Some("en_US.UTF-8"));
        for set in ["LANG", "LC_ALL", "LC_CTYPE"] {
            let lookup = |name: &str| (name == set).then(|| "de_DE.UTF-8".into());
            let system = || unreachable!("the system locale is read with {set} set");
            assert_eq!(missing_locale(lookup, system), None, "{set} is set");
        }
    }

    #[test]
    fn child_programs_see_the_launch_shell() {
        let session = spawn_shell();
        type_line(&session, "echo $SHELL");
        wait_for_line(&session, &Config::default().shell.to_string_lossy());
    }

    #[test]
    fn a_real_process_reaches_the_grid_and_takes_keys() {
        let session = spawn_shell();
        type_line(&session, "echo agentd-42");
        wait_for_line(&session, "agentd-42");
    }

    #[test]
    fn a_shell_gets_an_answer_to_a_device_attributes_query() {
        let session = spawn_shell();
        type_line(
            &session,
            r#"sh -c "stty raw -echo; printf '\\033[c'; r=\$(dd bs=1 count=12 2>/dev/null); stty sane; echo reply:\${r#?}""#,
        );
        wait_for_line(&session, "reply:[?62;22;52c");
    }

    #[test]
    fn ctrl_c_interrupts_the_foreground_process() {
        let session = spawn_shell();
        type_line(&session, "echo started; sleep 4343 && echo slept");
        wait_for_line(&session, "started");
        wait_for_busy(&session, true);
        let ctrl_c = crate::input::map_text(
            "c",
            rune::Mods {
                ctrl: true,
                ..rune::Mods::default()
            },
            Some(rune::PhysicalKeyCode::KeyC),
            Some('c'),
        );
        let sleeping = || {
            Command::new("pgrep")
                .args(["-f", "sleep 4343"])
                .output()
                .unwrap()
                .status
                .success()
        };
        // `sleep` holds the terminal before it resets its signal handling, so an early ^C is lost.
        // Resend only after the previous one has had time to take effect.
        let deadline = Instant::now() + Duration::from_secs(15);
        while sleeping() {
            assert!(Instant::now() < deadline, "sleep was never interrupted");
            session.key(ctrl_c.clone());
            let settled = Instant::now() + Duration::from_secs(1);
            while sleeping() && Instant::now() < settled {
                thread::sleep(Duration::from_millis(20));
            }
        }
        wait_for_busy(&session, false);
        type_line(&session, "echo after-4");
        wait_for_line(&session, "after-4");
        assert!(
            !session
                .grid()
                .lines()
                .iter()
                .any(|line| line.trim_end() == "slept")
        );
    }

    #[test]
    fn command_backspace_clears_the_line() {
        let session = spawn_shell();
        session.key(crate::input::map_text(
            "echo stale",
            rune::Mods::default(),
            None,
            None,
        ));
        let cmd = rune::Mods {
            cmd: true,
            logo: true,
            ..rune::Mods::default()
        };
        session.key(
            crate::input::map_key(
                &rune::KeyEvent {
                    key: rune::Key::Backspace,
                    mods: cmd,
                },
                Some(rune::PhysicalKeyCode::Backspace),
                None,
            )
            .unwrap(),
        );
        type_line(&session, "echo fresh-7");
        wait_for_line(&session, "fresh-7");
        assert!(
            !session
                .grid()
                .lines()
                .iter()
                .any(|line| line.contains("stale"))
        );
    }
}
