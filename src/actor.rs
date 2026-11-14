use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Child;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossbeam_channel::{Receiver, Sender, never, select};
use libghostty_vt::effect::SemanticPromptKind;
use libghostty_vt::effect::{ClipboardLocation, ClipboardReadError, ClipboardWriteError};
use libghostty_vt::effect::{ProgramStatusKind, ProgramStatusState, ProgressState};
use libghostty_vt::mode::Mode;
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, focus, key};
use nix::errno::Errno;
use nix::pty::Winsize;
use nix::sys::signal::{Signal, killpg};
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::unistd::Pid;
use rune::Waker;

use crate::LogErr;
use crate::config::{ClipboardAccess, TerminalSettings};
use crate::grid::{
    FRAME, Grid, GridBuilder, Presentation, ScrollbarFade, Surface, answer_queries, apply_theme,
    new_terminal,
};
use crate::link::reported_directory;
use crate::protocol::{Event, Msg, Progress, ProgressKind, Visibility};
use crate::search::Searcher;
use crate::terminal_input::{TerminalInput, open_link, paste_text, selection_text};

/// How long the size must hold before the program sees it, as each SIGWINCH makes inline TUIs reprint.
const RESIZE_SETTLE: Duration = Duration::from_millis(100);
/// Later than `RESIZE_SETTLE`, so background tabs' programs reprint after the shown one has.
const BACKGROUND_RESIZE_SETTLE: Duration = Duration::from_millis(400);
/// Ghostty's: a program that dies mid-task never removes its progress. Short in tests.
const PROGRESS_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 1000 } else { 15_000 });
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const BLINK: Duration = Duration::from_millis(530);
/// Ghostty's interval for watching the terminal modes that mark a password prompt.
const PASSWORD_POLL: Duration = Duration::from_millis(200);

