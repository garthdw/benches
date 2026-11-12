use std::cell::RefCell;
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, bounded, never, select, unbounded};
use libghostty_vt::terminal::Mode;
use libghostty_vt::{Terminal, focus, key};
use nix::errno::Errno;
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rune::Waker;

use crate::config::{Config, Theme};
use crate::grid::{
    DEFAULT_BG, DEFAULT_FG, Grid, GridBuilder, Presentation, ScrollbarFade, Surface,
};
use crate::input::KeyInput;
use crate::terminal_input::{
    PointerInput, PointerPhase, TerminalInput, WheelInput, open_link, paste_text, selection_text,
};

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

pub(crate) const FRAME: Duration = Duration::from_millis(8);
/// How long the size must hold before the program sees it, as each SIGWINCH makes inline TUIs reprint.
const RESIZE_SETTLE: Duration = Duration::from_millis(100);
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const BLINK: Duration = Duration::from_millis(530);

pub trait LogErr<T> {
    fn log_err(self, context: &str) -> Option<T>;
}

impl<T, E: Display> LogErr<T> for std::result::Result<T, E> {
    fn log_err(self, context: &str) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("agentd: {context}: {error:#}");
                None
            }
        }
    }
}

/// Sent from the actor for the UI to pick up in `poll`.
pub enum Event {
    Copied(String),
    PasteRejected(String),
    /// The program's title (OSC 0 or 2); empty when it clears it.
    Title(String),
    Exited(ExitStatus),
    /// A bell or desktop notification while not shown with keys, once until it next is.
    /// A bell has an empty title and body; OSC 9 has no title.
    Notify {
        title: String,
        body: String,
    },
    /// ⌘ moved onto or off a link; read `Session::link_hovered`.
    LinkHover,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    #[default]
    None,
    /// A bell or desktop notification (OSC 9 or 777).
    Alert,
}

pub(crate) fn new_terminal<'cb>(cols: u16, rows: u16) -> Result<Terminal<'static, 'cb>> {
    let mut terminal = Terminal::new(cols, rows)?;
    terminal
        .set_scrollback_max_lines(Some(10_000))?
        .set_terminfo_name("xterm-256color")?
        .set_default_fg_color(Some(DEFAULT_FG))?
        .set_default_bg_color(Some(DEFAULT_BG))?;
    terminal.set_mode(Mode::GRAPHEME_CLUSTER, true)?;
    Ok(terminal)
}

pub(crate) fn apply_theme(terminal: &mut Terminal<'static, '_>, theme: &Theme) -> Result<()> {
    let mut palette = terminal.default_color_palette()?;
    for &(index, color) in &theme.palette {
        palette.0[usize::from(index)] = color;
    }
    terminal
        .set_default_fg_color(Some(theme.foreground))?
        .set_default_bg_color(Some(theme.background))?
        .set_default_cursor_color(theme.cursor)?
        .set_default_color_palette(Some(palette))?;
    Ok(())
}

enum Msg {
    Key(KeyInput),
    Focus(bool),
    Pointer(PointerInput),
    Wheel(WheelInput),
    Scrollbar { phase: PointerPhase, y: f32 },
    ScrollbarHover(bool),
    CommandHeld(bool),
    Copy,
    Paste { text: String, allow_unsafe: bool },
    Resize(Surface),
    Visibility(Visibility),
    Stop,
}

