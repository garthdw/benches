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

use crate::config::{Config, Theme};
use crate::input::KeyInput;

const FRAME: Duration = Duration::from_millis(8);
/// How long the size must hold before the program sees it, as each SIGWINCH makes inline TUIs reprint.
const RESIZE_SETTLE: Duration = Duration::from_millis(100);
const HOLD_TIMEOUT: Duration = Duration::from_secs(1);
const BLINK: Duration = Duration::from_millis(530);
const AUTOSCROLL: Duration = Duration::from_millis(50);
const MULTI_CLICK: Duration = Duration::from_millis(500);
const SCROLLBAR_HOLD: Duration = Duration::from_secs(1);
const SCROLLBAR_FADE: Duration = Duration::from_millis(200);
const MESSAGE_COLS: u16 = 80;
pub const DEFAULT_FG: RgbColor = RgbColor {
    r: 0xd8,
    g: 0xd9,
    b: 0xda,
};
pub const DEFAULT_BG: RgbColor = RgbColor {
    r: 0x14,
    g: 0x15,
    b: 0x17,
};

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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    #[default]
    None,
    /// A bell or desktop notification (OSC 9 or 777).
    Alert,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub cell_width: u16,
    pub cell_height: u16,
    pub padding: f32,
    pub scale: f32,
}

impl Metrics {
    /// The first cell's top-left in pixels, whole so every cell edge lands on a pixel.
    pub fn origin(self, rect: [f32; 4]) -> (f32, f32) {
        (
            (rect[0] + self.padding).round(),
            (rect[1] + self.padding).round(),
        )
    }

    pub fn cell_rect(self, origin: (f32, f32), col: u16, row: u16) -> [f32; 4] {
        let (width, height) = (f32::from(self.cell_width), f32::from(self.cell_height));
        [
            origin.0 + f32::from(col) * width,
            origin.1 + f32::from(row) * height,
            width,
            height,
        ]
    }

    pub fn to_points(self, rect: [f32; 4]) -> [f32; 4] {
        rect.map(|pixels| pixels / self.scale)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub cols: u16,
    pub rows: u16,
    pub metrics: Metrics,
}

impl Surface {
    /// The whole cells that fit a pixel `size` inside the padding, at least one each way.
    pub fn fit((width, height): (f32, f32), metrics: Metrics) -> Self {
        let fit = |extent: f32, cell: u16| {
            (((extent - 2.0 * metrics.padding) / f32::from(cell)) as u16).max(1)
        };
        Self {
            cols: fit(width, metrics.cell_width),
            rows: fit(height, metrics.cell_height),
            metrics,
        }
    }
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
    pub cells: Arc<[Cell]>,
    pub clusters: Arc<[String]>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_style: CursorVisualStyle,
    pub cursor_blinking: bool,
    pub cursor_color: [u8; 3],
    pub metrics: Metrics,
    pub bg: [u8; 3],
    pub scrollbar: Option<Scrollbar>,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            generation: 0,
            cols: 0,
            cells: Arc::default(),
            clusters: Arc::default(),
            cursor: None,
            cursor_style: CursorVisualStyle::Block,
            cursor_blinking: false,
            cursor_color: [0; 3],
            metrics: Metrics::default(),
            bg: rgb(DEFAULT_BG),
            scrollbar: None,
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
            cells: cells.into(),
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

/// The thumb never shrinks below this, in points, so it stays easy to grab.
const MIN_THUMB_POINTS: f32 = 24.0;

/// Where the viewport sits in the scrollback, in rows, and how the thumb looks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scrollbar {
    total: u64,
    offset: u64,
    len: u64,
    /// Zero while hidden; the thumb fades out after scrolling stops.
    pub opacity: f32,
    /// While hovered or dragged.
    pub wide: bool,
}

impl Scrollbar {
    /// `None` when there is nothing to scroll, including on the alternate screen, which has no scrollback.
    fn of(terminal: &Terminal<'static, '_>) -> Result<Option<Self>> {
        if terminal.active_screen()? == Screen::Alternate {
            return Ok(None);
        }
        let bar = terminal.scrollbar()?;
        Ok((bar.total > bar.len).then_some(Self {
            total: bar.total,
            offset: bar.offset,
            len: bar.len,
            ..Self::default()
        }))
    }

