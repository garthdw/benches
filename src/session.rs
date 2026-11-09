use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded, never, select, unbounded};
use libghostty_vt::render::{CellIterator, Dirty, RowIterator};
use libghostty_vt::style::RgbColor;
use libghostty_vt::{RenderState, Terminal, key};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rune::Waker;

use crate::input::KeyInput;

const FRAME: Duration = Duration::from_millis(8);
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

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
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
    pub fn from_vt(bytes: &[u8], cols: u16, rows: u16) -> Self {
        let mut terminal = new_terminal(cols, rows);
        terminal.vt_write(bytes);
        GridBuilder::new()
            .build(&terminal, 1)
            .expect("a new terminal is dirty")
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.cells
            .chunks(usize::from(self.cols).max(1))
            .map(|row| row.iter().map(|c| c.ch).collect())
            .collect()
    }
}

fn new_terminal<'cb>(cols: u16, rows: u16) -> Terminal<'static, 'cb> {
    let mut terminal = Terminal::new(cols, rows).expect("create terminal");
    terminal
        .set_scrollback_max_lines(Some(10_000))
        .expect("scrollback")
        .set_terminfo_name("xterm-256color")
        .expect("terminfo")
        .set_default_fg_color(Some(DEFAULT_FG))
        .expect("fg")
        .set_default_bg_color(Some(DEFAULT_BG))
        .expect("bg");
    terminal
}

fn rgb(c: RgbColor) -> [u8; 3] {
    [c.r, c.g, c.b]
}

struct GridBuilder {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
}

impl GridBuilder {
    fn new() -> Self {
        Self {
            state: RenderState::new().expect("render state"),
            rows: RowIterator::new().expect("row iterator"),
            cells: CellIterator::new().expect("cell iterator"),
        }
    }

    fn build(&mut self, terminal: &Terminal<'static, '_>, generation: u64) -> Option<Grid> {
        let snapshot = self.state.update(terminal).expect("render snapshot");
        if matches!(snapshot.dirty().expect("dirty"), Dirty::Clean) {
            return None;
        }
        let colors = snapshot.colors().expect("colors");
        let (cols, rows) = (
            snapshot.cols().expect("cols"),
            snapshot.rows().expect("rows"),
        );
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        let mut row_it = self.rows.update(&snapshot).expect("rows");
        while let Some(row) = row_it.next() {
            let mut cell_it = self.cells.update(row).expect("cells");
            while let Some(cell) = cell_it.next() {
                let mut fg = cell.fg_color().expect("fg").unwrap_or(colors.foreground);
                let mut bg = cell.bg_color().expect("bg");
                let mut bold = false;
                if cell.has_styling().expect("styling") {
                    let style = cell.style().expect("style");
                    bold = style.bold;
                    if style.inverse {
                        let old_fg = fg;
                        fg = bg.unwrap_or(colors.background);
                        bg = Some(old_fg);
                    }
                }
                let mut ch = [' '];
                // Fills the buffer with the base codepoint even when the grapheme is longer.
                let _ = cell.graphemes_buf(&mut ch);
                cells.push(Cell {
                    ch: ch[0],
                    fg: rgb(fg),
                    bg: bg.map(rgb),
                    bold,
                });
            }
        }
        let cursor = if snapshot.cursor_visible().expect("cursor") {
            snapshot
                .cursor_viewport()
                .expect("cursor")
                .map(|c| (c.x, c.y))
        } else {
            None
        };
        snapshot.clean().expect("clean");
        Some(Grid {
            generation,
            cols,
            cells,
            cursor,
            cursor_color: rgb(colors.cursor.unwrap_or(colors.foreground)),
            bg: rgb(colors.background),
        })
    }
}

enum Msg {
    Key(KeyInput),
    Resize {
        cols: u16,
        rows: u16,
        cell_w: u16,
        cell_h: u16,
    },
}

/// A process on a PTY. Its `Terminal` lives on an actor thread, as libghostty types are `!Send`.
pub struct Session {
    commands: Sender<Msg>,
    grid: Arc<Mutex<Arc<Grid>>>,
}

impl Session {
    pub fn spawn(command: Option<&str>, cwd: &Path, waker: Waker) -> std::io::Result<Self> {
        let (cols, rows) = (80, 24);
        let size = Winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).map_err(std::io::Error::from)?;
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
        let mut child = process.spawn()?;