/// How much of a session the UI shows, which decides what it publishes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Visibility {
    /// The terminal itself, published every frame.
    Shown,
    /// Only its tab label, so titles still come back.
    Listed,
    /// Nothing; the latest title waits until it is listed or shown.
    Hidden,
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
            process.arg("-c").arg(format!("exec {command}"));
        }
        process
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
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
        let child = process.spawn()?;
        let pid = Arc::new(Mutex::new(Some(child.id() as i32)));

        let master = pty.master;
        let (output_tx, output_rx) = bounded(64);
        let mut reader = File::from(master.try_clone()?);
        thread::spawn(move || {
            let mut buffer = [0; 64 * 1024];
            while let Ok(length @ 1..) = reader.read(&mut buffer) {
                if output_tx.send(buffer[..length].to_vec()).is_err() {
                    break;
                }
            }
        });
        let (input_tx, input_rx) = unbounded::<Vec<u8>>();
        let mut writer = File::from(master.try_clone()?);
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
            events: events_tx,
            waker,
            theme: config.theme.clone(),
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

    /// The process's current directory, which follows the shell's `cd`.
    pub fn cwd(&self) -> Option<PathBuf> {
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

    pub fn copy(&self) {
        self.send(Msg::Copy);
    }

    pub fn paste(&self, text: String, allow_unsafe: bool) {
        self.send(Msg::Paste { text, allow_unsafe });
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

fn focus_report(terminal: &Terminal<'static, '_>, focused: bool) -> Result<Option<Vec<u8>>> {
    if !terminal.mode(Mode::FOCUS_EVENT)? {
        return Ok(None);
    }
    let event = if focused {
        focus::Event::Gained
    } else {
        focus::Event::Lost
    };
    let mut buffer = [0; 8];
    let length = event.encode(&mut buffer)?;
    Ok(Some(buffer[..length].to_vec()))
}

fn store(slot: &Mutex<Arc<Grid>>, grid: Grid) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(grid);
}

/// Holds a new grid size until it stops changing, so a sidebar slide or window drag sends one SIGWINCH.
#[derive(Default)]
struct ResizeDebounce {
    pending: Option<Surface>,
    applied: Option<Surface>,
    due: Option<Instant>,
}

impl ResizeDebounce {
    /// The first size, and changes that keep the grid size, apply at once.
    fn request(&mut self, surface: Surface, now: Instant) {
        let same_grid = self
            .applied
            .is_none_or(|applied| (applied.cols, applied.rows) == (surface.cols, surface.rows));
        self.pending = Some(surface);
        self.due = Some(if same_grid { now } else { now + RESIZE_SETTLE });
    }

    fn take_due(&mut self, now: Instant) -> Option<Surface> {
        if self.due.is_none_or(|due| now < due) {
            return None;
        }
        self.due = None;
        self.applied = self.pending.take();
        self.applied
    }
}

struct Actor {
    cols: u16,
    rows: u16,
    output: Receiver<Vec<u8>>,
    commands: Receiver<Msg>,
    input: Sender<Vec<u8>>,
    child: Child,
    pid: Arc<Mutex<Option<i32>>>,
    master: OwnedFd,
    slot: Arc<Mutex<Arc<Grid>>>,
    alerted: Arc<AtomicBool>,
    link_hovered: Arc<AtomicBool>,
    events: Sender<Event>,
    waker: Waker,
    theme: Theme,
}

impl Actor {
    fn update_link_hover(
        &self,
        input: &mut TerminalInput,
        terminal: &Terminal<'static, '_>,
        command: bool,
    ) {
        if let Some(Some(hovered)) = input.link_hover(terminal, command).log_err("find link") {
            self.link_hovered.store(hovered, Ordering::Release);
            self.emit(Event::LinkHover);
        }
    }

    fn emit(&self, event: Event) {
        self.events.send(event).log_err("queue event");
        (self.waker)();
    }

    fn apply_resize(&self, terminal: &mut Terminal<'static, '_>, surface: Surface) {
        let Surface {
            cols,
            rows,
            metrics,
        } = surface;
        let (cell_width, cell_height) = (metrics.cell_width, metrics.cell_height);
        terminal
            .resize(cols, rows, u32::from(cell_width), u32::from(cell_height))
            .log_err("resize terminal");
        let size = Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: cols.saturating_mul(cell_width),
            ws_ypixel: rows.saturating_mul(cell_height),
        };
        // SAFETY: `master` is an open PTY and `size` outlives the call.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } < 0 {
            eprintln!("agentd: set PTY size: {}", std::io::Error::last_os_error());
        }
    }

    fn run(&mut self) -> Result<()> {
        let mut terminal = new_terminal(self.cols, self.rows)?;
        apply_theme(&mut terminal, &self.theme)?;
        let mut builder = GridBuilder::new()?;
        builder.selection_background = self.theme.selection_background;
        builder.selection_foreground = self.theme.selection_foreground;
        let publisher = Rc::new(RefCell::new(Publisher {
            builder,
            slot: self.slot.clone(),
            waker: self.waker.clone(),
            generation: 0,
            next_frame: Instant::now(),
            hold_started: None,
            presentation: Presentation::default(),
            next_blink: None,
            visibility: Visibility::Shown,
        }));

        let pty_input = self.input.clone();
        terminal.on_pty_write(move |_, data| {
            pty_input.send(data.to_vec()).log_err("queue PTY reply");
        })?;
        let hold_publisher = publisher.clone();
        terminal.on_render_hold(move |terminal, held| {
            let mut publisher = hold_publisher.borrow_mut();
            publisher.hold_started = held.then(Instant::now);
            // Keep the finished frame from before the hold, or a program that redraws constantly looks frozen.
            if held && publisher.visible() && Instant::now() >= publisher.next_frame {
                publisher.publish(terminal).log_err("publish held frame");
            }
        })?;

        let title_changed = Rc::new(std::cell::Cell::new(false));
        terminal.on_title_changed({
            let title_changed = title_changed.clone();
            move |_| title_changed.set(true)
        })?;
        let alerted = Rc::new(std::cell::Cell::new(false));
        let notice = Rc::new(RefCell::new(None));
        terminal.on_bell({
            let alerted = alerted.clone();
            move |_| alerted.set(true)
        })?;
        terminal.on_desktop_notification({
            let alerted = alerted.clone();
            let notice = notice.clone();
            move |_, notification| {
                alerted.set(true);
                *notice.borrow_mut() = Some((
                    String::from_utf8_lossy(notification.title()).into_owned(),
                    String::from_utf8_lossy(notification.body()).into_owned(),
                ));
            }
        })?;
        let mut notified = false;

        let mut input = TerminalInput::new()?;
        let mut fade = ScrollbarFade::default();
        fade.follow(&terminal);
        let mut scrollbar_hovered = false;
        let mut resize = ResizeDebounce::default();
        let mut dirty = true;
        let mut titles = Titles {
            sent: String::new(),
            next: Instant::now(),
        };
        loop {
            let now = Instant::now();
            let publisher_deadline = publisher.borrow().deadline(dirty, now);
            let visibility = publisher.borrow().visibility;
            let wait = [
                input.next_autoscroll,
                resize.due,
                fade.next_change(now),
                titles.due(visibility).filter(|_| title_changed.get()),
            ]
            .into_iter()
            .flatten()
            .fold(publisher_deadline, Instant::min)
            .saturating_duration_since(now);
            select! {
                recv(self.output) -> bytes => match bytes {
                    Ok(bytes) => {
                        terminal.vt_write(&bytes);
                        fade.follow(&terminal);
                        dirty = true;
                        let alerted = alerted.replace(false);
                        let notice = notice.borrow_mut().take();
                        if alerted
                            && !publisher.borrow().visible()
                            && !self.alerted.swap(true, Ordering::AcqRel)
                        {
                            (self.waker)();
                        }
                        let seen = {
                            let publisher = publisher.borrow();
                            publisher.visible() && publisher.presentation.focused
                        };
                        if alerted && !seen && !notified {
                            notified = true;
                            let (title, body) = notice.unwrap_or_default();
                            self.emit(Event::Notify { title, body });
                        }
                    }
                    Err(_) => {
                        self.output = never();
                        forget(&self.pid);
                        if let Some(status) = self.child.wait().log_err("wait for process") {
                            self.emit(Event::Exited(status));
                        }
                        terminal.vt_write(b"\r\n[process exited]\r\n");
                        dirty = true;
                    }
                },
                recv(self.commands) -> command => match command {
                    Ok(Msg::Key(key)) => {
                        if let Some(encoded) = input.key(&mut terminal, key).log_err("encode key")
                            && !encoded.is_empty()
                        {
                            self.input.send(encoded).log_err("queue key input");
                            publisher.borrow_mut().restart_blink(Instant::now());
                            dirty = true;
                        }
                    }
                    Ok(Msg::Pointer(pointer)) => {
                        if let Some(Some(url)) =
                            input.link_under(&terminal, pointer).log_err("find link")
                        {
                            open_link(&url);
                        } else if let Some(bytes) = input.pointer(&terminal, pointer).log_err("handle pointer")
                            && !bytes.is_empty()
                        {
                            self.input.send(bytes).log_err("queue mouse report");
                        }
                        let command = pointer.mods.contains(key::Mods::SUPER);
                        self.update_link_hover(&mut input, &terminal, command);
                    }
                    Ok(Msg::Wheel(wheel)) => {
                        match input.wheel(&mut terminal, wheel).log_err("handle wheel") {
                            Some(Some(bytes)) if !bytes.is_empty() => {
                                self.input.send(bytes).log_err("queue wheel input");
                            }
                            // Shown even when the viewport is already at an end, as macOS does.
                            Some(None) => fade.show(Instant::now()),
                            _ => {}
                        }
                        dirty = true;
                    }
                    Ok(Msg::Scrollbar { phase, y }) => {
                        input.drag_scrollbar(&mut terminal, phase, y).log_err("drag scrollbar");
                        dirty = true;
                    }
                    Ok(Msg::ScrollbarHover(hovered)) => scrollbar_hovered = hovered,
                    Ok(Msg::CommandHeld(held)) => self.update_link_hover(&mut input, &terminal, held),
                    Ok(Msg::Copy) => {
                        if let Some(Some(text)) = selection_text(&terminal).log_err("copy selection") {
                            self.emit(Event::Copied(text));
                        }
                    }
                    Ok(Msg::Focus(focused)) => {
                        let mut publisher = publisher.borrow_mut();
                        if focused && publisher.visible() {
                            notified = false;
                        }
                        publisher.presentation.focused = focused;
                        publisher.restart_blink(Instant::now());
                        dirty = true;
                        if let Some(Some(report)) = focus_report(&terminal, focused).log_err("encode focus report") {
                            self.input.send(report).log_err("queue focus report");
                        }
                    }
                    Ok(Msg::Paste { text, allow_unsafe }) => {
                        match paste_text(&mut terminal, &text, allow_unsafe).log_err("paste") {
                            Some(true) => {
                                publisher.borrow_mut().restart_blink(Instant::now());
                                dirty = true;
                            }
                            Some(false) => {
                                self.emit(Event::PasteRejected(text));
                            }
                            None => {}
                        }
                    }
                    Ok(Msg::Resize(surface)) => {
                        resize.request(surface, Instant::now());
                    }
                    Ok(Msg::Stop) => terminate(&mut self.child, &self.pid),
                    Ok(Msg::Visibility(visibility)) => {
                        if visibility == Visibility::Shown {
                            self.alerted.store(false, Ordering::Release);
                        }
                        // The UI's hover and pointer move to whichever session takes this one's place.
                        if visibility != Visibility::Shown {
                            scrollbar_hovered = false;
                            if let Some(release) = input.cancel_pointer(&terminal).log_err("cancel pointer")
                                && !release.is_empty()
                            {
                                self.input.send(release).log_err("queue mouse release");
                            }
                        }
                        publisher.borrow_mut().visibility = visibility;
                        dirty = true;
                    }
                    Err(_) => return Ok(()),
                },
                default(wait) => {}
            }
            // libghostty only exposes a new title once the write that set it returns.
            let visibility = publisher.borrow().visibility;
            if title_changed.get()
                && titles
                    .due(visibility)
                    .is_some_and(|due| Instant::now() >= due)
            {
                title_changed.set(false);
                if let Some(title) = terminal.title().log_err("read title")
                    && let Some(title) = titles.send(title, Instant::now())
                {
                    self.emit(Event::Title(title));
                }
            }
            // Before borrowing the publisher: a resize ends any render hold through its callback.
            if let Some(surface) = resize.take_due(Instant::now()) {
                self.apply_resize(&mut terminal, surface);
                fade.follow(&terminal);
                input.surface = Some(surface);
                publisher.borrow_mut().presentation.metrics = surface.metrics;
                dirty = true;
            }
            let mut publisher = publisher.borrow_mut();
            dirty |= publisher.tick_blink(Instant::now());
            let held = scrollbar_hovered || input.scrollbar_grab.is_some();
            let opacity = fade.opacity(&terminal, held, Instant::now());
            let presentation = &mut publisher.presentation;
            dirty |=
                (presentation.scrollbar_opacity, presentation.scrollbar_wide) != (opacity, held);
            (presentation.scrollbar_opacity, presentation.scrollbar_wide) = (opacity, held);
            input
                .tick_autoscroll(&terminal, Instant::now())
                .log_err("autoscroll selection");
            if std::mem::take(&mut input.selection_changed) {
                publisher.builder.selection_changed();
                dirty = true;
            }
            if publisher
                .hold_started
                .is_some_and(|started| started.elapsed() >= HOLD_TIMEOUT)
            {
                // Doesn't invoke the hold callback, so the publisher is released by hand.
                terminal
                    .set_mode(Mode::SYNC_OUTPUT, false)
                    .log_err("end expired render hold");
                publisher.hold_started = None;
            }
            // A selection must track the pointer even while a program is holding its frame.
            let held = publisher.hold_started.is_some() && !input.selecting;
            if dirty && !held && publisher.visible() && Instant::now() >= publisher.next_frame {
                publisher.publish(&terminal)?;
                dirty = false;
            }
        }
    }
}