    /// The track's length and the thumb's minimum, in pixels, beside `rows` rows; for `thumb` and `row_at`.
    pub fn track(rows: usize, cell_height: f32, scale: f32) -> (f32, f32) {
        (rows as f32 * cell_height, MIN_THUMB_POINTS * scale)
    }

    /// The thumb's top and height within a track `track` pixels tall.
    pub fn thumb(self, track: f32, min: f32) -> (f32, f32) {
        let height = (track * self.len as f32 / self.total as f32)
            .max(min)
            .min(track);
        let travel = self.total.saturating_sub(self.len).max(1);
        let top = (track - height) * self.offset.min(travel) as f32 / travel as f32;
        (top, height)
    }

    /// The scrollback row that puts the thumb's top at `top`.
    fn row_at(self, top: f32, track: f32, min: f32) -> usize {
        let (_, height) = self.thumb(track, min);
        let travel = track - height;
        if travel <= 0.0 {
            return 0;
        }
        let rows = self.total.saturating_sub(self.len) as f32;
        ((top / travel).clamp(0.0, 1.0) * rows).round() as usize
    }
}

/// Shows the scrollbar while the viewport scrolls or the pointer holds it, then fades it out.
#[derive(Default)]
struct ScrollbarFade {
    until: Option<Instant>,
    held: bool,
    /// The viewport offset that output or a resize last left, so only other moves count as scrolls.
    offset: Option<u64>,
}

impl ScrollbarFade {
    fn show(&mut self, now: Instant) {
        self.until = Some(now + SCROLLBAR_HOLD);
    }

    fn follow(&mut self, terminal: &Terminal<'static, '_>) {
        self.offset = terminal
            .scrollbar()
            .log_err("read scrollbar")
            .map(|bar| bar.offset);
    }

    /// The thumb's opacity at `now`, showing it if the viewport moved since the last call or
    /// `follow`, or the pointer holds it or just let it go.
    fn opacity(&mut self, terminal: &Terminal<'static, '_>, held: bool, now: Instant) -> f32 {
        let offset = terminal
            .scrollbar()
            .log_err("read scrollbar")
            .map(|bar| bar.offset);
        let scrolled = offset != self.offset;
        self.offset = offset;
        if scrolled || held || self.held {
            self.show(now);
        }
        self.held = held;
        let Some(until) = self.until else {
            return 0.0;
        };
        let fading = now.saturating_duration_since(until).as_secs_f32();
        (1.0 - fading / SCROLLBAR_FADE.as_secs_f32()).max(0.0)
    }

