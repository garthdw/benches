use std::cell::RefCell;
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded, never, select, unbounded};
use libghostty_vt::render::{CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::screen::{CellWide, Screen};
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
};
use libghostty_vt::style::{RgbColor, StyleColor, Underline};
use libghostty_vt::terminal::{ClipboardMime, Mode, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{RenderState, Terminal, fmt, focus, key, mouse, paste};
use nix::errno::Errno;
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rune::Waker;

use crate::input::KeyInput;

const FRAME: Duration = Duration::from_millis(8);
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const BLINK: Duration = Duration::from_millis(530);
const AUTOSCROLL: Duration = Duration::from_millis(50);
const MULTI_CLICK: Duration = Duration::from_millis(500);
/// Fixed until `config.toml` arrives in M5.
const LAUNCH_SHELL: &str = "/opt/homebrew/bin/fish";
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

trait LogErr<T> {
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub cell_width: u16,
    pub cell_height: u16,
    pub padding: f32,
    pub scale: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub cols: u16,
    pub rows: u16,
    pub metrics: Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Press,
    Drag,
    Release,
    Move,
}

/// Positions are in points, relative to the terminal surface.
#[derive(Clone, Copy, Debug)]
pub struct PointerInput {
    pub phase: PointerPhase,
    pub button: Option<mouse::Button>,
    pub x: f32,
    pub y: f32,
    pub mods: key::Mods,
}

#[derive(Clone, Copy, Debug)]
pub struct WheelInput {
    pub dy: f32,
    pub x: f32,
    pub y: f32,
    pub mods: key::Mods,
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub character: char,
    pub fg: [u8; 3],
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
    pub underline_color: Option<[u8; 3]>,
    pub wide: bool,
    pub spacer: bool,
    /// Index into `Grid::clusters` when the cell holds more than one codepoint.
    pub cluster: Option<u32>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: rgb(DEFAULT_FG),
            bg: None,
            bold: false,
            italic: false,
            strikethrough: false,
            overline: false,
            underline: Underline::None,
            underline_color: None,
            wide: false,
            spacer: false,
            cluster: None,
        }
    }
}

pub struct Grid {
    pub generation: u64,
    pub cols: u16,
    pub cells: Vec<Cell>,
    pub clusters: Vec<String>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_style: CursorVisualStyle,
    pub cursor_blinking: bool,
    pub cursor_color: [u8; 3],
    pub metrics: Metrics,
    pub bg: [u8; 3],
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            generation: 0,
            cols: 0,
            cells: Vec::new(),
            clusters: Vec::new(),
            cursor: None,
            cursor_style: CursorVisualStyle::Block,
            cursor_blinking: false,
            cursor_color: [0; 3],
            metrics: Metrics::default(),
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
            .build(&terminal, 1, Presentation::default())?
            .context("a new terminal should be dirty")
    }

    /// Plain text built without libghostty, so it can report libghostty failures.
    pub fn message(text: &str) -> Self {
        let mut cells: Vec<Cell> = text
            .chars()
            .map(|character| Cell {
                character,
                ..Cell::default()
            })
            .collect();
        cells.resize(
            cells.len().next_multiple_of(usize::from(MESSAGE_COLS)),
            Cell::default(),
        );
        Self {
            // A live session never reaches this, so the UI always sees the message as a change.
            generation: u64::MAX,
            cols: MESSAGE_COLS,
            cells,
            ..Self::default()
        }
    }

    pub fn cluster(&self, cell: &Cell) -> Option<&str> {
        let index = usize::try_from(cell.cluster?).ok()?;
        self.clusters.get(index).map(String::as_str)
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        self.cells
            .chunks(usize::from(self.cols).max(1))
            .map(|row| {
                let mut line = String::new();
                for cell in row.iter().filter(|cell| !cell.spacer) {
                    match self.cluster(cell) {
                        Some(cluster) => line.push_str(cluster),
                        None => line.push(cell.character),
                    }
                }
                line
            })
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
    terminal.set_mode(Mode::GRAPHEME_CLUSTER, true)?;
    Ok(terminal)
}

fn rgb(color: RgbColor) -> [u8; 3] {
    [color.r, color.g, color.b]
}

fn faint(color: [u8; 3], background: [u8; 3]) -> [u8; 3] {
    std::array::from_fn(|index| {
        ((u16::from(color[index]) + u16::from(background[index])) / 2) as u8
    })
}

/// What the actor knows that the terminal doesn't.
#[derive(Clone, Copy, PartialEq)]
struct Presentation {
    focused: bool,
    cursor_on: bool,
    metrics: Metrics,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            focused: true,
            cursor_on: true,
            metrics: Metrics::default(),
        }
    }
}

struct GridBuilder {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    last_shown: Option<(CursorVisualStyle, bool, Presentation)>,
}