/// Sends program titles at most once a frame, and none while hidden, as animated titles can change constantly.
struct Titles {
    sent: String,
    next: Instant,
}

impl Titles {
    /// When a changed title may next be sent, or `None` while hidden.
    fn due(&self, visibility: Visibility) -> Option<Instant> {
        (visibility != Visibility::Hidden).then_some(self.next)
    }

    /// The title to send, unless it is the one sent last, as shells often repeat it on every prompt.
    fn send(&mut self, title: &[u8], now: Instant) -> Option<String> {
        if title == self.sent.as_bytes() {
            return None;
        }
        self.sent = String::from_utf8_lossy(title).into_owned();
        self.next = now + FRAME;
        Some(self.sent.clone())
    }
}

struct Publisher {
    builder: GridBuilder,
    slot: Arc<Mutex<Arc<Grid>>>,
    waker: Waker,
    generation: u64,
    next_frame: Instant,
    hold_started: Option<Instant>,
    presentation: Presentation,
    next_blink: Option<Instant>,
    visibility: Visibility,
}

impl Publisher {
    fn visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    fn deadline(&self, dirty: bool, now: Instant) -> Instant {
        let deadline = match self.hold_started {
            Some(started) => started + HOLD_TIMEOUT,
            None if dirty && self.visible() => self.next_frame,
            None => now + Duration::from_secs(3600),
        };
        self.next_blink
            .filter(|_| self.visible())
            .map_or(deadline, |blink| deadline.min(blink))
    }