    /// When the opacity next changes by itself.
    fn next_change(&self, now: Instant) -> Option<Instant> {
        let until = self.until.filter(|_| !self.held)?;
        if now < until {
            Some(until)
        } else {
            (now < until + SCROLLBAR_FADE).then_some(now + FRAME)
        }
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

fn apply_theme(terminal: &mut Terminal<'static, '_>, theme: &Theme) -> Result<()> {
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
    scrollbar_opacity: f32,
    scrollbar_wide: bool,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            focused: true,
            cursor_on: true,
            metrics: Metrics::default(),
            scrollbar_opacity: 0.0,
            scrollbar_wide: false,
        }
    }
}

type Content = (Arc<[Cell]>, Arc<[String]>);

struct GridBuilder {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    last_shown: Option<(CursorVisualStyle, bool, Presentation, Option<Scrollbar>)>,
    /// The cells and clusters of the last build, for frames where only the presentation changed.
    content: Option<Content>,
    /// From the theme; without a background, selected cells swap their colours.
    selection_background: Option<RgbColor>,
    selection_foreground: Option<RgbColor>,
}

impl GridBuilder {
    fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            last_shown: None,
            content: None,
            selection_background: None,
            selection_foreground: None,
        })
    }

    /// libghostty isn't known to mark the render state dirty when only the selection moves.
    fn selection_changed(&mut self) {
        self.last_shown = None;
        self.content = None;
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
        let scrollbar = Scrollbar::of(terminal)?.map(|scrollbar| Scrollbar {
            opacity: presentation.scrollbar_opacity,
            wide: presentation.scrollbar_wide,
            ..scrollbar
        });
        let shown = (cursor_style, cursor_blinking, presentation, scrollbar);
        // libghostty leaves the render state clean when only the cursor's style or the scrollback length changes.
        let unchanged = self.last_shown.replace(shown) == Some(shown);
        if matches!(snapshot.dirty()?, Dirty::Clean) && unchanged {
            return Ok(None);
        }
        let colors = snapshot.colors()?;
        let (cols, rows) = (snapshot.cols()?, snapshot.rows()?);
        let dirty = snapshot.dirty()?;
        let reusable = if matches!(dirty, Dirty::Clean) {
            self.content.clone()
        } else {
            None
        };
        let (cells, clusters) = match reusable {
            Some(content) => content,
            None => {
                let columns = usize::from(cols);
                let previous = self.content.clone().filter(|(cells, _)| {
                    matches!(dirty, Dirty::Partial) && cells.len() == columns * usize::from(rows)
                });
                let mut cells = Vec::with_capacity(columns * usize::from(rows));
                let mut clusters = Vec::new();
                let mut row_iterator = self.rows.update(&snapshot)?;
                while let Some(row) = row_iterator.next() {
                    if let Some((old_cells, old_clusters)) = &previous
                        && !row.dirty()?
                        && let Some(old_row) = old_cells.get(cells.len()..cells.len() + columns)
                    {
                        for old in old_row {
                            let mut cell = *old;
                            if let Some(index) = cell.cluster {
                                cell.cluster = Some(u32::try_from(clusters.len())?);
                                clusters.push(
                                    old_clusters
                                        .get(usize::try_from(index)?)
                                        .context("a copied cell's cluster should exist")?
                                        .clone(),
                                );
                            }
                            cells.push(cell);
                        }
                        continue;
                    }
                    let mut cell_iterator = self.cells.update(row)?;
                    while let Some(cell) = cell_iterator.next() {
                        let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
                        let mut bg = cell.bg_color()?;
                        let mut output = Cell::default();
                        let mut invisible = false;
                        let mut is_faint = false;
                        let selected = cell.is_selected()?;
                        let mut inverted = selected && self.selection_background.is_none();
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
                        if selected && let Some(background) = self.selection_background {
                            bg = Some(background);
                            fg = self.selection_foreground.unwrap_or(fg);
                        }
                        let width = cell.raw_cell()?.wide()?;
                        output.wide = width == CellWide::Wide;
                        output.spacer =
                            matches!(width, CellWide::SpacerTail | CellWide::SpacerHead);
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
                let content: Content = (cells.into(), clusters.into());
                self.content = Some(content.clone());
                content
            }
        };
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
            scrollbar,
        }))
    }
}