impl GridBuilder {
    fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            last_shown: None,
        })
    }

    /// libghostty isn't known to mark the render state dirty when only the selection moves.
    fn selection_changed(&mut self) {
        self.last_shown = None;
    }

    fn build(
        &mut self,
        terminal: &Terminal<'static, '_>,
        generation: u64,
        presentation: Presentation,
    ) -> Result<Option<Grid>> {
        let snapshot = self.state.update(terminal)?;
        let cursor_style = if presentation.focused {
            snapshot.cursor_visual_style()?
        } else {
            CursorVisualStyle::BlockHollow
        };
        let cursor_blinking = snapshot.cursor_blinking()? && snapshot.cursor_visible()?;
        let shown = (cursor_style, cursor_blinking, presentation);
        // libghostty leaves the render state clean when only the cursor's style changes.
        let unchanged = self.last_shown.replace(shown) == Some(shown);
        if matches!(snapshot.dirty()?, Dirty::Clean) && unchanged {
            return Ok(None);
        }
        let colors = snapshot.colors()?;
        let (cols, rows) = (snapshot.cols()?, snapshot.rows()?);
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        let mut clusters = Vec::new();
        let mut row_iterator = self.rows.update(&snapshot)?;
        while let Some(row) = row_iterator.next() {
            let mut cell_iterator = self.cells.update(row)?;
            while let Some(cell) = cell_iterator.next() {
                let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                let mut bg = cell.bg_color()?;
                let mut output = Cell::default();
                let mut invisible = false;
                let mut is_faint = false;
                let mut inverted = cell.is_selected()?;
                if cell.has_styling()? {
                    let style = cell.style()?;
                    output.bold = style.bold;
                    output.italic = style.italic;
                    is_faint = style.faint;
                    output.strikethrough = style.strikethrough;
                    output.overline = style.overline;
                    output.underline = style.underline;
                    output.underline_color = match style.underline_color {
                        StyleColor::None => None,
                        StyleColor::Rgb(color) => Some(rgb(color)),
                        StyleColor::Palette(index) => {
                            Some(rgb(colors.palette[usize::from(index.0)]))
                        }
                    };
                    invisible = style.invisible;
                    inverted ^= style.inverse;
                }
                if inverted {
                    let old_fg = fg;
                    fg = bg.unwrap_or(colors.background);
                    bg = Some(old_fg);
                }
                let width = cell.raw_cell()?.wide()?;
                output.wide = width == CellWide::Wide;
                output.spacer = matches!(width, CellWide::SpacerTail | CellWide::SpacerHead);
                output.fg = if is_faint {
                    faint(rgb(fg), rgb(bg.unwrap_or(colors.background)))
                } else {
                    rgb(fg)
                };
                output.bg = bg.map(rgb);
                if !invisible {
                    let mut character = [' '];
                    match cell.graphemes_buf(&mut character) {
                        Ok(()) => {}
                        // The buffer holds the base codepoint even when the grapheme is longer.
                        Err(libghostty_vt::Error::OutOfSpace { .. }) => {
                            let mut cluster = String::new();
                            cell.graphemes_utf8(&mut cluster)?;
                            output.cluster = Some(u32::try_from(clusters.len())?);
                            clusters.push(cluster);
                        }
                        Err(error) => return Err(error.into()),
                    }
                    output.character = character[0];
                }
                cells.push(output);
            }
        }
        let cursor = if snapshot.cursor_visible()? && (presentation.cursor_on || !cursor_blinking) {
            snapshot.cursor_viewport()?.map(|viewport| {
                let x = viewport.x.saturating_sub(u16::from(viewport.at_wide_tail));
                (x, viewport.y)
            })
        } else {
            None
        };
        snapshot.clean()?;
        Ok(Some(Grid {
            generation,
            cols,
            cells,
            clusters,
            cursor,
            cursor_style,
            cursor_blinking,
            cursor_color: rgb(colors.cursor.unwrap_or(colors.foreground)),
            metrics: presentation.metrics,
            bg: rgb(colors.background),
        }))
    }
}

enum Msg {
    Key(KeyInput),
    Focus(bool),
    Pointer(PointerInput),
    Wheel(WheelInput),
    Copy,
    Paste { text: String, allow_unsafe: bool },
    Resize(Surface),
    Visible(bool),
}

#[derive(Default)]
struct PointerOutcome {
    bytes: Vec<u8>,
    selection_changed: bool,
}

/// Moves whole lines out of `remainder`, keeping the fraction for the next delta.
fn whole_lines(remainder: &mut f32, delta: f32) -> i32 {
    *remainder += delta;
    let lines = remainder.trunc();
    *remainder -= lines;
    lines as i32
}

