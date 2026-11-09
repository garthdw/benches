use std::cell::RefCell;
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, never, select, unbounded};
use libghostty_vt::render::{CellIterator, Dirty, RowIterator};
use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::Mode;
use libghostty_vt::{RenderState, Terminal, key};
use nix::errno::Errno;
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rune::Waker;

use crate::input::KeyInput;

const FRAME: Duration = Duration::from_millis(8);
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const MESSAGE_COLS: u16 = 80;
const DEFAULT_FG: RgbColor = RgbColor {
    r: 0xd8,
    g: 0xd9,
    b: 0xda,
};
const DEFAULT_BG: RgbColor = RgbColor {
    r: 0x14,
    g: 0x15,
    b: 0x17,
};

trait LogErr {
    fn log_err(self, context: &str);
}

impl<T, E: Display> LogErr for std::result::Result<T, E> {
    fn log_err(self, context: &str) {
        if let Err(error) = self {
            eprintln!("agentd: {context}: {error}");
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub character: char,
    pub fg: [u8; 3],
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
}

pub struct Grid {
    pub generation: u64,
    pub cols: u16,
    pub cells: Vec<Cell>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_color: [u8; 3],
    pub bg: [u8; 3],
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            generation: 0,
            cols: 0,
            cells: Vec::new(),
            cursor: None,
            cursor_color: [0; 3],
            bg: rgb(DEFAULT_BG),
        }
    }
}

impl Grid {
    /// Parses `bytes` on the calling thread, for fixtures and tests.
    pub fn from_vt(bytes: &[u8], cols: u16, rows: u16) -> Result<Self> {
        let mut terminal = new_terminal(cols, rows)?;
        terminal.vt_write(bytes);
        GridBuilder::new()?
            .build(&terminal, 1)?
            .context("a new terminal should be dirty")
    }

    /// Plain text built without libghostty, so it can report libghostty failures.
    pub fn message(text: &str) -> Self {
        let blank = Cell {
            character: ' ',
            fg: rgb(DEFAULT_FG),
            bg: None,
            bold: false,
        };
        let mut cells: Vec<Cell> = text
            .chars()
            .map(|character| Cell { character, ..blank })
            .collect();
        cells.resize(
            cells.len().next_multiple_of(usize::from(MESSAGE_COLS)),
            blank,
        );
        Self {
            // A live session never reaches this, so the UI always sees the message as a change.
            generation: u64::MAX,
            cols: MESSAGE_COLS,
            cells,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.cells
            .chunks(usize::from(self.cols).max(1))
            .map(|row| row.iter().map(|cell| cell.character).collect())
            .collect()
    }
}

fn new_terminal<'cb>(cols: u16, rows: u16) -> Result<Terminal<'static, 'cb>> {
    let mut terminal = Terminal::new(cols, rows)?;
    terminal
        .set_scrollback_max_lines(Some(10_000))?
        .set_terminfo_name("xterm-256color")?
        .set_default_fg_color(Some(DEFAULT_FG))?
        .set_default_bg_color(Some(DEFAULT_BG))?;
    Ok(terminal)
}

fn rgb(color: RgbColor) -> [u8; 3] {
    [color.r, color.g, color.b]
}

struct GridBuilder {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
}