enum Msg {
    Key(KeyInput),
    Focus(bool),
    Pointer(PointerInput),
    Wheel(WheelInput),
    Scrollbar { phase: PointerPhase, y: f32 },
    ScrollbarHover(bool),
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
    /// How far below the thumb's top the pointer holds it, while dragging the scrollbar.
    scrollbar_grab: Option<f32>,
    /// Set by `select` for the actor to take, as libghostty doesn't mark the render state dirty.
    selection_changed: bool,
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
            scrollbar_grab: None,
            selection_changed: false,
        })
    }

    /// Ends any press, drag or selection, whose release would go to whichever session is shown next.
    /// Returns the release report for a button the program saw pressed, so it isn't left held.
    fn cancel_pointer(&mut self, terminal: &Terminal<'static, '_>) -> Result<Vec<u8>> {
        self.selecting = false;
        self.next_autoscroll = None;
        self.scrollbar_grab = None;
        match (self.pressed.take(), self.surface) {
            (Some(button), Some(surface)) => self.report(
                terminal,
                surface,
                mouse::Action::Release,
                Some(button),
                key::Mods::empty(),
            ),
            _ => Ok(Vec::new()),
        }
    }

    fn select(
        &mut self,
        terminal: &Terminal<'static, '_>,
        selection: Option<&libghostty_vt::selection::Selection<'_>>,
    ) -> Result<()> {
        terminal.set_selection(selection)?;
        self.selection_changed = true;
        Ok(())
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
        let metrics = surface.metrics;
        let origin = metrics.origin([0.0; 4]);
        let cell = |position: f32, origin: f32, size: u16, count: u16| {
            ((position - origin) / f32::from(size)).clamp(0.0, f32::from(count.saturating_sub(1)))
        };
        PointCoordinate {
            x: cell(x, origin.0, metrics.cell_width, surface.cols) as u16,
            y: cell(y, origin.1, metrics.cell_height, surface.rows) as u32,
        }
    }

    fn geometry(surface: Surface) -> Geometry {
        let metrics = surface.metrics;
        let padding = metrics.origin([0.0; 4]).0 as u32;
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

    /// Drags the thumb, or on a press off it, centres the thumb under the pointer as macOS does.
    /// `y` is in points from the top of the terminal surface.
    fn drag_scrollbar(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        phase: PointerPhase,
        y: f32,
    ) -> Result<()> {
        if phase == PointerPhase::Release {
            self.scrollbar_grab = None;
        }
        let Some(surface) = self.surface else {
            return Ok(());
        };
        if !matches!(phase, PointerPhase::Press | PointerPhase::Drag) {
            return Ok(());
        }
        let Some(scrollbar) = Scrollbar::of(terminal)? else {
            return Ok(());
        };
        let metrics = surface.metrics;
        let (track, min) = Scrollbar::track(
            surface.rows.into(),
            metrics.cell_height.into(),
            metrics.scale,
        );
        let y = y * metrics.scale - metrics.origin([0.0; 4]).1;
        let grab = match (phase, self.scrollbar_grab) {
            (PointerPhase::Drag, Some(grab)) => grab,
            (PointerPhase::Drag, None) => return Ok(()),
            _ => {
                let (top, height) = scrollbar.thumb(track, min);
                if (top..top + height).contains(&y) {
                    y - top
                } else {
                    height / 2.0
                }
            }
        };
        self.scrollbar_grab = Some(grab);
        terminal.scroll_viewport(ScrollViewport::Row(scrollbar.row_at(y - grab, track, min)));
        Ok(())
    }

    fn pointer(
        &mut self,
        terminal: &Terminal<'static, '_>,
        input: PointerInput,
    ) -> Result<Vec<u8>> {
        let Some(surface) = self.surface else {
            return Ok(Vec::new());
        };
        let scale = surface.metrics.scale;
        self.position = (input.x * scale, input.y * scale);
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
                    self.select(terminal, selection.as_ref())?;
                    return Ok(Vec::new());
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
                self.select(terminal, selection.as_ref())?;
                self.schedule_autoscroll(terminal, Instant::now())?;
                return Ok(Vec::new());
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
            // Rune also sends hover moves during a drag, which would read as the button lifting.
            PointerPhase::Move
                if self.pressed.is_none() && !self.selecting && terminal.is_mouse_tracking()? =>
            {
                Some((mouse::Action::Motion, None))
            }
            _ => None,
        };
        let Some((action, button)) = to_report else {
            return Ok(Vec::new());
        };
        self.report(terminal, surface, action, button, input.mods)
    }

    /// Scrolls one line while a selection drag is held past the edge.
    fn tick_autoscroll(&mut self, terminal: &Terminal<'static, '_>, now: Instant) -> Result<()> {
        let Some(surface) = self.surface else {
            return Ok(());
        };
        if !self.next_autoscroll.is_some_and(|due| now >= due) {
            return Ok(());
        }
        if matches!(self.gesture.autoscroll(terminal)?, Autoscroll::None) {
            self.next_autoscroll = None;
            return Ok(());
        }
        let viewport = Self::viewport_point(surface, self.position);
        let selection = self.autoscroll_tick.apply(
            &mut self.gesture,
            terminal,
            viewport,
            Self::geometry(surface),
        )?;
        self.select(terminal, selection.as_ref())?;
        self.schedule_autoscroll(terminal, now)
    }

    /// Bytes for the program, or `None` when the wheel scrolled the viewport.
    fn wheel(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        input: WheelInput,
    ) -> Result<Option<Vec<u8>>> {
        let Some(surface) = self.surface else {
            return Ok(Some(Vec::new()));
        };
        let line_height = f32::from(surface.metrics.cell_height) / surface.metrics.scale;
        let lines = whole_lines(&mut self.wheel_remainder, input.dy / line_height);
        let mut bytes = Vec::new();
        if lines == 0 {
            return Ok(Some(bytes));
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
            return Ok(None);
        }
        Ok(Some(bytes))
    }
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
    events: Sender<Event>,
    waker: Waker,
    theme: Theme,
}