    fn tick_blink(&mut self, now: Instant) -> bool {
        if !self.visible() || !self.next_blink.is_some_and(|blink| now >= blink) {
            return false;
        }
        self.presentation.cursor_on = !self.presentation.cursor_on;
        self.next_blink = Some(now + BLINK);
        true
    }

    /// Keeps the cursor solid while the user is typing.
    fn restart_blink(&mut self, now: Instant) {
        if self.next_blink.is_some() {
            self.presentation.cursor_on = true;
            self.next_blink = Some(now + BLINK);
        }
    }

    fn publish(&mut self, terminal: &Terminal<'static, '_>) -> Result<()> {
        let now = Instant::now();
        self.next_frame = now + FRAME;
        if let Some(grid) = self
            .builder
            .build(terminal, self.generation + 1, self.presentation)?
        {
            if grid.cursor_blinking && self.presentation.focused {
                self.next_blink.get_or_insert(now + BLINK);
            } else {
                self.next_blink = None;
                self.presentation.cursor_on = true;
            }
            self.generation = grid.generation;
            store(&self.slot, grid);
            (self.waker)();
        }
        Ok(())
    }
}

fn signal_group(group: Pid, signal: Signal) {
    if let Err(error) = killpg(group, signal)
        && error != Errno::ESRCH
    {
        eprintln!("agentd: signal process group: {error}");
    }
}