/// Programs reading a password turn echo off while keeping line editing, as `sudo` and `ssh` do.
fn password_prompt(master: &OwnedFd) -> bool {
    tcgetattr(master).is_ok_and(|termios| {
        termios.local_flags.contains(LocalFlags::ICANON)
            && !termios.local_flags.contains(LocalFlags::ECHO)
    })
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

struct Finished {
    failed: bool,
    /// Ran past the configured threshold.
    long: bool,
}

/// Times commands from OSC 133 C to D. A shell and its integration may both emit the marks, so
/// only the first C and the first D after it count.
#[derive(Default)]
struct CommandTimer {
    started: Option<Instant>,
}

impl CommandTimer {
    fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    fn finish(
        &mut self,
        now: Instant,
        exit_code: Option<i32>,
        threshold: Option<Duration>,
    ) -> Option<Finished> {
        let ran = now.saturating_duration_since(self.started.take()?);
        Some(Finished {
            failed: exit_code.is_some_and(|code| code != 0),
            long: threshold.is_some_and(|threshold| ran >= threshold),
        })
    }
}

fn watch_commands(
    terminal: &mut Terminal<'static, '_>,
    threshold: Rc<Cell<Option<Duration>>>,
) -> Result<Rc<RefCell<Vec<Finished>>>> {
    let finished = Rc::new(RefCell::new(Vec::new()));
    let mut timer = CommandTimer::default();
    terminal.on_semantic_prompt({
        let finished = finished.clone();
        move |_, prompt| match prompt.kind() {
            Ok(SemanticPromptKind::OutputStart) => timer.start(Instant::now()),
            Ok(SemanticPromptKind::CommandEnd) => {
                if let Some(command) =
                    timer.finish(Instant::now(), prompt.exit_code(), threshold.get())
                {
                    finished.borrow_mut().push(command);
                }
            }
            _ => {}
        }
    })?;
    Ok(finished)
}

pub(crate) fn store(slot: &Mutex<Arc<Grid>>, grid: Grid) {
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
    fn request(&mut self, surface: Surface, now: Instant, shown: bool) {
        let same_grid = self
            .applied
            .is_none_or(|applied| (applied.cols, applied.rows) == (surface.cols, surface.rows));
        self.pending = Some(surface);
        let settle = if shown {
            RESIZE_SETTLE
        } else {
            BACKGROUND_RESIZE_SETTLE
        };
        self.due = Some(if same_grid { now } else { now + settle });
    }

    /// Applies a pending size at once, for a tab about to be shown.
    fn hurry(&mut self, now: Instant) {
        self.due = self.due.map(|due| due.min(now));
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

pub(crate) struct Actor {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) output: Receiver<Vec<u8>>,
    pub(crate) commands: Receiver<Msg>,
    pub(crate) input: Sender<Vec<u8>>,
    pub(crate) child: Child,
    pub(crate) pid: Arc<Mutex<Option<i32>>>,
    pub(crate) master: OwnedFd,
    pub(crate) slot: Arc<Mutex<Arc<Grid>>>,
    pub(crate) alerted: Arc<AtomicBool>,
    pub(crate) hover: Arc<AtomicU8>,
    pub(crate) reported_cwd: Arc<Mutex<Option<PathBuf>>>,
    pub(crate) events: Sender<Event>,
    pub(crate) waker: Waker,
    pub(crate) settings: TerminalSettings,
}

impl Actor {
    fn update_hover(
        &self,
        input: &mut TerminalInput,
        terminal: &Terminal<'static, '_>,
        mods: key::Mods,
        shown: bool,
    ) {
        if !shown {
            return;
        }
        if let Some(Some(hover)) = input.hover(terminal, mods).log_err("find hover") {
            self.hover.store(hover as u8, Ordering::Release);
            self.emit(Event::Hover);
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

    /// Registering a callback again replaces it, so this also applies a reloaded config.
    fn apply_settings(
        &self,
        terminal: &mut Terminal<'static, '_>,
        reply_muted: &Rc<Cell<bool>>,
    ) -> Result<()> {
        let settings = &self.settings;
        apply_theme(terminal, &settings.theme)?;
        answer_queries(terminal, &settings.theme, settings.clipboard_write)?;
        let events = self.events.clone();
        let waker = self.waker.clone();
        install_clipboard(
            terminal,
            settings.clipboard_read,
            settings.clipboard_write,
            reply_muted.clone(),
            Rc::new(move |event| {
                events.send(event).log_err("queue event");
                waker();
            }),
        )
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        let mut terminal = new_terminal(self.cols, self.rows)?;
        let reply_muted = Rc::new(Cell::new(false));
        self.apply_settings(&mut terminal, &reply_muted)?;
        let mut builder = GridBuilder::new()?;
        builder.set_selection_colors(&self.settings.theme);
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
            failing: false,
        }));

        let pty_input = self.input.clone();
        terminal.on_pty_write({
            let reply_muted = reply_muted.clone();
            move |_, data| queue_reply(&pty_input, &reply_muted, data)
        })?;
        let hold_publisher = publisher.clone();
        terminal.on_render_hold(move |terminal, held| {
            let mut publisher = hold_publisher.borrow_mut();
            publisher.hold_started = held.then(Instant::now);
            // Keep the finished frame from before the hold, or a program that redraws constantly looks frozen.
            if held && publisher.visible() && Instant::now() >= publisher.next_frame {
                publisher.publish_logged(terminal);
            }
        })?;

        let title_changed = Rc::new(std::cell::Cell::new(false));
        terminal.on_title_changed({
            let title_changed = title_changed.clone();
            move |_| title_changed.set(true)
        })?;
        terminal.on_pwd_changed({
            let reported_cwd = self.reported_cwd.clone();
            let hostname = nix::unistd::gethostname()
                .log_err("read hostname")
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            move |terminal| {
                let directory = terminal
                    .pwd()
                    .log_err("read reported directory")
                    .and_then(|reported| reported_directory(reported, &hostname));
                *reported_cwd.lock().unwrap_or_else(PoisonError::into_inner) = directory;
            }
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
        terminal.on_program_status({
            let alerted = alerted.clone();
            let notice = notice.clone();
            move |_, report| {
                if !matches!(report.state(), Ok(ProgramStatusState::Blocked)) {
                    return;
                }
                alerted.set(true);
                let body = match report.kind() {
                    Ok(ProgramStatusKind::Permission) => "Needs permission",
                    Ok(ProgramStatusKind::Question) => "Has a question",
                    Ok(ProgramStatusKind::Auth) => "Needs authentication",
                    _ => "Needs input",
                };
                // A program's own OSC 9 or 777 text for the same prompt is more specific.
                notice
                    .borrow_mut()
                    .get_or_insert_with(|| (String::new(), body.to_owned()));
            }
        })?;
        let reported = Rc::new(std::cell::Cell::new(None));
        terminal.on_progress_report({
            let reported = reported.clone();
            move |_, report| {
                let Ok(state) = report.state() else { return };
                let percent = report.progress().unwrap_or(0).min(100);
                let kind = match state {
                    ProgressState::Remove => None,
                    ProgressState::Set => Some(ProgressKind::Normal),
                    ProgressState::Error => Some(ProgressKind::Error),
                    ProgressState::Indeterminate => Some(ProgressKind::Indeterminate),
                    ProgressState::Pause => Some(ProgressKind::Paused),
                    _ => return,
                };
                reported.set(Some(kind.map(|kind| Progress { kind, percent })));
            }
        })?;
        let mut progress_due: Option<Instant> = None;
        let mut notified = false;
        let finish_threshold = Rc::new(Cell::new(self.settings.command_finish_after));
        let finished_commands = watch_commands(&mut terminal, finish_threshold.clone())?;

        let mut input = TerminalInput::new()?;
        let mut searcher = Searcher::default();
        let mut fade = ScrollbarFade::default();
        fade.follow(&terminal);
        let mut scrollbar_hovered = false;
        let mut mods = key::Mods::empty();
        let mut resize = ResizeDebounce::default();
        let mut dirty = true;
        // libghostty keeps no timer for animated images; this clock and the due time drive it.
        let animation_epoch = Instant::now();
        let mut animation_due: Option<Instant> = None;
        let mut tick_animation = true;
        let mut next_password_check = Instant::now();
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
                animation_due.filter(|_| visibility == Visibility::Shown),
                titles.due(visibility).filter(|_| title_changed.get()),
                progress_due,
                (visibility == Visibility::Shown).then_some(next_password_check),
            ]
            .into_iter()
            .flatten()
            .fold(publisher_deadline, Instant::min)
            .saturating_duration_since(now);
            select! {
                recv(self.output) -> bytes => match bytes {
                    Ok(bytes) => {
                        terminal.vt_write(&bytes);
                        if publisher.borrow().visible() {
                            fade.follow(&terminal);
                        }
                        // A program turns mouse tracking on and off under a still pointer.
                        self.update_hover(&mut input, &terminal, mods, publisher.borrow().visible());
                        dirty = true;
                        tick_animation = true;
                        let alerted = alerted.replace(false);
                        let notice = notice.borrow_mut().take();
                        let finished = finished_commands.take();
                        if (alerted || finished.iter().any(|command| command.long))
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
                        if let Some(progress) = reported.take() {
                            progress_due = progress.map(|_| Instant::now() + PROGRESS_TIMEOUT);
                            self.emit(Event::Progress(progress));
                        }
                        for command in finished {
                            self.emit(Event::CommandFinished {
                                failed: command.failed,
                            });
                        }
                    }
                    Err(_) => {
                        self.output = never();
                        if progress_due.take().is_some() {
                            self.emit(Event::Progress(None));
                        }
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
                        mods = pointer.mods;
                        self.update_hover(&mut input, &terminal, mods, publisher.borrow().visible());
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
                        // Scrolling moves another link, or none, under the pointer.
                        self.update_hover(&mut input, &terminal, mods, publisher.borrow().visible());
                        dirty = true;
                    }
                    Ok(Msg::Scrollbar { phase, y }) => {
                        input.drag_scrollbar(&mut terminal, phase, y).log_err("drag scrollbar");
                        dirty = true;
                    }
                    Ok(Msg::ScrollbarHover(hovered)) => scrollbar_hovered = hovered,
                    Ok(Msg::Modifiers(held)) => {
                        mods = held;
                        self.update_hover(&mut input, &terminal, mods, publisher.borrow().visible());
                    }
                    Ok(Msg::Copy) => {
                        if let Some(Some(text)) = selection_text(&terminal).log_err("copy selection") {
                            self.emit(Event::Copied(text));
                        }
                    }
                    Ok(Msg::PromptJump(delta)) => {
                        terminal.scroll_viewport(ScrollViewport::DeltaPrompt(delta));
                        fade.show(Instant::now());
                        dirty = true;
                    }
                    Ok(Msg::SelectLastOutput) => {
                        input.select_last_output(&terminal).log_err("select output");
                        dirty = true;
                    }
                    Ok(Msg::ClipboardReply { location, text }) => {
                        let reply = osc52_reply(location, text.as_deref().unwrap_or_default());
                        self.input.send(reply).log_err("queue clipboard reply");
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
                    Ok(Msg::Search(query)) => {
                        searcher.set(&mut terminal, &query).log_err("search");
                        dirty = true;
                    }
                    Ok(Msg::SearchStep { older }) => {
                        searcher.step(&mut terminal, older).log_err("step search");
                        dirty = true;
                    }
                    Ok(Msg::Resize(surface)) => {
                        resize.request(surface, Instant::now(), publisher.borrow().visible());
                    }
                    Ok(Msg::Stop) => terminate(&mut self.child, &self.pid),
                    Ok(Msg::Configure(settings)) => {
                        self.settings = settings;
                        finish_threshold.set(self.settings.command_finish_after);
                        self.apply_settings(&mut terminal, &reply_muted).log_err("apply config");
                        publisher.borrow_mut().builder.set_selection_colors(&self.settings.theme);
                        dirty = true;
                    }
                    Ok(Msg::Visibility(visibility)) => {
                        if visibility == Visibility::Shown {
                            self.alerted.store(false, Ordering::Release);
                            resize.hurry(Instant::now());
                        }
                        // Output while not shown didn't follow the viewport, so it mustn't read as a scroll.
                        fade.hide(&terminal);
                        // The UI's hover and pointer move to whichever session takes this one's place.
                        if visibility != Visibility::Shown {
                            scrollbar_hovered = false;
                            if let Some(release) = input.cancel_pointer(&terminal).log_err("cancel pointer")
                                && !release.is_empty()
                            {
                                self.input.send(release).log_err("queue mouse release");
                            }
                            mods = key::Mods::empty();
                            self.update_hover(&mut input, &terminal, mods, publisher.borrow().visible());
                        }
                        publisher.borrow_mut().visibility = visibility;
                        dirty = true;
                        tick_animation = true;
                    }
                    Err(_) => return Ok(()),
                },
                default(wait) => {}
            }
            if progress_due.is_some_and(|due| Instant::now() >= due) {
                progress_due = None;
                self.emit(Event::Progress(None));
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
            if publisher.visible() && Instant::now() >= next_password_check {
                next_password_check = Instant::now() + PASSWORD_POLL;
                let password_input = password_prompt(&self.master);
                dirty |= publisher.presentation.password_input != password_input;
                publisher.presentation.password_input = password_input;
            }
            dirty |= publisher.tick_blink(Instant::now());
            // Output may have started an animation, which only a tick reveals.
            if publisher.visible()
                && (tick_animation || animation_due.is_some_and(|due| Instant::now() >= due))
            {
                tick_animation = false;
                let elapsed = animation_epoch.elapsed().as_millis() as u64;
                animation_due = terminal
                    .kitty_graphics_animation_tick(elapsed)
                    .log_err("tick animations")
                    .flatten()
                    .map(|next| animation_epoch + Duration::from_millis(next));
                dirty |= animation_due.is_some();
            }
            if publisher.visible() {
                let held = scrollbar_hovered || input.scrollbar_grab.is_some();
                let opacity = fade.opacity(&terminal, held, Instant::now());
                let presentation = &mut publisher.presentation;
                dirty |= (presentation.scrollbar_opacity, presentation.scrollbar_wide)
                    != (opacity, held);
                (presentation.scrollbar_opacity, presentation.scrollbar_wide) = (opacity, held);
            }
            let presentation = &mut publisher.presentation;
            dirty |= presentation.link != input.link;
            presentation.link = input.link;
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
                if let Some((highlights, found)) =
                    terminal.cols().log_err("read columns").and_then(|cols| {
                        searcher
                            .highlights(&mut terminal, cols)
                            .log_err("find matches")
                    })
                {
                    publisher.builder.set_search(highlights, found);
                }
                // A failed publish stays dirty so the next frame retries, rather than ending the tab.
                dirty = !publisher.publish_logged(&terminal);
            }
        }
    }
}

/// Queues bytes libghostty writes to the program, unless the clipboard read callback muted the
/// reply it automatically sends when it returns unanswered.
fn queue_reply(input: &Sender<Vec<u8>>, reply_muted: &Cell<bool>, data: &[u8]) {
    if !reply_muted.take() {
        input.send(data.to_vec()).log_err("queue PTY reply");
    }
}

/// libghostty answers a clipboard read only from inside its callback, which must not wait for
/// the UI. So a permitted read returns unanswered, `reply_muted` swallows the empty reply that
/// libghostty then sends, and `Msg::ClipboardReply` writes the real one later.
fn install_clipboard(
    terminal: &mut Terminal<'static, '_>,
    read: ClipboardAccess,
    write: ClipboardAccess,
    reply_muted: Rc<Cell<bool>>,
    emit: Rc<dyn Fn(Event)>,
) -> Result<()> {
    terminal.on_clipboard_write({
        let emit = emit.clone();
        move |_, request| {
            let mut contents = request.contents().peekable();
            let text = if contents.peek().is_none() {
                Some(String::new())
            } else {
                contents
                    .find(|content| content.mime == b"text/plain")
                    .map(|content| String::from_utf8_lossy(content.data).into_owned())
            };
            match (write, text) {
                (ClipboardAccess::Deny, _) => {
                    request.reply(Err(ClipboardWriteError::Denied), false)
                }
                (_, None) => request.reply(Err(ClipboardWriteError::Unsupported), false),
                (access, Some(text)) => {
                    emit(Event::ClipboardWrite {
                        text,
                        ask: access == ClipboardAccess::Ask,
                    });
                    request.reply(Ok(()), false);
                }
            }
        }
    })?;
    terminal.on_clipboard_read(move |_, request| {
        let text_only = request.mimes().eq([&b"text/plain"[..]]) && !request.list();
        match read {
            _ if !text_only => request.reply(Err(ClipboardReadError::Unsupported), &[], false),
            ClipboardAccess::Deny => request.reply(Err(ClipboardReadError::Denied), &[], false),
            access => {
                reply_muted.set(true);
                emit(Event::ClipboardRead {
                    location: request.location(),
                    ask: access == ClipboardAccess::Ask,
                });
            }
        }
    })?;
    Ok(())
}

fn osc52_reply(location: ClipboardLocation, text: &str) -> Vec<u8> {
    let selector = match location {
        ClipboardLocation::Standard => 'c',
        ClipboardLocation::Selection => 's',
        ClipboardLocation::Primary => 'p',
    };
    format!("\x1b]52;{selector};{}\x1b\\", STANDARD.encode(text)).into_bytes()
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
    failing: bool,
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

    /// A publish that keeps failing is retried every frame, so only the first failure of a streak is logged.
    fn publish_logged(&mut self, terminal: &Terminal<'static, '_>) -> bool {
        match self.publish(terminal) {
            Ok(()) => self.note_publish(false),
            Err(error) => {
                if self.note_publish(true) {
                    eprintln!("agentd: publish: {error:#}");
                }
                false
            }
        }
    }

    /// Records an attempt's outcome; true when it is the first failure since a success.
    fn note_publish(&mut self, failed: bool) -> bool {
        let first_failure = failed && !self.failing;
        self.failing = failed;
        first_failure
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
pub(crate) fn forget(pid: &Mutex<Option<i32>>) {
    *pid.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

pub(crate) fn terminate(child: &mut Child, pid: &Mutex<Option<i32>>) {
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
    use crate::grid::Metrics;
    use crate::test_support::TEST_SURFACE;

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
            failing: false,
        }
    }
    const DUPLICATED_COMMAND: &[u8] =
        b"\x1b]133;A\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07\x1b]133;C\x07out\r\n\
          \x1b]133;D;2\x07\x1b]133;D;2\x07";

    fn finished_by(bytes: &[u8], threshold: Option<Duration>) -> Vec<(bool, bool)> {
        let mut terminal = new_terminal(20, 5).unwrap();
        let finished = watch_commands(&mut terminal, Rc::new(Cell::new(threshold))).unwrap();
        terminal.vt_write(bytes);
        let finished = finished.borrow();
        finished
            .iter()
            .map(|command| (command.failed, command.long))
            .collect()
    }
    #[test]
    fn duplicate_marks_finish_a_command_once() {
        assert_eq!(
            finished_by(DUPLICATED_COMMAND, Some(Duration::ZERO)),
            [(true, true)]
        );
    }
    #[test]
    fn only_a_command_past_the_threshold_is_long() {
        let hour = Some(Duration::from_secs(3600));
        assert_eq!(finished_by(DUPLICATED_COMMAND, hour), [(true, false)]);
        assert_eq!(finished_by(DUPLICATED_COMMAND, None), [(true, false)]);
    }
    #[test]
    fn a_finish_without_a_start_or_an_exit_code_is_not_a_failure() {
        assert!(finished_by(b"\x1b]133;D;0\x07", Some(Duration::ZERO)).is_empty());
        assert_eq!(
            finished_by(b"\x1b]133;C\x07\x1b]133;D\x07", Some(Duration::ZERO)),
            [(false, true)]
        );
    }
    #[test]
    fn the_first_start_times_the_command() {
        let now = Instant::now();
        let mut timer = CommandTimer::default();
        timer.start(now);
        timer.start(now + Duration::from_secs(9));
        let threshold = Some(Duration::from_secs(5));
        let finished = timer
            .finish(now + Duration::from_secs(10), Some(0), threshold)
            .unwrap();
        assert!(finished.long);
        assert!(
            timer
                .finish(now + Duration::from_secs(11), Some(0), threshold)
                .is_none()
        );
    }
    #[test]
    fn duplicate_prompt_marks_give_one_stop_per_prompt() {
        let mut terminal = new_terminal(20, 3).unwrap();
        for command in 0..3 {
            terminal.vt_write(
                format!(
                    "\x1b]133;A\x07\x1b]133;A\x07$ \x1b]133;B\x07c{command}\r\n\
                     \x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;D;0\x07"
                )
                .as_bytes(),
            );
        }
        terminal.vt_write(b"\x1b]133;A\x07$ ");
        let offset = |terminal: &mut Terminal<'static, '_>, delta| {
            terminal.scroll_viewport(ScrollViewport::DeltaPrompt(delta));
            terminal.scrollbar().unwrap().offset
        };
        // Each command takes two rows, so prompts sit on rows 0, 2, 4 and 6 and the view starts at 4.
        assert_eq!(offset(&mut terminal, -1), 2);
        assert_eq!(offset(&mut terminal, -1), 0);
        assert_eq!(offset(&mut terminal, -1), 0);
        assert_eq!(offset(&mut terminal, 1), 2);
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
    fn the_first_resize_applies_at_once() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now, true);
        assert_eq!(debounce.take_due(now), Some(TEST_SURFACE));
        assert_eq!(debounce.due, None);
    }
    #[test]
    fn a_burst_of_resizes_applies_once_it_settles() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now, true);
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
                true,
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
    fn a_background_resize_settles_later_unless_shown() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now, false);
        debounce.take_due(now);
        let wider = Surface {
            cols: 30,
            ..TEST_SURFACE
        };
        debounce.request(wider, now, false);
        assert_eq!(debounce.due, Some(now + BACKGROUND_RESIZE_SETTLE));
        assert_eq!(debounce.take_due(now + RESIZE_SETTLE), None);
        debounce.hurry(now + RESIZE_SETTLE);
        assert_eq!(debounce.take_due(now + RESIZE_SETTLE), Some(wider));
        debounce.hurry(now + RESIZE_SETTLE);
        assert_eq!(debounce.due, None);
    }

    #[test]
    fn a_change_of_scale_alone_applies_at_once() {
        let now = Instant::now();
        let mut debounce = ResizeDebounce::default();
        debounce.request(TEST_SURFACE, now, true);
        debounce.take_due(now);
        let rescaled = Surface {
            metrics: Metrics {
                scale: 2.0,
                ..TEST_SURFACE.metrics
            },
            ..TEST_SURFACE
        };
        debounce.request(rescaled, now, true);
        assert_eq!(debounce.take_due(now), Some(rescaled));
    }
    #[test]
    fn only_the_first_failure_of_a_streak_is_logged() {
        let mut publisher = idle_publisher(Instant::now());
        let logged: Vec<bool> = [true, true, false, true, true]
            .into_iter()
            .map(|failed| publisher.note_publish(failed))
            .collect();
        assert_eq!(logged, [true, false, false, true, false]);
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
    /// What the program would receive after `input` is written to a terminal with the given access.
    fn clipboard_session(
        read: ClipboardAccess,
        write: ClipboardAccess,
        input: &[u8],
    ) -> (Vec<Event>, Vec<u8>) {
        let mut terminal = new_terminal(10, 2).unwrap();
        let (events_tx, events) = crossbeam_channel::unbounded();
        let (replies_tx, replies) = crossbeam_channel::unbounded();
        let reply_muted = Rc::new(Cell::new(false));
        terminal
            .on_pty_write({
                let reply_muted = reply_muted.clone();
                move |_, data| queue_reply(&replies_tx, &reply_muted, data)
            })
            .unwrap();
        install_clipboard(
            &mut terminal,
            read,
            write,
            reply_muted,
            Rc::new(move |event| events_tx.send(event).unwrap()),
        )
        .unwrap();
        terminal.vt_write(input);
        (
            events.try_iter().collect(),
            replies.try_iter().flatten().collect(),
        )
    }

    const WRITE_HI: &[u8] = b"\x1b]52;c;aGk=\x07";
    const READ: &[u8] = b"\x1b]52;c;?\x07";

    #[test]
    fn an_osc_52_write_reaches_the_ui_when_allowed() {
        let (events, replies) =
            clipboard_session(ClipboardAccess::Ask, ClipboardAccess::Allow, WRITE_HI);
        assert!(
            matches!(&events[..], [Event::ClipboardWrite { text, ask: false }] if text == "hi")
        );
        assert!(replies.is_empty());
    }

    #[test]
    fn an_osc_52_write_waits_for_the_user_when_asking() {
        let (events, _) = clipboard_session(ClipboardAccess::Ask, ClipboardAccess::Ask, WRITE_HI);
        assert!(matches!(&events[..], [Event::ClipboardWrite { text, ask: true }] if text == "hi"));
    }

    #[test]
    fn a_denied_osc_52_write_goes_nowhere() {
        let (events, _) = clipboard_session(ClipboardAccess::Ask, ClipboardAccess::Deny, WRITE_HI);
        assert!(events.is_empty());
    }

    #[test]
    fn an_osc_52_read_is_held_for_the_ui_with_no_reply_yet() {
        let (events, replies) =
            clipboard_session(ClipboardAccess::Ask, ClipboardAccess::Allow, READ);
        assert!(matches!(
            &events[..],
            [Event::ClipboardRead {
                location: ClipboardLocation::Standard,
                ask: true
            }]
        ));
        assert!(replies.is_empty());
    }

    #[test]
    fn an_osc_52_read_goes_ahead_without_asking_when_allowed() {
        let (events, _) = clipboard_session(ClipboardAccess::Allow, ClipboardAccess::Allow, READ);
        assert!(matches!(
            &events[..],
            [Event::ClipboardRead { ask: false, .. }]
        ));
    }

    #[test]
    fn a_denied_osc_52_read_gets_an_empty_clipboard() {
        let (events, replies) =
            clipboard_session(ClipboardAccess::Deny, ClipboardAccess::Allow, READ);
        assert!(events.is_empty());
        assert_eq!(replies, b"\x1b]52;c;\x07");
    }

    #[test]
    fn the_muted_reply_does_not_swallow_later_replies() {
        let (_, replies) = clipboard_session(
            ClipboardAccess::Ask,
            ClipboardAccess::Allow,
            b"\x1b]52;c;?\x07\x1b[c",
        );
        assert!(replies.starts_with(b"\x1b[?"), "{replies:?}");
    }

    #[test]
    fn a_clipboard_reply_is_an_osc_52_with_the_text_in_base64() {
        assert_eq!(
            osc52_reply(ClipboardLocation::Standard, "hi"),
            b"\x1b]52;c;aGk=\x1b\\"
        );
        assert_eq!(
            osc52_reply(ClipboardLocation::Primary, ""),
            b"\x1b]52;p;\x1b\\"
        );
    }

    #[test]
    fn echo_off_with_line_editing_is_a_password_prompt() {
        use nix::sys::termios::{SetArg, tcsetattr};
        let pty = nix::pty::openpty(None, None).unwrap();
        assert!(!password_prompt(&pty.master));
        let mut termios = tcgetattr(&pty.slave).unwrap();
        termios.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&pty.slave, SetArg::TCSANOW, &termios).unwrap();
        assert!(password_prompt(&pty.master));
        termios.local_flags.remove(LocalFlags::ICANON);
        tcsetattr(&pty.slave, SetArg::TCSANOW, &termios).unwrap();
        assert!(!password_prompt(&pty.master));
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
}