impl Actor {
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
                        if let Some(bytes) = input.pointer(&terminal, pointer).log_err("handle pointer")
                            && !bytes.is_empty()
                        {
                            self.input.send(bytes).log_err("queue mouse report");
                        }
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

    fn fractional_metrics(scale: f32) -> Metrics {
        Metrics {
            cell_width: 9,
            cell_height: 19,
            padding: 12.0 * scale,
            scale,
        }
    }

    #[test]
    fn hit_testing_finds_the_cell_drawn_at_any_scale() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let metrics = fractional_metrics(scale);
            let surface = Surface::fit((700.0, 500.0), metrics);
            let origin = metrics.origin([0.0; 4]);
            for (col, row) in [(0, 0), (1, 3), (surface.cols - 1, surface.rows - 1)] {
                let [left, top, width, height] = metrics.cell_rect(origin, col, row);
                for (x, y) in [(left, top), (left + width / 2.0, top + height / 2.0)] {
                    let point = TerminalInput::viewport_point(surface, (x, y));
                    assert_eq!((point.x, u16::try_from(point.y)), (col, Ok(row)), "{scale}");
                }
                let just_before = TerminalInput::viewport_point(surface, (left - 0.01, top - 0.01));
                assert_eq!(just_before.x, col.saturating_sub(1), "{scale}");
            }
        }
    }

    #[test]
    fn the_cell_origin_is_whole_pixels_within_the_rect() {
        let metrics = fractional_metrics(1.5);
        assert_eq!(metrics.origin([0.0; 4]), (18.0, 18.0));
        assert_eq!(metrics.origin([10.4, 20.6, 0.0, 0.0]), (28.0, 39.0));
    }

    #[test]
    fn fit_keeps_whole_cells_inside_the_padding_and_at_least_one() {
        let metrics = fractional_metrics(2.0);
        let surface = Surface::fit((48.0 + 9.0 * 4.0 + 8.0, 48.0 + 19.0 * 2.0 + 5.0), metrics);
        assert_eq!((surface.cols, surface.rows), (4, 2));
        let tiny = Surface::fit((1.0, 1.0), metrics);
        assert_eq!((tiny.cols, tiny.rows), (1, 1));
    }

    #[test]
    fn rects_convert_to_points_by_the_scale() {
        let metrics = fractional_metrics(2.0);
        assert_eq!(
            metrics.to_points([10.0, 20.0, 30.0, 40.0]),
            [5.0, 10.0, 15.0, 20.0]
        );
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
        assert_eq!(press, b"\x1b[<0;3;2M");
        let release = input
            .pointer(&terminal, pointer_at(PointerPhase::Release, 2, 1))
            .unwrap();
        assert_eq!(release, b"\x1b[<0;3;2m");
    }

    #[test]
    fn cancelling_a_held_press_reports_its_release() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 2, 1))
            .unwrap();
        assert_eq!(input.cancel_pointer(&terminal).unwrap(), b"\x1b[<0;3;2m");
        assert_eq!(input.cancel_pointer(&terminal).unwrap(), b"");
    }

    /// Rune reports a hover move alongside every drag move, and `main.rs` forwards both.
    fn report_drag(
        terminal: &Terminal<'static, '_>,
        input: &mut TerminalInput,
        mods: key::Mods,
    ) -> Vec<u8> {
        let events = [
            (PointerPhase::Press, 2),
            (PointerPhase::Move, 3),
            (PointerPhase::Drag, 3),
            (PointerPhase::Move, 3),
            (PointerPhase::Release, 3),
        ];
        let mut bytes = Vec::new();
        for (phase, column) in events {
            let mut pointer = pointer_at(phase, column, 1);
            pointer.mods = mods;
            if phase == PointerPhase::Move {
                pointer.button = None;
            }
            bytes.extend(input.pointer(terminal, pointer).unwrap());
        }
        bytes
    }

    #[test]
    fn a_drag_reports_no_buttonless_motion_to_any_motion_tracking() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");
        let bytes = report_drag(&terminal, &mut input_for_test(), key::Mods::empty());
        assert_eq!(bytes, b"\x1b[<0;3;2M\x1b[<32;4;2M\x1b[<0;4;2m");
    }

    #[test]
    fn a_shift_drag_selects_without_reporting_motion() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");
        let bytes = report_drag(&terminal, &mut input_for_test(), key::Mods::SHIFT);
        assert_eq!(bytes, b"");
    }

    #[test]
    fn shift_selects_instead_of_reporting() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        let mut press = pointer_at(PointerPhase::Press, 2, 1);
        press.mods = key::Mods::SHIFT;
        let bytes = input.pointer(&terminal, press).unwrap();
        assert!(bytes.is_empty() && input.selection_changed);
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

    fn busy_terminal() -> Terminal<'static, 'static> {
        let mut terminal = new_terminal(200, 60).unwrap();
        for row in 0..80 {
            terminal.vt_write(
                format!(
                    "\x1b[3{}mline {row} the quick brown fox \x1b[1;4{}mjumps\x1b[0m over 世界 the lazy dog é\r\n",
                    row % 8,
                    row % 7
                )
                .repeat(1)
                .as_bytes(),
            );
        }
        terminal
    }

    /// Times only the build; `prepare` writes to the terminal and picks the presentation.
    fn per_frame(
        label: &str,
        terminal: &mut Terminal<'static, 'static>,
        builder: &mut GridBuilder,
        mut prepare: impl FnMut(&mut Terminal<'static, 'static>, u32) -> Presentation,
    ) {
        let frames = 300;
        let mut elapsed = std::time::Duration::ZERO;
        for frame in 0..frames {
            let presentation = prepare(terminal, frame);
            let start = std::time::Instant::now();
            builder
                .build(terminal, u64::from(frame), presentation)
                .unwrap();
            elapsed += start.elapsed();
        }
        println!(
            "{label}: {} ns/frame",
            elapsed.as_nanos() / u128::from(frames)
        );
    }

    #[test]
    #[ignore = "benchmark: cargo test --release bench_ -- --ignored --nocapture"]
    fn bench_grid_build() {
        let mut terminal = busy_terminal();
        let mut builder = GridBuilder::new().unwrap();
        builder
            .build(&terminal, 1, Presentation::default())
            .unwrap();
        per_frame(
            "steady output (scrolling)",
            &mut terminal,
            &mut builder,
            |terminal, _| {
                terminal.vt_write(b"\x1b[32msteady output line with some text 12345\x1b[0m\r\n");
                Presentation::default()
            },
        );
        per_frame(
            "one dirty row",
            &mut terminal,
            &mut builder,
            |terminal, frame| {
                terminal.vt_write(format!("\x1b[30;1H\x1b[2Kspinner {frame}").as_bytes());
                Presentation::default()
            },
        );
        per_frame("cursor blink", &mut terminal, &mut builder, |_, frame| {
            Presentation {
                cursor_on: frame % 2 == 0,
                ..Presentation::default()
            }
        });
        per_frame("scrollbar fade", &mut terminal, &mut builder, |_, frame| {
            Presentation {
                scrollbar_opacity: frame as f32 / 300.0,
                ..Presentation::default()
            }
        });
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
    fn a_presentation_only_change_shares_cells_and_moves_the_cursor_state() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello \x1b[?12h");
        let mut builder = GridBuilder::new().unwrap();
        let first = builder
            .build(&terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        let blinked_off = Presentation {
            cursor_on: false,
            scrollbar_opacity: 0.5,
            ..Presentation::default()
        };
        let second = builder.build(&terminal, 2, blinked_off).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first.cells, &second.cells));
        assert_eq!(first.cursor, Some((6, 0)));
        assert_eq!(second.cursor, None);
        builder.selection_changed();
        let third = builder.build(&terminal, 3, blinked_off).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&first.cells, &third.cells));
    }

    #[test]
    fn a_partial_update_copies_clean_rows_and_rereads_dirty_ones() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write("e\u{301}x\r\nsecond\r\n\x1b[4;1H\u{1f468}\u{200d}\u{1f469}".as_bytes());
        let mut builder = GridBuilder::new().unwrap();
        let first = builder
            .build(&terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        terminal.vt_write(b"\x1b[2;1Hchanged\x1b[5;1Hlast");
        let second = builder
            .build(&terminal, 2, Presentation::default())
            .unwrap()
            .unwrap();
        let lines = second.lines();
        assert_eq!(lines[0], first.lines()[0]);
        assert_eq!(lines[1].trim_end(), "changed");
        assert_eq!(lines[3], first.lines()[3]);
        assert_eq!(lines[4].trim_end(), "last");
        assert_eq!(second.cluster(&second.cells[0]), Some("e\u{301}"));
    }

    const THEME_RED: RgbColor = RgbColor {
        r: 0xff,
        g: 0,
        b: 0,
    };
    const THEME_BLUE: RgbColor = RgbColor {
        r: 0,
        g: 0,
        b: 0xff,
    };

    #[test]
    fn a_theme_sets_the_default_colours_and_palette() {
        let theme = Theme {
            foreground: THEME_RED,
            background: THEME_BLUE,
            palette: vec![(1, THEME_BLUE)],
            ..Theme::default()
        };
        let mut terminal = new_terminal(10, 2).unwrap();
        apply_theme(&mut terminal, &theme).unwrap();
        terminal.vt_write(b"a\x1b[31mb");
        let grid = GridBuilder::new()
            .unwrap()
            .build(&terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        assert_eq!(grid.bg, rgb(THEME_BLUE));
        assert_eq!(grid.cells[0].fg, rgb(THEME_RED));
        assert_eq!(grid.cells[1].fg, rgb(THEME_BLUE));
    }

    #[test]
    fn theme_selection_colours_replace_the_swap() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        let mut builder = GridBuilder::new().unwrap();
        builder.selection_background = Some(THEME_RED);
        builder.selection_foreground = Some(THEME_BLUE);
        select_hello(&terminal, &mut input_for_test());
        let grid = builder
            .build(&terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        assert_eq!(grid.cells[0].bg, Some(rgb(THEME_RED)));
        assert_eq!(grid.cells[0].fg, rgb(THEME_BLUE));
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
        input.selection_changed = false;
        input.tick_autoscroll(&terminal, due - AUTOSCROLL).unwrap();
        assert!(!input.selection_changed);
        input.tick_autoscroll(&terminal, due).unwrap();
        assert!(input.selection_changed);
        let number = |line: &str| line.trim_start_matches("line").parse::<u32>().unwrap();
        assert_eq!(number(&before) - number(&top_line(&terminal)), 1);
    }

    #[test]
    fn the_thumb_shows_the_viewport_share_and_position() {
        let scrollbar = Scrollbar {
            total: 100,
            offset: 75,
            len: 25,
            ..Scrollbar::default()
        };
        assert_eq!(scrollbar.thumb(200.0, 10.0), (150.0, 50.0));
        let top = Scrollbar {
            offset: 0,
            ..scrollbar
        };
        assert_eq!(top.thumb(200.0, 10.0), (0.0, 50.0));
        let long = Scrollbar {
            total: 10_000,
            ..top
        };
        assert_eq!(long.thumb(200.0, 10.0).1, 10.0);
    }

    #[test]
    fn a_thumb_position_maps_back_to_its_row() {
        let scrollbar = Scrollbar {
            total: 100,
            offset: 30,
            len: 25,
            ..Scrollbar::default()
        };
        let (top, _) = scrollbar.thumb(200.0, 10.0);
        assert_eq!(scrollbar.row_at(top, 200.0, 10.0), 30);
        assert_eq!(scrollbar.row_at(-50.0, 200.0, 10.0), 0);
        assert_eq!(scrollbar.row_at(500.0, 200.0, 10.0), 75);
    }

    #[test]
    fn only_a_primary_screen_with_history_has_a_scrollbar() {
        assert_eq!(Grid::from_vt(b"hi", 20, 5).unwrap().scrollbar, None);
        let mut history = String::new();
        for line in 0..30 {
            history.push_str(&format!("line{line}\r\n"));
        }
        let grid = Grid::from_vt(history.as_bytes(), 20, 5).unwrap();
        assert_eq!(
            grid.scrollbar,
            Some(Scrollbar {
                total: 31,
                offset: 26,
                len: 5,
                ..Scrollbar::default()
            })
        );
        history.push_str("\x1b[?1049h");
        assert_eq!(
            Grid::from_vt(history.as_bytes(), 20, 5).unwrap().scrollbar,
            None
        );
    }

    #[test]
    fn output_below_a_scrolled_viewport_publishes_a_new_scrollbar() {
        let mut terminal = scrolled_terminal();
        terminal.scroll_viewport(ScrollViewport::Top);
        let mut builder = GridBuilder::new().unwrap();
        let shown = Presentation::default();
        let before = builder.build(&terminal, 1, shown).unwrap().unwrap();
        terminal.vt_write(b"more\r\n");
        let after = builder.build(&terminal, 2, shown).unwrap().unwrap();
        assert_ne!(after.scrollbar, before.scrollbar);
    }

    #[test]
    fn pressing_the_track_and_dragging_the_thumb_scrolls() {
        let mut terminal = scrolled_terminal();
        let mut input = input_for_test();
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Press, 0.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line0");
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Drag, 100.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line26");
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Release, 100.0)
            .unwrap();
        assert_eq!(input.scrollbar_grab, None);
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Drag, 0.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line26");
    }

    fn faded(terminal: &Terminal<'static, '_>) -> (ScrollbarFade, Instant) {
        let mut fade = ScrollbarFade::default();
        fade.follow(terminal);
        (fade, Instant::now())
    }

    #[test]
    fn output_alone_leaves_the_scrollbar_hidden() {
        let mut terminal = scrolled_terminal();
        let (mut fade, now) = faded(&terminal);
        terminal.vt_write(b"more\r\n");
        fade.follow(&terminal);
        assert_eq!(fade.opacity(&terminal, false, now), 0.0);
        assert_eq!(fade.next_change(now), None);
    }

    #[test]
    fn a_scroll_shows_the_scrollbar_until_it_fades() {
        let mut terminal = scrolled_terminal();
        let (mut fade, now) = faded(&terminal);
        terminal.scroll_viewport(ScrollViewport::Delta(-3));
        assert_eq!(fade.opacity(&terminal, false, now), 1.0);
        assert_eq!(fade.next_change(now), Some(now + SCROLLBAR_HOLD));
        let fading = now + SCROLLBAR_HOLD + SCROLLBAR_FADE / 2;
        assert_eq!(fade.opacity(&terminal, false, fading), 0.5);
        assert_eq!(fade.next_change(fading), Some(fading + FRAME));
        let gone = now + SCROLLBAR_HOLD + SCROLLBAR_FADE;
        assert_eq!(fade.opacity(&terminal, false, gone), 0.0);
        assert_eq!(fade.next_change(gone), None);
    }

    #[test]
    fn a_held_scrollbar_stays_until_a_while_after_release() {
        let terminal = scrolled_terminal();
        let (mut fade, now) = faded(&terminal);
        assert_eq!(fade.opacity(&terminal, true, now), 1.0);
        assert_eq!(fade.next_change(now), None);
        let released = now + 5 * SCROLLBAR_HOLD;
        assert_eq!(fade.opacity(&terminal, false, released), 1.0);
        assert_eq!(fade.next_change(released), Some(released + SCROLLBAR_HOLD));
    }

    #[test]
    fn a_scrollbar_drag_holds_until_release() {
        let mut terminal = scrolled_terminal();
        let mut input = input_for_test();
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Press, 50.0)
            .unwrap();
        assert!(input.scrollbar_grab.is_some());
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Release, 50.0)
            .unwrap();
        assert!(input.scrollbar_grab.is_none());
    }

    #[test]
    fn the_scrollbar_carries_its_look_from_the_presentation() {
        let terminal = scrolled_terminal();
        let shown = Presentation {
            scrollbar_opacity: 0.5,
            scrollbar_wide: true,
            ..Presentation::default()
        };
        let grid = GridBuilder::new()
            .unwrap()
            .build(&terminal, 1, shown)
            .unwrap()
            .unwrap();
        let scrollbar = grid.scrollbar.unwrap();
        assert_eq!((scrollbar.opacity, scrollbar.wide), (0.5, true));
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
        assert_eq!(bytes, None);
        assert_ne!(top_line(&terminal), before);
    }

    #[test]
    fn the_wheel_sends_arrows_on_the_alternate_screen() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1049h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(40.0))
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"\x1b[A\x1b[A");
    }

    #[test]
    fn the_wheel_reports_buttons_to_a_tracking_program() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(20.0))
            .unwrap()
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