impl GridBuilder {
    fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
        })
    }

    fn build(&mut self, terminal: &Terminal<'static, '_>, generation: u64) -> Result<Option<Grid>> {
        let snapshot = self.state.update(terminal)?;
        if matches!(snapshot.dirty()?, Dirty::Clean) {
            return Ok(None);
        }
        let colors = snapshot.colors()?;
        let (cols, rows) = (snapshot.cols()?, snapshot.rows()?);
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        let mut row_iterator = self.rows.update(&snapshot)?;
        while let Some(row) = row_iterator.next() {
            let mut cell_iterator = self.cells.update(row)?;
            while let Some(cell) = cell_iterator.next() {
                let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                let mut bg = cell.bg_color()?;
                let mut bold = false;
                if cell.has_styling()? {
                    let style = cell.style()?;
                    bold = style.bold;
                    if style.inverse {
                        let old_fg = fg;
                        fg = bg.unwrap_or(colors.background);
                        bg = Some(old_fg);
                    }
                }
                let mut character = [' '];
                match cell.graphemes_buf(&mut character) {
                    // The buffer holds the base codepoint even when the grapheme is longer.
                    Ok(()) | Err(libghostty_vt::Error::OutOfSpace { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
                cells.push(Cell {
                    character: character[0],
                    fg: rgb(fg),
                    bg: bg.map(rgb),
                    bold,
                });
            }
        }
        let cursor = if snapshot.cursor_visible()? {
            snapshot
                .cursor_viewport()?
                .map(|viewport| (viewport.x, viewport.y))
        } else {
            None
        };
        snapshot.clean()?;
        Ok(Some(Grid {
            generation,
            cols,
            cells,
            cursor,
            cursor_color: rgb(colors.cursor.unwrap_or(colors.foreground)),
            bg: rgb(colors.background),
        }))
    }
}

enum Msg {
    Key(KeyInput),
    Resize {
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
}

/// A process on a PTY. Its `Terminal` lives on an actor thread, as libghostty types are `!Send`.
pub struct Session {
    commands: Option<Sender<Msg>>,
    grid: Arc<Mutex<Arc<Grid>>>,
}

impl Session {
    pub fn spawn(command: Option<&str>, cwd: &Path, waker: Waker) -> Result<Self> {
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

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut process = Command::new(shell);
        process.args(["-l", "-i"]);
        if let Some(command) = command {
            process.arg("-c").arg(format!("exec {command}"));
        }
        process
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "agentd")
            .stdin(Stdio::from(pty.slave));
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
        let mut actor = Actor {
            cols,
            rows,
            output: output_rx,
            commands: commands_rx,
            input: input_tx,
            child,
            master,
            slot: grid.clone(),
            waker,
        };
        thread::spawn(move || {
            if let Err(error) = actor.run() {
                store(
                    &actor.slot,
                    Grid::message(&format!("terminal failed: {error:#}")),
                );
                (actor.waker)();
            }
            terminate(&mut actor.child);
        });
        Ok(Self {
            commands: Some(commands),
            grid,
        })
    }

    /// A session with no process, showing `grid`. Input and resizes go nowhere.
    pub fn fixture(grid: Grid) -> Self {
        Self {
            commands: None,
            grid: Arc::new(Mutex::new(Arc::new(grid))),
        }
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

    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        self.send(Msg::Resize {
            cols,
            rows,
            cell_width,
            cell_height,
        });
    }

    fn send(&self, message: Msg) {
        if let Some(commands) = &self.commands {
            commands.send(message).log_err("send to session");
        }
    }
}

fn set_cloexec(fd: &OwnedFd) -> Result<()> {
    use nix::fcntl::{F_SETFD, FdFlag, fcntl};
    fcntl(fd, F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(())
}

fn store(slot: &Mutex<Arc<Grid>>, grid: Grid) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(grid);
}

struct Actor {
    cols: u16,
    rows: u16,
    output: Receiver<Vec<u8>>,
    commands: Receiver<Msg>,
    input: Sender<Vec<u8>>,
    child: Child,
    master: OwnedFd,
    slot: Arc<Mutex<Arc<Grid>>>,
    waker: Waker,
}

impl Actor {
    fn run(&mut self) -> Result<()> {
        let mut terminal = new_terminal(self.cols, self.rows)?;
        let publisher = Rc::new(RefCell::new(Publisher {
            builder: GridBuilder::new()?,
            slot: self.slot.clone(),
            waker: self.waker.clone(),
            generation: 0,
            next_frame: Instant::now(),
            hold_started: None,
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
            if held && Instant::now() >= publisher.next_frame {
                publisher.publish(terminal).log_err("publish held frame");
            }
        })?;

        let mut encoder = key::Encoder::new()?;
        let mut event = key::Event::new()?;
        let mut dirty = true;
        loop {
            let wait = publisher.borrow().wait(dirty, Instant::now());
            select! {
                recv(self.output) -> bytes => match bytes {
                    Ok(bytes) => { terminal.vt_write(&bytes); dirty = true; }
                    Err(_) => {
                        self.output = never();
                        self.child.wait().log_err("wait for process");
                        terminal.vt_write(b"\r\n[process exited]\r\n");
                        dirty = true;
                    }
                },
                recv(self.commands) -> command => match command {
                    Ok(Msg::Key(input)) => {
                        event
                            .set_action(key::Action::Press)
                            .set_key(input.key)
                            .set_mods(input.mods)
                            .set_consumed_mods(input.consumed)
                            .set_unshifted_codepoint(input.unshifted)
                            .set_utf8(input.text);
                        let mut encoded = Vec::new();
                        encoder
                            .set_options_from_terminal(&terminal)
                            .encode_to_vec(&event, &mut encoded)
                            .log_err("encode key");
                        if !encoded.is_empty() {
                            self.input.send(encoded).log_err("queue key input");
                        }
                    }
                    Ok(Msg::Resize { cols, rows, cell_width, cell_height }) => {
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
                        dirty = true;
                    }
                    Err(_) => return Ok(()),
                },
                default(wait) => {}
            }
            let mut publisher = publisher.borrow_mut();
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
            if dirty && publisher.hold_started.is_none() && Instant::now() >= publisher.next_frame {
                publisher.publish(&terminal)?;
                dirty = false;
            }
        }
    }
}

struct Publisher {
    builder: GridBuilder,
    slot: Arc<Mutex<Arc<Grid>>>,
    waker: Waker,
    generation: u64,
    next_frame: Instant,
    hold_started: Option<Instant>,
}

impl Publisher {
    fn wait(&self, dirty: bool, now: Instant) -> Duration {
        match self.hold_started {
            Some(started) => (started + HOLD_TIMEOUT).saturating_duration_since(now),
            None if dirty => self.next_frame.saturating_duration_since(now),
            None => Duration::from_secs(3600),
        }
    }

    fn publish(&mut self, terminal: &Terminal<'static, '_>) -> Result<()> {
        self.next_frame = Instant::now() + FRAME;
        if let Some(grid) = self.builder.build(terminal, self.generation + 1)? {
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

fn terminate(child: &mut Child) {
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

    #[test]
    fn parses_text_and_colour_into_the_grid() {
        let grid = Grid::from_vt(b"hi \x1b[1;31mred\x1b[0m", 10, 2).unwrap();
        assert_eq!(grid.lines()[0], "hi red    ");
        let red = grid.cells[3];
        assert_eq!((red.character, red.bold), ('r', true));
        assert_ne!(red.fg, rgb(DEFAULT_FG));
        assert_eq!(grid.cursor, Some((6, 0)));
    }

    #[test]
    fn a_message_fills_whole_rows_and_counts_as_a_change() {
        let grid = Grid::message("no shell");
        assert_eq!(grid.lines(), vec![format!("{:<80}", "no shell")]);
        assert_ne!(grid.generation, Grid::default().generation);
    }

    #[test]
    fn an_unchanged_terminal_builds_nothing_but_a_cursor_move_does() {
        let mut terminal = new_terminal(10, 4).unwrap();
        let mut builder = GridBuilder::new().unwrap();
        terminal.vt_write(b"hi");
        assert!(builder.build(&terminal, 1).unwrap().is_some());
        assert!(builder.build(&terminal, 2).unwrap().is_none());
        terminal.vt_write(b"\x1b[3;4H");
        assert_eq!(
            builder.build(&terminal, 3).unwrap().unwrap().cursor,
            Some((3, 2))
        );
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
        let deadline = Instant::now() + Duration::from_secs(15);
        while !session
            .grid()
            .lines()
            .iter()
            .any(|line| line.trim_end() == want)
        {
            assert!(
                Instant::now() < deadline,
                "{want} never arrived: {:#?}",
                session.grid().lines()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_held_frame_sleeps_until_the_hold_deadline() {
        let now = Instant::now();
        let publisher = Publisher {
            builder: GridBuilder::new().unwrap(),
            slot: Arc::default(),
            waker: Arc::new(|| {}),
            generation: 0,
            next_frame: now,
            hold_started: Some(now),
        };
        assert_eq!(publisher.wait(true, now), HOLD_TIMEOUT);
    }

    #[test]
    fn output_after_a_hold_that_never_ends_still_appears() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(
            &session,
            "printf '\\033[?2026h'; echo held-$((1+1)); sleep 30",
        );
        wait_for_line(&session, "held-2");
    }

    #[test]
    fn a_real_process_reaches_the_grid_and_takes_keys() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&session, "echo agentd-$((40+2))");
        wait_for_line(&session, "agentd-42");
    }

    #[test]
    fn ctrl_c_interrupts_the_foreground_process() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&session, "sleep 30; echo slept; echo marker-$((1+1))");
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
        type_line(&session, "echo after-$((2+2))");
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
