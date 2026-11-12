use std::cell::RefCell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::Child;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, never, select};
use libghostty_vt::mode::Mode;
use libghostty_vt::{Terminal, focus, key};
use nix::errno::Errno;
use nix::pty::Winsize;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rune::Waker;

use crate::LogErr;
use crate::config::Theme;
use crate::grid::{
    FRAME, Grid, GridBuilder, Presentation, ScrollbarFade, Surface, apply_theme, new_terminal,
};
use crate::protocol::{Event, Msg, Visibility};
use crate::search::Searcher;
use crate::terminal_input::{TerminalInput, open_link, paste_text, selection_text};

/// How long the size must hold before the program sees it, as each SIGWINCH makes inline TUIs reprint.
const RESIZE_SETTLE: Duration = Duration::from_millis(100);
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const BLINK: Duration = Duration::from_millis(530);

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
    pub(crate) link_hovered: Arc<AtomicBool>,
    pub(crate) events: Sender<Event>,
    pub(crate) waker: Waker,
    pub(crate) theme: Theme,
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

    pub(crate) fn run(&mut self) -> Result<()> {
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
        let mut searcher = Searcher::default();
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
                    Ok(Msg::Search(query)) => {
                        searcher.set(&mut terminal, &query).log_err("search");
                        dirty = true;
                    }
                    Ok(Msg::SearchStep { older }) => {
                        searcher.step(&mut terminal, older).log_err("step search");
                        dirty = true;
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
                if let Some((highlights, found)) =
                    terminal.cols().log_err("read columns").and_then(|cols| {
                        searcher
                            .highlights(&mut terminal, cols)
                            .log_err("find matches")
                    })
                {
                    publisher.builder.set_search(highlights, found);
                }
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