/// False if libghostty refused the text because it could run commands.
fn paste_text(
    terminal: &mut Terminal<'static, '_>,
    text: &str,
    allow_unsafe: bool,
) -> Result<bool> {
    let mimes = [ClipboardMime::new(b"text/plain")];
    let options = paste::Options::new().with_allow_unsafe(allow_unsafe);
    match terminal.paste(options, &mimes, |_, out: &mut dyn Write| {
        out.write_all(text.as_bytes())
    }) {
        Ok(_) => {
            terminal.scroll_viewport(ScrollViewport::Bottom);
            Ok(true)
        }
        Err(libghostty_vt::Error::Rejected) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn selection_text(terminal: &Terminal<'static, '_>) -> Result<Option<String>> {
    let Some(mut formatter) = fmt::Formatter::new_selection(terminal)? else {
        return Ok(None);
    };
    formatter.set_format(fmt::Format::Plain)?.set_unwrap(true)?;
    let bytes = formatter.format_alloc(None)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Turns raw input into terminal bytes or selection changes. Lives with the `Terminal` on its actor.
struct TerminalInput {
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    autoscroll_tick: AutoscrollTickEvent<'static>,
    started: Instant,
    surface: Option<Surface>,
    selecting: bool,
    pressed: Option<mouse::Button>,
    position: (f32, f32),
    next_autoscroll: Option<Instant>,
    wheel_remainder: f32,
}

impl TerminalInput {
    fn new() -> Result<Self> {
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(MULTI_CLICK)?;
        Ok(Self {
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            gesture: Gesture::new()?,
            press,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            autoscroll_tick: AutoscrollTickEvent::new()?,
            started: Instant::now(),
            surface: None,
            selecting: false,
            pressed: None,
            position: (0.0, 0.0),
            next_autoscroll: None,
            wheel_remainder: 0.0,
        })
    }

    fn encode_key(&mut self, terminal: &Terminal<'static, '_>, input: KeyInput) -> Result<Vec<u8>> {
        self.key_event
            .set_action(key::Action::Press)
            .set_key(input.key)
            .set_mods(input.mods)
            .set_consumed_mods(input.consumed)
            .set_unshifted_codepoint(input.unshifted)
            .set_utf8(input.text);
        let mut encoded = Vec::new();
        self.key_encoder
            .set_options_from_terminal(terminal)
            .set_macos_option_as_alt(key::OptionAsAlt::True)
            .encode_to_vec(&self.key_event, &mut encoded)?;
        Ok(encoded)
    }

    fn key(&mut self, terminal: &mut Terminal<'static, '_>, input: KeyInput) -> Result<Vec<u8>> {
        let encoded = self.encode_key(terminal, input)?;
        if !encoded.is_empty() {
            terminal.scroll_viewport(ScrollViewport::Bottom);
        }
        Ok(encoded)
    }

    fn viewport_point(surface: Surface, (x, y): (f32, f32)) -> PointCoordinate {
        let Metrics {
            cell_width,
            cell_height,
            padding,
            ..
        } = surface.metrics;
        let cell = |position: f32, size: u16, count: u16| {
            ((position - padding) / f32::from(size)).clamp(0.0, f32::from(count.saturating_sub(1)))
        };
        PointCoordinate {
            x: cell(x, cell_width, surface.cols) as u16,
            y: cell(y, cell_height, surface.rows) as u32,
        }
    }

    fn geometry(surface: Surface) -> Geometry {
        let metrics = surface.metrics;
        let padding = metrics.padding.round() as u32;
        Geometry {
            columns: surface.cols.into(),
            cell_width: metrics.cell_width.into(),
            padding_left: padding,
            screen_height: u32::from(surface.rows) * u32::from(metrics.cell_height) + 2 * padding,
        }
    }

    fn report(
        &mut self,
        terminal: &Terminal<'static, '_>,
        surface: Surface,
        action: mouse::Action,
        button: Option<mouse::Button>,
        mods: key::Mods,
    ) -> Result<Vec<u8>> {
        let Geometry {
            columns,
            cell_width,
            padding_left: padding,
            screen_height,
            ..
        } = Self::geometry(surface);
        self.mouse_encoder
            .set_options_from_terminal(terminal)
            .set_size(mouse::EncoderSize {
                screen_width: columns * cell_width + 2 * padding,
                screen_height,
                cell_width,
                cell_height: surface.metrics.cell_height.into(),
                padding_top: padding,
                padding_bottom: padding,
                padding_right: padding,
                padding_left: padding,
            })?
            .set_any_button_pressed(self.pressed.is_some())
            .set_track_last_cell(true);
        self.mouse_event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(mouse::Position {
                x: self.position.0,
                y: self.position.1,
            });
        let mut encoded = Vec::new();
        self.mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut encoded)?;
        Ok(encoded)
    }

    fn schedule_autoscroll(
        &mut self,
        terminal: &Terminal<'static, '_>,
        now: Instant,
    ) -> Result<()> {
        self.next_autoscroll = match self.gesture.autoscroll(terminal)? {
            Autoscroll::None => None,
            _ => Some(now + AUTOSCROLL),
        };
        Ok(())
    }

    fn pointer(
        &mut self,
        terminal: &Terminal<'static, '_>,
        input: PointerInput,
    ) -> Result<PointerOutcome> {
        let Some(surface) = self.surface else {
            return Ok(PointerOutcome::default());
        };
        let scale = surface.metrics.scale;
        self.position = (input.x * scale, input.y * scale);
        let selection_changed = PointerOutcome {
            bytes: Vec::new(),
            selection_changed: true,
        };
        let grid_ref_point = Point::Viewport(Self::viewport_point(surface, self.position));
        let to_report = match input.phase {
            PointerPhase::Press => {
                let reporting =
                    terminal.is_mouse_tracking()? && !input.mods.contains(key::Mods::SHIFT);
                self.selecting = !reporting && input.button == Some(mouse::Button::Left);
                if self.selecting {
                    let grid_ref = terminal.grid_ref(grid_ref_point)?;
                    let selection = self
                        .press
                        .set_repeat_distance(surface.metrics.cell_width.into())?
                        .set_time(self.started.elapsed())?
                        .set_position(self.position.0.into(), self.position.1.into())?
                        .apply(&mut self.gesture, terminal, grid_ref)?;
                    terminal.set_selection(selection.as_ref())?;
                    return Ok(selection_changed);
                }
                if reporting {
                    self.pressed = input.button;
                    Some((mouse::Action::Press, input.button))
                } else {
                    None
                }
            }
            PointerPhase::Drag if self.selecting => {
                let grid_ref = terminal.grid_ref(grid_ref_point)?;
                let selection = self
                    .drag
                    .set_rectangle(input.mods.contains(key::Mods::ALT))?
                    .set_position(self.position.0.into(), self.position.1.into())?
                    .apply(
                        &mut self.gesture,
                        terminal,
                        grid_ref,
                        Self::geometry(surface),
                    )?;
                terminal.set_selection(selection.as_ref())?;
                self.schedule_autoscroll(terminal, Instant::now())?;
                return Ok(selection_changed);
            }
            PointerPhase::Drag if self.pressed.is_some() => {
                Some((mouse::Action::Motion, self.pressed))
            }
            PointerPhase::Release if self.selecting => {
                let grid_ref = terminal.grid_ref(grid_ref_point).ok();
                self.release.apply(&mut self.gesture, terminal, grid_ref)?;
                self.selecting = false;
                self.next_autoscroll = None;
                None
            }
            PointerPhase::Release if self.pressed.is_some() => {
                Some((mouse::Action::Release, self.pressed.take()))
            }
            PointerPhase::Move if terminal.is_mouse_tracking()? => {
                Some((mouse::Action::Motion, None))
            }
            _ => None,
        };
        let Some((action, button)) = to_report else {
            return Ok(PointerOutcome::default());
        };
        Ok(PointerOutcome {
            bytes: self.report(terminal, surface, action, button, input.mods)?,
            selection_changed: false,
        })
    }

    /// Scrolls one line while a selection drag is held past the edge. True if anything moved.
    fn tick_autoscroll(&mut self, terminal: &Terminal<'static, '_>, now: Instant) -> Result<bool> {
        let Some(surface) = self.surface else {
            return Ok(false);
        };
        if !self.next_autoscroll.is_some_and(|due| now >= due) {
            return Ok(false);
        }
        if matches!(self.gesture.autoscroll(terminal)?, Autoscroll::None) {
            self.next_autoscroll = None;
            return Ok(false);
        }
        let viewport = Self::viewport_point(surface, self.position);
        let selection = self.autoscroll_tick.apply(
            &mut self.gesture,
            terminal,
            viewport,
            Self::geometry(surface),
        )?;
        terminal.set_selection(selection.as_ref())?;
        self.schedule_autoscroll(terminal, now)?;
        Ok(true)
    }

    fn wheel(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        input: WheelInput,
    ) -> Result<Vec<u8>> {
        let Some(surface) = self.surface else {
            return Ok(Vec::new());
        };
        let line_height = f32::from(surface.metrics.cell_height) / surface.metrics.scale;
        let lines = whole_lines(&mut self.wheel_remainder, input.dy / line_height);
        let mut bytes = Vec::new();
        if lines == 0 {
            return Ok(bytes);
        }
        let toward_history = lines > 0;
        if terminal.is_mouse_tracking()? {
            self.position = (
                input.x * surface.metrics.scale,
                input.y * surface.metrics.scale,
            );
            let button = Some(if toward_history {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            });
            for _ in 0..lines.unsigned_abs() {
                for action in [mouse::Action::Press, mouse::Action::Release] {
                    bytes.extend(self.report(terminal, surface, action, button, input.mods)?);
                }
            }
        } else if terminal.active_screen()? == Screen::Alternate
            && terminal.mode(Mode::ALT_SCROLL)?
        {
            let arrow = if toward_history {
                key::Key::ArrowUp
            } else {
                key::Key::ArrowDown
            };
            for _ in 0..lines.unsigned_abs() {
                bytes.extend(self.encode_key(
                    terminal,
                    KeyInput {
                        key: arrow,
                        mods: key::Mods::empty(),
                        consumed: key::Mods::empty(),
                        text: None,
                        unshifted: '\0',
                    },
                )?);
            }
        } else {
            terminal.scroll_viewport(ScrollViewport::Delta(-(lines as isize)));
        }
        Ok(bytes)
    }
}

/// A process on a PTY. Its `Terminal` lives on an actor thread, as libghostty types are `!Send`.
pub struct Session {
    id: u64,
    commands: Option<Sender<Msg>>,
    grid: Arc<Mutex<Arc<Grid>>>,
    events: Receiver<Event>,
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

        let mut process = Command::new(LAUNCH_SHELL);
        process.args(["-l", "-i"]);
        if let Some(command) = command {
            process.arg("-c").arg(format!("exec {command}"));
        }
        process
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "agentd")
            .env("SHELL", LAUNCH_SHELL)
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
        let (events_tx, events) = unbounded();
        let mut actor = Actor {
            cols,
            rows,
            output: output_rx,
            commands: commands_rx,
            input: input_tx,
            child,
            master,
            slot: grid.clone(),
            events: events_tx,
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
            id: next_id(),
            commands: Some(commands),
            grid,
            events,
        })
    }

    /// A session with no process, showing `grid`. Input and resizes go nowhere.
    pub fn fixture(grid: Grid) -> Self {
        Self {
            id: next_id(),
            commands: None,
            grid: Arc::new(Mutex::new(Arc::new(grid))),
            events: unbounded().1,
        }
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
        self.send(Msg::Focus(focused));
    }

    pub fn pointer(&self, input: PointerInput) {
        self.send(Msg::Pointer(input));
    }

    pub fn wheel(&self, input: WheelInput) {
        self.send(Msg::Wheel(input));
    }

    pub fn copy(&self) {
        self.send(Msg::Copy);
    }

    pub fn paste(&self, text: String, allow_unsafe: bool) {
        self.send(Msg::Paste { text, allow_unsafe });
    }

    pub fn next_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn resize(&self, surface: Surface) {
        self.send(Msg::Resize(surface));
    }

    /// A hidden session keeps running but stops publishing grids and waking the UI.
    pub fn set_visible(&self, visible: bool) {
        self.send(Msg::Visible(visible));
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

/// Applies resizes at frame rate, so a live drag can't flood the program with SIGWINCH.
struct ResizeThrottle {
    pending: Option<Surface>,
    next: Instant,
}

impl ResizeThrottle {
    fn wait(&self, now: Instant) -> Option<Duration> {
        self.pending
            .map(|_| self.next.saturating_duration_since(now))
    }

    fn take_due(&mut self, now: Instant) -> Option<Surface> {
        if now < self.next {
            return None;
        }
        let surface = self.pending.take()?;
        self.next = now + FRAME;
        Some(surface)
    }
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
    events: Sender<Event>,
    waker: Waker,
}

impl Actor {
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
        let publisher = Rc::new(RefCell::new(Publisher {
            builder: GridBuilder::new()?,
            slot: self.slot.clone(),
            waker: self.waker.clone(),
            generation: 0,
            next_frame: Instant::now(),
            hold_started: None,
            presentation: Presentation::default(),
            next_blink: None,
            visible: true,
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
            if held && publisher.visible && Instant::now() >= publisher.next_frame {
                publisher.publish(terminal).log_err("publish held frame");
            }
        })?;

        let mut input = TerminalInput::new()?;
        let mut resize = ResizeThrottle {
            pending: None,
            next: Instant::now(),
        };
        let mut dirty = true;
        loop {
            let now = Instant::now();
            let wait = publisher.borrow().wait(dirty, now);
            let wait = input
                .next_autoscroll
                .map_or(wait, |due| wait.min(due.saturating_duration_since(now)));
            let wait = resize.wait(now).map_or(wait, |due| wait.min(due));
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
                        if let Some(outcome) = input.pointer(&terminal, pointer).log_err("handle pointer") {
                            if !outcome.bytes.is_empty() {
                                self.input.send(outcome.bytes).log_err("queue mouse report");
                            }
                            if outcome.selection_changed {
                                publisher.borrow_mut().builder.selection_changed();
                                dirty = true;
                            }
                        }
                    }
                    Ok(Msg::Wheel(wheel)) => {
                        if let Some(bytes) = input.wheel(&mut terminal, wheel).log_err("handle wheel") {
                            if !bytes.is_empty() {
                                self.input.send(bytes).log_err("queue wheel input");
                            }
                            dirty = true;
                        }
                    }
                    Ok(Msg::Copy) => {
                        if let Some(Some(text)) = selection_text(&terminal).log_err("copy selection") {
                            self.events.send(Event::Copied(text)).log_err("queue copied text");
                            (self.waker)();
                        }
                    }
                    Ok(Msg::Focus(focused)) => {
                        let mut publisher = publisher.borrow_mut();
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
                                self.events.send(Event::PasteRejected(text)).log_err("queue rejected paste");
                                (self.waker)();
                            }
                            None => {}
                        }
                    }
                    Ok(Msg::Resize(surface)) => {
                        resize.pending = Some(surface);
                        dirty = true;
                    }
                    Ok(Msg::Visible(visible)) => {
                        publisher.borrow_mut().visible = visible;
                        dirty = true;
                    }
                    Err(_) => return Ok(()),
                },
                default(wait) => {}
            }
            let mut publisher = publisher.borrow_mut();
            dirty |= publisher.tick_blink(Instant::now());
            if input
                .tick_autoscroll(&terminal, Instant::now())
                .log_err("autoscroll selection")
                == Some(true)
            {
                publisher.builder.selection_changed();
                dirty = true;
            }
            if let Some(surface) = resize.take_due(Instant::now()) {
                self.apply_resize(&mut terminal, surface);
                input.surface = Some(surface);
                publisher.presentation.metrics = surface.metrics;
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
            if dirty && !held && publisher.visible && Instant::now() >= publisher.next_frame {
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
    presentation: Presentation,
    next_blink: Option<Instant>,
    visible: bool,
}

impl Publisher {
    fn wait(&self, dirty: bool, now: Instant) -> Duration {
        let deadline = match self.hold_started {
            Some(started) => started + HOLD_TIMEOUT,
            None if dirty && self.visible => self.next_frame,
            None => now + Duration::from_secs(3600),
        };
        self.next_blink
            .filter(|_| self.visible)
            .map_or(deadline, |blink| deadline.min(blink))
            .saturating_duration_since(now)
    }

    fn tick_blink(&mut self, now: Instant) -> bool {
        if !self.visible || !self.next_blink.is_some_and(|blink| now >= blink) {
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
    fn a_wide_character_takes_a_wide_cell_and_a_spacer() {
        let grid = Grid::from_vt("a世b".as_bytes(), 8, 1).unwrap();
        assert_eq!(grid.lines()[0], "a世b    ");
        assert!(grid.cells[1].wide && !grid.cells[1].spacer);
        assert!(grid.cells[2].spacer && !grid.cells[2].wide);
        assert_eq!(grid.cells[3].character, 'b');
    }

    #[test]
    fn combining_marks_and_zwj_sequences_stay_in_one_cluster() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let text = format!("e\u{301}{family}x");
        let grid = Grid::from_vt(text.as_bytes(), 8, 1).unwrap();
        assert_eq!(grid.cluster(&grid.cells[0]), Some("e\u{301}"));
        assert_eq!(grid.cluster(&grid.cells[1]), Some(family));
        assert!(grid.cells[1].wide && grid.cells[2].spacer);
        assert_eq!(grid.cells[3].character, 'x');
        assert_eq!(grid.cluster(&grid.cells[3]), None);
    }

    #[test]
    fn an_emoji_variation_selector_widens_the_cell() {
        let grid = Grid::from_vt("\u{2764}\u{FE0F}x".as_bytes(), 8, 1).unwrap();
        assert!(grid.cells[0].wide);
        assert_eq!(grid.cells[2].character, 'x');
    }

    #[test]
    fn text_styles_reach_the_cell() {
        let grid = Grid::from_vt(b"\x1b[3;2;9;53mx", 4, 1).unwrap();
        let cell = grid.cells[0];
        assert!(cell.italic && cell.strikethrough && cell.overline);
        assert_eq!(cell.underline, Underline::None);
        assert_eq!(cell.fg, faint(rgb(DEFAULT_FG), rgb(DEFAULT_BG)));
    }

    #[test]
    fn faint_is_halfway_between_the_colour_and_the_background() {
        assert_eq!(faint([200, 100, 0], [0, 100, 200]), [100, 100, 100]);
    }

    #[test]
    fn underline_style_and_colour_reach_the_cell() {
        let grid = Grid::from_vt(b"\x1b[4:3;58;2;1;2;3mx\x1b[0m\x1b[4my", 4, 1).unwrap();
        assert_eq!(grid.cells[0].underline, Underline::Curly);
        assert_eq!(grid.cells[0].underline_color, Some([1, 2, 3]));
        assert_eq!(grid.cells[1].underline, Underline::Single);
        assert_eq!(grid.cells[1].underline_color, None);
    }

    #[test]
    fn invisible_text_has_no_glyph() {
        let grid = Grid::from_vt(b"\x1b[8mx", 4, 1).unwrap();
        assert_eq!(grid.lines()[0], "    ");
    }

    #[test]
    fn a_cursor_on_a_wide_tail_moves_to_the_wide_cell() {
        let grid = Grid::from_vt("世\x1b[1;2H".as_bytes(), 8, 1).unwrap();
        assert_eq!(grid.cursor, Some((0, 0)));
    }

    #[test]
    fn the_cursor_style_reaches_the_grid() {
        let grid = Grid::from_vt(b"\x1b[6 q", 8, 1).unwrap();
        assert_eq!(grid.cursor_style, CursorVisualStyle::Bar);
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
        let shown = Presentation::default();
        assert!(builder.build(&terminal, 1, shown).unwrap().is_some());
        assert!(builder.build(&terminal, 2, shown).unwrap().is_none());
        terminal.vt_write(b"\x1b[3;4H");
        assert_eq!(
            builder.build(&terminal, 3, shown).unwrap().unwrap().cursor,
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

    fn wait_until<T>(
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
            thread::sleep(Duration::from_millis(10));
        }
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
            visible: true,
        }
    }

    #[test]
    fn a_hidden_dirty_publisher_sleeps() {
        let now = Instant::now();
        let mut publisher = idle_publisher(now);
        publisher.next_blink = Some(now);
        publisher.visible = false;
        assert!(publisher.wait(true, now) > Duration::from_secs(60));
        assert!(!publisher.tick_blink(now));
    }

    const TEST_SURFACE: Surface = Surface {
        cols: 20,
        rows: 5,
        metrics: Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 0.0,
            scale: 1.0,
        },
    };

    fn input_for_test() -> TerminalInput {
        let mut input = TerminalInput::new().unwrap();
        input.surface = Some(TEST_SURFACE);
        input
    }

    fn pointer_at(phase: PointerPhase, column: u16, row: u16) -> PointerInput {
        PointerInput {
            phase,
            button: Some(mouse::Button::Left),
            x: f32::from(column) * 10.0 + 5.0,
            y: f32::from(row) * 20.0 + 10.0,
            mods: key::Mods::empty(),
        }
    }

    fn scrolled_terminal<'cb>() -> Terminal<'static, 'cb> {
        let mut terminal = new_terminal(20, 5).unwrap();
        for line in 0..30 {
            terminal.vt_write(format!("line{line}\r\n").as_bytes());
        }
        terminal
    }

    fn top_line(terminal: &Terminal<'static, '_>) -> String {
        let grid = GridBuilder::new()
            .unwrap()
            .build(terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        grid.lines()[0].trim_end().to_owned()
    }

    fn wheel_by(dy: f32) -> WheelInput {
        WheelInput {
            dy,
            x: 5.0,
            y: 10.0,
            mods: key::Mods::empty(),
        }
    }

    #[test]
    fn a_press_reports_to_a_tracking_program() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        let press = input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 2, 1))
            .unwrap();
        assert_eq!(press.bytes, b"\x1b[<0;3;2M");
        let release = input
            .pointer(&terminal, pointer_at(PointerPhase::Release, 2, 1))
            .unwrap();
        assert_eq!(release.bytes, b"\x1b[<0;3;2m");
    }

    #[test]
    fn shift_selects_instead_of_reporting() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        let mut press = pointer_at(PointerPhase::Press, 2, 1);
        press.mods = key::Mods::SHIFT;
        let outcome = input.pointer(&terminal, press).unwrap();
        assert!(outcome.bytes.is_empty() && outcome.selection_changed);
    }

    fn select_hello(terminal: &Terminal<'static, '_>, input: &mut TerminalInput) {
        input
            .pointer(terminal, pointer_at(PointerPhase::Press, 0, 0))
            .unwrap();
        // A cell only joins the selection once the pointer is most of the way across it.
        let mut drag = pointer_at(PointerPhase::Drag, 4, 0);
        drag.x = 49.0;
        input.pointer(terminal, drag).unwrap();
        input
            .pointer(terminal, pointer_at(PointerPhase::Release, 4, 0))
            .unwrap();
    }

    #[test]
    fn dragging_selects_text_that_can_be_copied() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        select_hello(&terminal, &mut input_for_test());
        assert_eq!(selection_text(&terminal).unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn with_no_selection_there_is_nothing_to_copy() {
        let terminal = new_terminal(20, 5).unwrap();
        assert_eq!(selection_text(&terminal).unwrap(), None);
    }

    #[test]
    fn selected_cells_swap_their_colours() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        let mut builder = GridBuilder::new().unwrap();
        builder
            .build(&terminal, 1, Presentation::default())
            .unwrap();
        select_hello(&terminal, &mut input_for_test());
        builder.selection_changed();
        let grid = builder
            .build(&terminal, 2, Presentation::default())
            .unwrap()
            .unwrap();
        assert_eq!(grid.cells[0].bg, Some(rgb(DEFAULT_FG)));
        assert_eq!(grid.cells[0].fg, rgb(DEFAULT_BG));
        assert_eq!(grid.cells[6].bg, None);
    }

    #[test]
    fn dragging_past_the_top_scrolls_one_line_per_tick() {
        let terminal = scrolled_terminal();
        let mut input = input_for_test();
        let before = top_line(&terminal);
        input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 0, 2))
            .unwrap();
        let mut drag = pointer_at(PointerPhase::Drag, 0, 0);
        drag.y = -30.0;
        input.pointer(&terminal, drag).unwrap();
        let due = input
            .next_autoscroll
            .expect("autoscroll should be scheduled");
        assert!(!input.tick_autoscroll(&terminal, due - AUTOSCROLL).unwrap());
        assert!(input.tick_autoscroll(&terminal, due).unwrap());
        let number = |line: &str| line.trim_start_matches("line").parse::<u32>().unwrap();
        assert_eq!(number(&before) - number(&top_line(&terminal)), 1);
    }

    #[test]
    fn wheel_deltas_keep_their_fraction_between_events() {
        let mut remainder = 0.0;
        assert_eq!(whole_lines(&mut remainder, 0.6), 0);
        assert_eq!(whole_lines(&mut remainder, 0.6), 1);
        assert_eq!(whole_lines(&mut remainder, -2.2), -2);
    }

    #[test]
    fn the_wheel_scrolls_scrollback_on_the_primary_screen() {
        let mut terminal = scrolled_terminal();
        let before = top_line(&terminal);
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(60.0))
            .unwrap();
        assert!(bytes.is_empty());
        assert_ne!(top_line(&terminal), before);
    }

    #[test]
    fn the_wheel_sends_arrows_on_the_alternate_screen() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1049h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(40.0))
            .unwrap();
        assert_eq!(bytes, b"\x1b[A\x1b[A");
    }

    #[test]
    fn the_wheel_reports_buttons_to_a_tracking_program() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(20.0))
            .unwrap();
        assert!(bytes.starts_with(b"\x1b[<64;1;1M"), "{bytes:?}");
    }

    #[test]
    fn typing_returns_the_viewport_to_the_bottom() {
        let mut terminal = scrolled_terminal();
        let bottom = top_line(&terminal);
        let mut input = input_for_test();
        input.wheel(&mut terminal, wheel_by(60.0)).unwrap();
        assert_ne!(top_line(&terminal), bottom);
        let typed = crate::input::map_text("a", rune::Mods::default(), None, None);
        input.key(&mut terminal, typed).unwrap();
        assert_eq!(top_line(&terminal), bottom);
    }

    fn terminal_capturing_replies<'cb>() -> (Terminal<'static, 'cb>, Rc<RefCell<Vec<u8>>>) {
        let mut terminal = new_terminal(20, 5).unwrap();
        let written = Rc::new(RefCell::new(Vec::new()));
        let sink = written.clone();
        terminal
            .on_pty_write(move |_, data| sink.borrow_mut().extend_from_slice(data))
            .unwrap();
        (terminal, written)
    }

    #[test]
    fn a_bracketed_paste_is_framed_and_allowed_to_hold_newlines() {
        let (mut terminal, written) = terminal_capturing_replies();
        terminal.vt_write(b"\x1b[?2004h");
        assert!(paste_text(&mut terminal, "a\nb", false).unwrap());
        assert_eq!(written.borrow().as_slice(), b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn an_unbracketed_newline_is_refused_until_allowed() {
        let (mut terminal, written) = terminal_capturing_replies();
        assert!(!paste_text(&mut terminal, "rm -rf x\n", false).unwrap());
        assert!(written.borrow().is_empty());
        assert!(paste_text(&mut terminal, "rm -rf x\n", true).unwrap());
        assert_eq!(written.borrow().as_slice(), b"rm -rf x\r");
    }

    #[test]
    fn option_b_is_escape_b() {
        let mut terminal = new_terminal(20, 5).unwrap();
        let option = rune::Mods {
            alt: true,
            ..rune::Mods::default()
        };
        let typed =
            crate::input::map_text("∫", option, Some(rune::PhysicalKeyCode::KeyB), Some('b'));
        assert_eq!(
            input_for_test().key(&mut terminal, typed).unwrap(),
            b"\x1bb"
        );
    }

    #[test]
    fn a_paste_reaches_a_real_shell() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        session.paste("echo pasted-ok".to_owned(), false);
        type_line(&session, "");
        wait_for_line(&session, "pasted-ok");
    }

    #[test]
    fn a_refused_paste_comes_back_to_the_ui() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
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
        let outcome = input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 0, 0))
            .unwrap();
        assert!(outcome.bytes.is_empty() && !outcome.selection_changed);
    }

    #[test]
    fn a_burst_of_resizes_ends_at_the_last_size() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
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
    fn a_second_resize_waits_a_frame_even_when_nothing_publishes() {
        let now = Instant::now();
        let mut throttle = ResizeThrottle {
            pending: Some(TEST_SURFACE),
            next: now,
        };
        assert_eq!(throttle.take_due(now), Some(TEST_SURFACE));
        throttle.pending = Some(TEST_SURFACE);
        assert_eq!(throttle.take_due(now), None);
        assert_eq!(throttle.wait(now), Some(FRAME));
        assert_eq!(throttle.take_due(now + FRAME), Some(TEST_SURFACE));
    }

    #[test]
    fn a_held_frame_sleeps_until_the_hold_deadline() {
        let now = Instant::now();
        let publisher = Publisher {
            hold_started: Some(now),
            ..idle_publisher(now)
        };
        assert_eq!(publisher.wait(true, now), HOLD_TIMEOUT);
    }

    #[test]
    fn an_idle_actor_wakes_for_the_next_blink() {
        let now = Instant::now();
        let publisher = Publisher {
            next_blink: Some(now + BLINK),
            ..idle_publisher(now)
        };
        assert_eq!(publisher.wait(false, now), BLINK);
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
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&session, "printf '\\033[?2026h'; echo held-2; sleep 30");
        wait_for_line(&session, "held-2");
    }

    #[test]
    fn a_hidden_session_publishes_once_shown() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        session.set_visible(false);
        type_line(&session, "echo hidden-ok");
        thread::sleep(Duration::from_secs(1));
        let lines = session.grid().lines();
        assert!(!lines.iter().any(|line| line.trim_end() == "hidden-ok"));
        session.set_visible(true);
        wait_for_line(&session, "hidden-ok");
    }

    #[test]
    fn a_hidden_flood_leaves_another_session_responsive() {
        let flood = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        flood.set_visible(false);
        type_line(&flood, "yes");
        thread::sleep(Duration::from_millis(500));
        let generation = flood.grid().generation;
        let typing = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&typing, "echo still-responsive");
        wait_for_line(&typing, "still-responsive");
        assert_eq!(flood.grid().generation, generation);
    }

    #[test]
    fn sessions_have_distinct_ids() {
        let first = Session::fixture(Grid::default());
        let second = Session::fixture(Grid::default());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn child_programs_see_the_launch_shell() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&session, "echo $SHELL");
        wait_for_line(&session, LAUNCH_SHELL);
    }

    #[test]
    fn a_real_process_reaches_the_grid_and_takes_keys() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
        type_line(&session, "echo agentd-42");
        wait_for_line(&session, "agentd-42");
    }

    #[test]
    fn ctrl_c_interrupts_the_foreground_process() {
        let session = Session::spawn(None, Path::new("/tmp"), Arc::new(|| {})).unwrap();
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