/// Call before reaping `Child`, which frees its pid for reuse.
fn forget(pid: &Mutex<Option<i32>>) {
    *pid.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

fn terminate(child: &mut Child, pid: &Mutex<Option<i32>>) {
    forget(pid);
    // Also true once the actor has reaped the process, whose pid may have been reused.
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let group = Pid::from_raw(child.id() as i32);
    signal_group(group, Signal::SIGHUP);
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    signal_group(group, Signal::SIGKILL);
    child.wait().log_err("wait for process");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Metrics, Scrollbar};
    use crate::test_support::{TEST_SURFACE, pointer_at, wait_until, wheel_by};
    use libghostty_vt::render::CursorVisualStyle;

    fn spawn_shell() -> Session {
        Session::spawn(None, Path::new("/tmp"), &Config::default(), Arc::new(|| {})).unwrap()
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

    fn idle_publisher(now: Instant) -> Publisher {
        Publisher {
            builder: GridBuilder::new().unwrap(),
            slot: Arc::default(),
            waker: Arc::new(|| {}),
            generation: 0,
            next_frame: now,
            hold_started: None,
            presentation: Presentation::default(),
            next_blink: None,
            visibility: Visibility::Shown,
        }
    }

    #[test]
    fn a_hidden_dirty_publisher_sleeps() {
        let now = Instant::now();
        let mut publisher = idle_publisher(now);
        publisher.next_blink = Some(now);
        publisher.visibility = Visibility::Hidden;
        assert!(publisher.deadline(true, now) > now + Duration::from_secs(60));
        assert!(!publisher.tick_blink(now));
    }

    #[test]
    fn a_paste_reaches_a_real_shell() {
        let session = spawn_shell();
        session.paste("echo pasted-ok".to_owned(), false);
        type_line(&session, "");
        wait_for_line(&session, "pasted-ok");
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
    fn the_first_resize_applies_at_once() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now);
        assert_eq!(debounce.take_due(now), Some(TEST_SURFACE));
        assert_eq!(debounce.due, None);
    }

    #[test]
    fn a_burst_of_resizes_applies_once_it_settles() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now);
        debounce.take_due(now);
        let mut at = now;
        for cols in 21..40 {
            at += FRAME;
            debounce.request(
                Surface {
                    cols,
                    ..TEST_SURFACE
                },
                at,
            );
            assert_eq!(debounce.take_due(at), None);
        }
        assert_eq!(debounce.due, Some(at + RESIZE_SETTLE));
        assert_eq!(debounce.take_due(at + RESIZE_SETTLE - FRAME), None);
        let settled = debounce.take_due(at + RESIZE_SETTLE);
        assert_eq!(
            settled,
            Some(Surface {
                cols: 39,
                ..TEST_SURFACE
            })
        );
        assert_eq!(debounce.due, None);
    }

    #[test]
    fn a_change_of_scale_alone_applies_at_once() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now);
        debounce.take_due(now);
        let rescaled = Surface {
            metrics: Metrics {
                scale: 2.0,
                ..TEST_SURFACE.metrics
            },
            ..TEST_SURFACE
        };
        debounce.request(rescaled, now);
        assert_eq!(debounce.take_due(now), Some(rescaled));
    }

    #[test]
    fn a_held_frame_sleeps_until_the_hold_deadline() {
        let now = Instant::now();
        let publisher = Publisher {
            hold_started: Some(now),
            ..idle_publisher(now)
        };
        assert_eq!(publisher.deadline(true, now), now + HOLD_TIMEOUT);
    }

    #[test]
    fn an_idle_actor_wakes_for_the_next_blink() {
        let now = Instant::now();
        let publisher = Publisher {
            next_blink: Some(now + BLINK),
            ..idle_publisher(now)
        };
        assert_eq!(publisher.deadline(false, now), now + BLINK);
    }

    #[test]
    fn a_blink_tick_toggles_only_once_due() {
        let now = Instant::now();
        let mut publisher = Publisher {
            next_blink: Some(now + BLINK),
            ..idle_publisher(now)
        };
        assert!(!publisher.tick_blink(now));
        assert!(publisher.tick_blink(now + BLINK));
        assert!(!publisher.presentation.cursor_on);
    }

    #[test]
    fn typing_turns_a_blinking_cursor_back_on() {
        let now = Instant::now();
        let mut publisher = Publisher {
            presentation: Presentation {
                cursor_on: false,
                ..Presentation::default()
            },
            next_blink: Some(now),
            ..idle_publisher(now)
        };
        publisher.restart_blink(now);
        assert!(publisher.presentation.cursor_on);
        assert_eq!(publisher.next_blink, Some(now + BLINK));
    }

    #[test]
    fn the_blink_off_phase_hides_only_a_blinking_cursor() {
        let off = Presentation {
            cursor_on: false,
            ..Presentation::default()
        };
        let mut builder = GridBuilder::new().unwrap();
        let mut terminal = new_terminal(10, 2).unwrap();
        terminal.vt_write(b"\x1b[2 q");
        assert!(
            builder
                .build(&terminal, 1, off)
                .unwrap()
                .unwrap()
                .cursor
                .is_some()
        );
        terminal.vt_write(b"\x1b[1 q");
        assert!(
            builder
                .build(&terminal, 2, off)
                .unwrap()
                .unwrap()
                .cursor
                .is_none()
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
    fn focus_is_reported_only_when_the_program_asked() {
        let mut terminal = new_terminal(10, 2).unwrap();
        assert_eq!(focus_report(&terminal, true).unwrap(), None);
        terminal.vt_write(b"\x1b[?1004h");
        assert_eq!(focus_report(&terminal, true).unwrap().unwrap(), b"\x1b[I");
        assert_eq!(focus_report(&terminal, false).unwrap().unwrap(), b"\x1b[O");
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
    fn titles_wait_a_frame_between_sends_and_while_hidden() {
        let now = Instant::now();
        let mut titles = Titles {
            sent: String::new(),
            next: now,
        };
        assert_eq!(titles.send(b"one", now).as_deref(), Some("one"));
        assert_eq!(titles.due(Visibility::Listed), Some(now + FRAME));
        assert_eq!(titles.send(b"one", now + FRAME), None);
        assert_eq!(titles.due(Visibility::Hidden), None);
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

    #[test]
    fn sessions_have_distinct_ids() {
        let first = Session::fixture(Grid::default());
        let second = Session::fixture(Grid::default());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn a_shell_is_busy_only_while_a_program_runs_in_the_foreground() {
        let session = spawn_shell();
        let idle = || (!session.busy()).then_some(());
        wait_until(Duration::from_secs(15), || "never idle".to_owned(), idle);
        type_line(&session, "sleep 5");
        let busy = || session.busy().then_some(());
        wait_until(Duration::from_secs(15), || "never busy".to_owned(), busy);
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

    #[test]
    fn a_background_job_ends_with_its_dropped_shell() {
        let session = spawn_shell();
        type_line(&session, "sleep 4242 &; echo started");
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
        wait_for_line(&session, "/opt/homebrew/bin/fish");
    }

    #[test]
    fn a_real_process_reaches_the_grid_and_takes_keys() {
        let session = spawn_shell();
        type_line(&session, "echo agentd-42");
        wait_for_line(&session, "agentd-42");
    }

    #[test]
    fn ctrl_c_interrupts_the_foreground_process() {
        let session = spawn_shell();
        type_line(&session, "sleep 30; echo slept; echo marker-2");
        thread::sleep(Duration::from_millis(1500));
        let ctrl = rune::Mods {
            ctrl: true,
            ..rune::Mods::default()
        };
        session.key(crate::input::map_text(
            "c",
            ctrl,
            Some(rune::PhysicalKeyCode::KeyC),
            Some('c'),
        ));
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
}