        let master = pty.master;
        let (output_tx, mut output_rx) = bounded(64);
        let mut reader = File::from(master.try_clone()?);
        thread::spawn(move || {
            let mut buf = [0; 64 * 1024];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if output_tx.send(buf[..n].to_vec()).is_err() {
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
        let slot = grid.clone();
        thread::spawn(move || {
            let mut terminal = new_terminal(cols, rows);
            let publisher = Rc::new(RefCell::new(Publisher {
                builder: GridBuilder::new(),
                slot,
                waker,
                generation: 0,
                next_frame: Instant::now(),
                held: false,
            }));

            let pty_input = input_tx.clone();
            terminal
                .on_pty_write(move |_, data| {
                    let _ = pty_input.send(data.to_vec());
                })
                .expect("pty write");
            let hold_publisher = publisher.clone();
            terminal
                .on_render_hold(move |term, held| {
                    let mut publisher = hold_publisher.borrow_mut();
                    publisher.held = held;
                    // Keep the finished frame from before the hold, or a program that redraws constantly looks frozen.
                    if held && Instant::now() >= publisher.next_frame {
                        publisher.publish(term);
                    }
                })
                .expect("render hold");

            let mut encoder = key::Encoder::new().expect("key encoder");
            let mut event = key::Event::new().expect("key event");
            let mut dirty = true;
            let mut exited = false;
            loop {
                let wait = if dirty {
                    publisher
                        .borrow()
                        .next_frame
                        .saturating_duration_since(Instant::now())
                } else {
                    Duration::from_secs(3600)
                };
                select! {
                    recv(output_rx) -> bytes => match bytes {
                        Ok(bytes) => { terminal.vt_write(&bytes); dirty = true; }
                        Err(_) => {
                            output_rx = never();
                            let _ = child.wait();
                            exited = true;
                            terminal.vt_write(b"\r\n[process exited]\r\n");
                            dirty = true;
                        }
                    },
                    recv(commands_rx) -> command => match command {
                        Ok(Msg::Key(input)) => {
                            event
                                .set_action(key::Action::Press)
                                .set_key(input.key)
                                .set_mods(input.mods)
                                .set_consumed_mods(input.consumed)
                                .set_unshifted_codepoint(input.unshifted)
                                .set_utf8(input.text);
                            let mut encoded = Vec::new();
                            if encoder.set_options_from_terminal(&terminal).encode_to_vec(&event, &mut encoded).is_ok()
                                && !encoded.is_empty()
                            {
                                let _ = input_tx.send(encoded);
                            }
                        }
                        Ok(Msg::Resize { cols, rows, cell_w, cell_h }) => {
                            let _ = terminal.resize(cols, rows, u32::from(cell_w), u32::from(cell_h));
                            let size = Winsize {
                                ws_col: cols,
                                ws_row: rows,
                                ws_xpixel: cols.saturating_mul(cell_w),
                                ws_ypixel: rows.saturating_mul(cell_h),
                            };
                            // SAFETY: `master` is an open PTY and `size` outlives the call.
                            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
                            dirty = true;
                        }
                        Err(_) => break,
                    },
                    default(wait) => {}
                }
                let mut publisher = publisher.borrow_mut();
                if dirty && !publisher.held && Instant::now() >= publisher.next_frame {
                    publisher.publish(&terminal);
                    dirty = false;
                }
            }
            if !exited {
                terminate(&mut child);
            }
        });
        Ok(Self { commands, grid })
    }

    /// A session with no process, showing `grid`. Input and resizes go nowhere.
    pub fn fixture(grid: Grid) -> Self {
        Self {
            commands: unbounded().0,
            grid: Arc::new(Mutex::new(Arc::new(grid))),
        }
    }

    pub fn grid(&self) -> Arc<Grid> {
        self.grid.lock().unwrap().clone()
    }

    pub fn key(&self, key: KeyInput) {
        let _ = self.commands.send(Msg::Key(key));
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        let _ = self.commands.send(Msg::Resize {
            cols,
            rows,
            cell_w,
            cell_h,
        });
    }
}

fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
    use nix::fcntl::{F_SETFD, FdFlag, fcntl};
    fcntl(fd, F_SETFD(FdFlag::FD_CLOEXEC)).map_err(std::io::Error::from)?;
    Ok(())
}

struct Publisher {
    builder: GridBuilder,
    slot: Arc<Mutex<Arc<Grid>>>,
    waker: Waker,
    generation: u64,
    next_frame: Instant,
    held: bool,
}

impl Publisher {
    fn publish(&mut self, terminal: &Terminal<'static, '_>) {
        self.next_frame = Instant::now() + FRAME;
        if let Some(grid) = self.builder.build(terminal, self.generation + 1) {
            self.generation = grid.generation;
            *self.slot.lock().unwrap() = Arc::new(grid);
            (self.waker)();
        }
    }
}

fn terminate(child: &mut Child) {
    let group = Pid::from_raw(child.id() as i32);
    let _ = killpg(group, Signal::SIGHUP);
    for _ in 0..50 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = killpg(group, Signal::SIGKILL);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_colour_into_the_grid() {
        let grid = Grid::from_vt(b"hi \x1b[1;31mred\x1b[0m", 10, 2);
        assert_eq!(grid.lines()[0], "hi red    ");
        let r = grid.cells[3];
        assert_eq!((r.ch, r.bold), ('r', true));
        assert_ne!(r.fg, rgb(DEFAULT_FG));
        assert_eq!(grid.cursor, Some((6, 0)));
    }

    #[test]
    fn an_unchanged_terminal_builds_nothing_but_a_cursor_move_does() {
        let mut terminal = new_terminal(10, 4);
        let mut builder = GridBuilder::new();
        terminal.vt_write(b"hi");
        assert!(builder.build(&terminal, 1).is_some());
        assert!(builder.build(&terminal, 2).is_none());
        terminal.vt_write(b"\x1b[3;4H");
        assert_eq!(builder.build(&terminal, 3).unwrap().cursor, Some((3, 2)));
    }

    fn type_line(session: &Session, line: &str) {
        for c in line.chars() {
            session.key(crate::input::map_text(
                &c.to_string(),
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
