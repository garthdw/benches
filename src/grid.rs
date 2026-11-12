use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use libghostty_vt::color::Rgb;
use libghostty_vt::effect::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::mode::Mode;
use libghostty_vt::render::{
    CellIteration, CellIterator, Colors, CursorVisualStyle, Dirty, RowIterator,
};
use libghostty_vt::report::{ColorScheme, SizeReportSize};
use libghostty_vt::screen::{CellContentTag, CellWide, Screen, cell};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::{RenderState, Terminal};

use crate::LogErr;
use crate::config::Theme;
use crate::images::{ImageReader, Placement};

pub(crate) const FRAME: Duration = Duration::from_millis(8);

const SCROLLBAR_HOLD: Duration = Duration::from_secs(1);
const SCROLLBAR_FADE: Duration = Duration::from_millis(200);
const MESSAGE_COLS: u16 = 80;
pub const DEFAULT_FG: Rgb = Rgb {
    r: 0xd8,
    g: 0xd9,
    b: 0xda,
};
pub const DEFAULT_BG: Rgb = Rgb {
    r: 0x14,
    g: 0x15,
    b: 0x17,
};
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

#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// In the selection, which splits ligatures as the cursor does.
    pub selected: bool,
    /// Index into `Grid::clusters` when the cell holds more than one codepoint.
    pub cluster: Option<u32>,
}

impl Cell {
    /// Whether two neighbours may be shaped together, which every style difference
    /// except the background forbids, as in Ghostty, so a ligature never changes colour midway.
    pub fn shapes_with(&self, other: &Cell) -> bool {
        (
            self.fg,
            self.bold,
            self.italic,
            self.selected,
            self.strikethrough,
            self.overline,
            self.underline,
            self.underline_color,
        ) == (
            other.fg,
            other.bold,
            other.italic,
            other.selected,
            other.strikethrough,
            other.overline,
            other.underline,
            other.underline_color,
        )
    }
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
            selected: false,
            cluster: None,
        }
    }
}

pub struct Grid {
    pub generation: u64,
    pub cols: u16,
    pub cells: Arc<[Cell]>,
    pub clusters: Arc<[Arc<str>]>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_style: CursorVisualStyle,
    pub cursor_blinking: bool,
    pub cursor_color: [u8; 3],
    pub password_input: bool,
    pub metrics: Metrics,
    pub bg: [u8; 3],
    pub scrollbar: Option<Scrollbar>,
    pub images: Arc<[Placement]>,
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
            password_input: false,
            metrics: Metrics::default(),
            bg: rgb(DEFAULT_BG),
            scrollbar: None,
            images: Arc::default(),
        }
    }
}

impl Grid {
    /// Parses `bytes` on the calling thread, for fixtures and tests.
    pub fn from_vt(bytes: &[u8], cols: u16, rows: u16) -> Result<Self> {
        Self::from_vt_with_cells(bytes, cols, rows, (0, 0))
    }

    /// As `from_vt`, with cells of `(width, height)` pixels, which sizes images; zero leaves them unset.
    pub fn from_vt_with_cells(
        bytes: &[u8],
        cols: u16,
        rows: u16,
        (cell_width, cell_height): (u32, u32),
    ) -> Result<Self> {
        let mut terminal = new_terminal(cols, rows)?;
        if cell_width > 0 && cell_height > 0 {
            terminal.resize(cols, rows, cell_width, cell_height)?;
        }
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

    pub fn cluster(&self, cell: &Cell) -> Option<&Arc<str>> {
        let index = usize::try_from(cell.cluster?).ok()?;
        self.clusters.get(index)
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
    pub(crate) total: u64,
    offset: u64,
    len: u64,
    /// Zero while hidden; the thumb fades out after scrolling stops.
    pub opacity: f32,
    /// While hovered or dragged.
    pub wide: bool,
}

impl Scrollbar {
    /// `None` when there is nothing to scroll, including on the alternate screen, which has no scrollback.
    pub(crate) fn of(terminal: &Terminal<'static, '_>) -> Result<Option<Self>> {
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
    pub(crate) fn row_at(self, top: f32, track: f32, min: f32) -> usize {
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
pub(crate) struct ScrollbarFade {
    until: Option<Instant>,
    held: bool,
    /// The viewport offset that output or a resize last left, so only other moves count as scrolls.
    offset: Option<u64>,
}

impl ScrollbarFade {
    pub(crate) fn show(&mut self, now: Instant) {
        self.until = Some(now + SCROLLBAR_HOLD);
    }

    pub(crate) fn follow(&mut self, terminal: &Terminal<'static, '_>) {
        self.offset = terminal
            .scrollbar()
            .log_err("read scrollbar")
            .map(|bar| bar.offset);
    }

    /// The thumb's opacity at `now`, showing it if the viewport moved since the last call or
    /// `follow`, or the pointer holds it or just let it go.
    pub(crate) fn opacity(
        &mut self,
        terminal: &Terminal<'static, '_>,
        held: bool,
        now: Instant,
    ) -> f32 {
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
    pub(crate) fn next_change(&self, now: Instant) -> Option<Instant> {
        let until = self.until.filter(|_| !self.held)?;
        if now < until {
            Some(until)
        } else {
            (now < until + SCROLLBAR_FADE).then_some(now + FRAME)
        }
    }
}

pub(crate) fn rgb(color: Rgb) -> [u8; 3] {
    [color.r, color.g, color.b]
}

fn faint(color: [u8; 3], background: [u8; 3]) -> [u8; 3] {
    std::array::from_fn(|index| {
        ((u16::from(color[index]) + u16::from(background[index])) / 2) as u8
    })
}

/// What the actor knows that the terminal doesn't.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Presentation {
    pub(crate) focused: bool,
    /// The PTY is in the mode programs use to read a password, which libghostty can't see.
    pub(crate) password_input: bool,
    pub(crate) cursor_on: bool,
    pub(crate) metrics: Metrics,
    pub(crate) scrollbar_opacity: f32,
    pub(crate) scrollbar_wide: bool,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            focused: true,
            password_input: false,
            cursor_on: true,
            metrics: Metrics::default(),
            scrollbar_opacity: 0.0,
            scrollbar_wide: false,
        }
    }
}

type Content = (Arc<[Cell]>, Arc<[Arc<str>]>);

/// Reads one cell through libghostty's cell iterator, which costs several calls.
fn read_cell(
    cell: &CellIteration<'_, '_>,
    colors: &Colors,
    (selection_background, selection_foreground): (Option<Rgb>, Option<Rgb>),
    clusters: &mut Vec<Arc<str>>,
) -> Result<Cell> {
    let mut fg = cell.fg_color()?.unwrap_or(colors.foreground);
    let mut bg = cell.bg_color()?;
    let mut output = Cell::default();
    let mut invisible = false;
    let mut is_faint = false;
    let selected = cell.is_selected()?;
    output.selected = selected;
    let mut inverted = selected && selection_background.is_none();
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
            StyleColor::Palette(index) => Some(rgb(colors.palette[usize::from(index.0)])),
        };
        invisible = style.invisible;
        inverted ^= style.inverse;
    }
    if inverted {
        let old_fg = fg;
        fg = bg.unwrap_or(colors.background);
        bg = Some(old_fg);
    }
    if selected && let Some(background) = selection_background {
        bg = Some(background);
        fg = selection_foreground.unwrap_or(fg);
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
                clusters.push(cluster.into());
            }
            Err(error) => return Err(error.into()),
        }
        output.character = character[0];
    }
    Ok(output)
}

pub(crate) struct GridBuilder {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    last_shown: Option<(
        CursorVisualStyle,
        bool,
        Presentation,
        Option<Scrollbar>,
        u64,
    )>,
    images: ImageReader,
    /// The cells and clusters of the last build, for frames where only the presentation changed.
    content: Option<Content>,
    /// From the theme; without a background, selected cells swap their colours.
    pub(crate) selection_background: Option<Rgb>,
    pub(crate) selection_foreground: Option<Rgb>,
    /// Off only in tests, to compare against reading every cell the slow way.
    pub(crate) plain_fast_path: bool,
}

impl GridBuilder {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            last_shown: None,
            images: ImageReader::new()?,
            content: None,
            selection_background: None,
            selection_foreground: None,
            plain_fast_path: true,
        })
    }

    /// libghostty isn't known to mark the render state dirty when only the selection moves.
    pub(crate) fn selection_changed(&mut self) {
        self.last_shown = None;
        self.content = None;
    }

    pub(crate) fn build(
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
        let image_generation = snapshot.image_placements_generation()?;
        let shown = (
            cursor_style,
            cursor_blinking,
            presentation,
            scrollbar,
            image_generation,
        );
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
                // Runs of identical cells, mostly blanks, share one decode.
                let mut decoded = None;
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
                    let selection = row.selection()?;
                    let mut cell_iterator = self.cells.update(row)?;
                    for (column, raw) in row.cells_raw()?.enumerate() {
                        let column = u16::try_from(column)?;
                        let (tag, codepoint, width, styled) = match decoded {
                            Some((previous, values)) if previous == raw => values,
                            _ => {
                                let values = raw.get_multi((
                                    cell::ContentTag,
                                    cell::Codepoint,
                                    cell::Wide,
                                    cell::HasStyling,
                                ))?;
                                decoded = Some((raw, values));
                                values
                            }
                        };
                        let selected = selection
                            .is_some_and(|range| (range.start_x..=range.end_x).contains(&column));
                        let character = match codepoint {
                            0 => Some(' '),
                            codepoint => char::from_u32(codepoint),
                        };
                        if let Some(character) = character
                            && self.plain_fast_path
                            && matches!(tag, CellContentTag::Codepoint)
                            && !styled
                            && !selected
                        {
                            cells.push(Cell {
                                character,
                                fg: rgb(colors.foreground),
                                wide: width == CellWide::Wide,
                                spacer: matches!(
                                    width,
                                    CellWide::SpacerTail | CellWide::SpacerHead
                                ),
                                ..Cell::default()
                            });
                            continue;
                        }
                        cell_iterator.select(column)?;
                        cells.push(read_cell(
                            &cell_iterator,
                            &colors,
                            (self.selection_background, self.selection_foreground),
                            &mut clusters,
                        )?);
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
        let images = self.images.read(&snapshot, terminal, image_generation)?;
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
            password_input: presentation.password_input,
            metrics: presentation.metrics,
            bg: rgb(colors.background),
            scrollbar,
            images,
        }))
    }
}

/// The bundled compiled terminfo directory, found relative to `exe` inside a `.app`.
pub(crate) fn bundled_terminfo(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?.parent()?.join("Resources/terminfo");
    dir.join("78/xterm-ghostty").is_file().then_some(dir)
}

pub(crate) fn terminfo_name(bundled: Option<&Path>) -> &'static str {
    if bundled.is_some() {
        "xterm-ghostty"
    } else {
        "xterm-256color"
    }
}

/// Per screen, as decoded pixels. Ghostty allows 320 MB, which several tabs of large images would multiply.
const IMAGE_STORAGE_LIMIT: u64 = 128 * 1024 * 1024;

/// libghostty's PNG hook is process-global and unsynchronised, so it is set once, before the
/// first terminal exists.
fn enable_png_images() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: every terminal is made by `new_terminal`, and `Once` makes other callers wait
        // here, so no thread is inside libghostty yet.
        unsafe {
            libghostty_vt::kitty::graphics::set_png_decoder(Some(Box::new(
                libghostty_vt::kitty::graphics::RustPngDecoder::new(),
            )))
        }
        .log_err("install PNG decoder");
    });
}

pub(crate) fn new_terminal<'cb>(cols: u16, rows: u16) -> Result<Terminal<'static, 'cb>> {
    enable_png_images();
    let mut terminal = Terminal::new(cols, rows)?;
    terminal
        .set_scrollback_max_lines(Some(10_000))?
        .set_terminfo_name(terminfo_name(
            std::env::current_exe()
                .ok()
                .and_then(|exe| bundled_terminfo(&exe))
                .as_deref(),
        ))?
        .set_default_fg_color(Some(DEFAULT_FG))?
        .set_default_bg_color(Some(DEFAULT_BG))?;
    terminal.set_mode(Mode::GRAPHEME_CLUSTER, true)?;
    // Images arrive inline or as a temp file, which `kitten icat` uses on a local terminal. The file
    // and shared memory media stay off, as they let a program, such as one over ssh, name any local
    // path or segment to be read and displayed.
    terminal
        .set_kitty_image_storage_limit(IMAGE_STORAGE_LIMIT)?
        .set_kitty_image_temp_file_dir(Some(&std::env::temp_dir()))?;
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

/// Answers the queries libghostty-vt leaves unanswered without a callback. The replies match
/// Ghostty's, so programs that probe it for features behave the same here.
pub(crate) fn answer_queries(terminal: &mut Terminal<'static, '_>, theme: &Theme) -> Result<()> {
    let Rgb { r, g, b } = theme.background;
    let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    let scheme = if luma < 128.0 {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    terminal.on_device_attributes(|_| {
        Some(DeviceAttributes {
            primary: PrimaryDeviceAttributes::new(
                ConformanceLevel::LEVEL_2,
                &[
                    DeviceAttributeFeature::ANSI_COLOR,
                    DeviceAttributeFeature::CLIPBOARD,
                ],
            ),
            secondary: SecondaryDeviceAttributes {
                device_type: DeviceType::VT220,
                firmware_version: 10,
                rom_cartridge: 0,
            },
            tertiary: TertiaryDeviceAttributes::default(),
        })
    })?;
    terminal.on_xtversion(|_| Some(concat!("agentd ", env!("CARGO_PKG_VERSION"))))?;
    terminal.on_enquiry(|_| None)?;
    terminal.on_color_scheme(move |_| Some(scheme))?;
    terminal.on_size(|terminal| {
        let (cols, rows) = (terminal.cols().ok()?, terminal.rows().ok()?);
        Some(SizeReportSize {
            rows,
            columns: cols,
            cell_width: terminal.width_px().ok()?.checked_div(u32::from(cols))?,
            cell_height: terminal.height_px().ok()?.checked_div(u32::from(rows))?,
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Theme;

    use crate::test_support::{THEME_BLUE, THEME_RED, fractional_metrics, scrolled_terminal};
    use libghostty_vt::render::ImageLayer;
    use libghostty_vt::terminal::ScrollViewport;

    fn reply_to(theme: &Theme, query: &[u8]) -> Vec<u8> {
        let replies = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = new_terminal(80, 24).unwrap();
        terminal.resize(80, 24, 9, 18).unwrap();
        answer_queries(&mut terminal, theme).unwrap();
        terminal
            .on_pty_write({
                let replies = replies.clone();
                move |_, data| replies.borrow_mut().extend_from_slice(data)
            })
            .unwrap();
        terminal.vt_write(query);
        replies.take()
    }

    #[test]
    fn device_attributes_quack_as_a_level_two_terminal_with_ghosttys_features() {
        let theme = Theme::default();
        assert_eq!(reply_to(&theme, b"\x1b[c"), b"\x1b[?62;22;52c");
        assert_eq!(reply_to(&theme, b"\x1b[>c"), b"\x1b[>1;10;0c");
        assert_eq!(reply_to(&theme, b"\x1b[=c"), b"\x1bP!|00000000\x1b\\");
    }

    #[test]
    fn xtversion_names_agentd_and_its_version() {
        let expected = format!("\x1bP>|agentd {}\x1b\\", env!("CARGO_PKG_VERSION"));
        assert_eq!(reply_to(&Theme::default(), b"\x1b[>q"), expected.as_bytes());
    }

    #[test]
    fn enquiry_gets_no_answer() {
        assert_eq!(reply_to(&Theme::default(), b"\x05"), b"");
    }

    #[test]
    fn size_queries_report_cells_and_pixels() {
        let theme = Theme::default();
        assert_eq!(reply_to(&theme, b"\x1b[14t"), b"\x1b[4;432;720t");
        assert_eq!(reply_to(&theme, b"\x1b[16t"), b"\x1b[6;18;9t");
        assert_eq!(reply_to(&theme, b"\x1b[18t"), b"\x1b[8;24;80t");
        assert_eq!(reply_to(&theme, b"\x1b[?2048h"), b"\x1b[48;24;80;432;720t");
    }

    #[test]
    fn a_resize_is_reported_in_band_once_mode_2048_is_set() {
        let replies = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut terminal = new_terminal(80, 24).unwrap();
        answer_queries(&mut terminal, &Theme::default()).unwrap();
        terminal
            .on_pty_write({
                let replies = replies.clone();
                move |_, data| replies.borrow_mut().extend_from_slice(data)
            })
            .unwrap();
        terminal.vt_write(b"\x1b[?2048h");
        replies.take();
        terminal.resize(100, 30, 10, 20).unwrap();
        assert_eq!(replies.take(), b"\x1b[48;30;100;600;1000t");
    }

    #[test]
    fn the_colour_scheme_follows_the_background_luminance() {
        let light = Theme {
            background: Rgb {
                r: 0xfa,
                g: 0xfa,
                b: 0xfa,
            },
            ..Theme::default()
        };
        assert_eq!(reply_to(&Theme::default(), b"\x1b[?996n"), b"\x1b[?997;1n");
        assert_eq!(reply_to(&light, b"\x1b[?996n"), b"\x1b[?997;2n");
    }

    #[test]
    fn terminfo_is_bundled_only_when_the_entry_exists() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("Contents/MacOS/agentd");
        assert_eq!(bundled_terminfo(&exe), None);
        assert_eq!(terminfo_name(None), "xterm-256color");

        let entry = root.path().join("Contents/Resources/terminfo/78");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("xterm-ghostty"), "").unwrap();
        let dir = bundled_terminfo(&exe);
        assert_eq!(dir, Some(root.path().join("Contents/Resources/terminfo")));
        assert_eq!(terminfo_name(dir.as_deref()), "xterm-ghostty");
    }

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
        assert_eq!(grid.cluster(&grid.cells[0]), Some(&"e\u{301}".into()));
        assert_eq!(grid.cluster(&grid.cells[1]), Some(&family.into()));
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
            "steady styled output (scrolling)",
            &mut terminal,
            &mut builder,
            |terminal, frame| {
                let entry = format!(
                    "\x1b[1;3{}mdrwxr-xr-x\x1b[0m 4 user \x1b[38;5;{}mstaff\x1b[0m \x1b[4;44mfile-{frame}.rs\x1b[0m \x1b[2mdim\x1b[7mrev\x1b[0m ",
                    frame % 8,
                    frame % 256
                );
                terminal.vt_write(format!("{}\r\n", entry.repeat(3)).as_bytes());
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
        assert_eq!(second.cluster(&second.cells[0]), Some(&"e\u{301}".into()));
    }

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

    const TWO_PIXELS: &str = "\x1b_Ga=T,f=32,s=2,v=1,i=7,q=2;/wAA/wD/AP8=\x1b\\";

    fn image_terminal<'cb>() -> Terminal<'static, 'cb> {
        let mut terminal = new_terminal(10, 5).unwrap();
        terminal.resize(10, 5, 10, 20).unwrap();
        terminal
    }

    fn build(builder: &mut GridBuilder, terminal: &Terminal<'static, '_>, generation: u64) -> Grid {
        builder
            .build(terminal, generation, Presentation::default())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_placed_image_is_carried_in_the_grid_at_its_cell() {
        let mut terminal = image_terminal();
        terminal.vt_write(b"\x1b[3;4H");
        terminal.vt_write(TWO_PIXELS.as_bytes());
        let grid = build(&mut GridBuilder::new().unwrap(), &terminal, 1);
        let [placement] = &grid.images[..] else {
            panic!("expected one placement, got {}", grid.images.len());
        };
        assert_eq!((placement.col, placement.row), (3, 2));
        assert_eq!(placement.size, [2, 1]);
        assert_eq!(placement.layer, ImageLayer::AboveText);
        assert_eq!(placement.image.rgba, [255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!((placement.image.width, placement.image.height), (2, 1));
    }

    #[test]
    fn an_image_scrolls_with_the_text_and_shares_its_pixels() {
        let mut terminal = image_terminal();
        let mut builder = GridBuilder::new().unwrap();
        terminal.vt_write(b"\x1b[4;1H");
        terminal.vt_write(TWO_PIXELS.as_bytes());
        let before = build(&mut builder, &terminal, 1);
        terminal.vt_write(b"\x1b[5;1H\r\n\r\n");
        let after = build(&mut builder, &terminal, 2);
        assert_eq!((before.images[0].row, after.images[0].row), (3, 1));
        assert!(Arc::ptr_eq(&before.images[0].image, &after.images[0].image));
    }

    #[test]
    fn clearing_the_screen_or_deleting_removes_the_image() {
        let mut builder = GridBuilder::new().unwrap();
        let mut terminal = image_terminal();
        terminal.vt_write(TWO_PIXELS.as_bytes());
        assert_eq!(build(&mut builder, &terminal, 1).images.len(), 1);
        terminal.vt_write(b"\x1b[H\x1b[2J");
        assert!(build(&mut builder, &terminal, 2).images.is_empty());
        terminal.vt_write(TWO_PIXELS.as_bytes());
        assert_eq!(build(&mut builder, &terminal, 3).images.len(), 1);
        terminal.vt_write(b"\x1b_Ga=d,d=A,q=2\x1b\\");
        assert!(build(&mut builder, &terminal, 4).images.is_empty());
    }

    #[test]
    fn a_text_only_frame_keeps_the_list_without_reading_it_again() {
        let mut builder = GridBuilder::new().unwrap();
        let mut terminal = image_terminal();
        terminal.vt_write(TWO_PIXELS.as_bytes());
        let before = build(&mut builder, &terminal, 1);
        terminal.vt_write(b"\x1b[5;5Hx");
        let after = build(&mut builder, &terminal, 2);
        assert!(Arc::ptr_eq(&before.images, &after.images));
    }

    #[test]
    fn placements_come_in_layers_then_z_order() {
        let mut terminal = image_terminal();
        for (id, z) in [(1, 5), (2, -1_073_741_825), (3, -1), (4, 0)] {
            terminal
                .vt_write(format!("\x1b_Ga=T,f=24,s=1,v=1,i={id},z={z},q=2;////\x1b\\").as_bytes());
        }
        let grid = build(&mut GridBuilder::new().unwrap(), &terminal, 1);
        let order: Vec<(u32, ImageLayer)> = grid
            .images
            .iter()
            .map(|placement| (placement.image.id, placement.layer))
            .collect();
        assert_eq!(
            order,
            [
                (2, ImageLayer::BelowBackground),
                (3, ImageLayer::BelowText),
                (4, ImageLayer::AboveText),
                (1, ImageLayer::AboveText),
            ]
        );
        assert_eq!(
            grid.images[0].image.rgba, [255; 4],
            "RGB gains an opaque alpha"
        );
    }

    #[test]
    fn a_png_is_decoded_for_the_grid() {
        let mut terminal = image_terminal();
        terminal.vt_write(
            b"\x1b_Ga=T,f=100,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\",
        );
        let grid = build(&mut GridBuilder::new().unwrap(), &terminal, 1);
        assert_eq!(grid.images[0].image.rgba, [255, 0, 0, 255]);
    }

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        bytes
            .chunks(3)
            .flat_map(|chunk| {
                let word = chunk.iter().enumerate().fold(0u32, |word, (index, byte)| {
                    word | u32::from(*byte) << (16 - 8 * index)
                });
                (0..4).map(move |index| {
                    if index <= chunk.len() {
                        char::from(ALPHABET[(word >> (18 - 6 * index) & 63) as usize])
                    } else {
                        '='
                    }
                })
            })
            .collect()
    }

    #[test]
    fn a_temp_file_loads_but_a_plain_file_does_not() {
        let mut terminal = image_terminal();
        let file = tempfile::Builder::new()
            .prefix("tty-graphics-protocol-")
            .tempfile()
            .unwrap();
        std::fs::write(file.path(), [1, 2, 3, 255]).unwrap();
        let path = file.path().to_str().unwrap().as_bytes();
        let payload = base64(path);
        terminal.vt_write(format!("\x1b_Ga=T,t=f,f=32,s=1,v=1,i=1,q=2;{payload}\x1b\\").as_bytes());
        terminal.vt_write(format!("\x1b_Ga=T,t=t,f=32,s=1,v=1,i=2,q=2;{payload}\x1b\\").as_bytes());
        let grid = build(&mut GridBuilder::new().unwrap(), &terminal, 1);
        let ids: Vec<u32> = grid
            .images
            .iter()
            .map(|placement| placement.image.id)
            .collect();
        assert_eq!(ids, [2]);
    }

    #[test]
    fn an_animation_tick_publishes_the_next_frame_as_a_new_generation() {
        let mut terminal = image_terminal();
        let mut builder = GridBuilder::new().unwrap();
        terminal.vt_write(
            b"\x1b_Ga=T,f=32,i=2,s=1,v=1,q=2;/wAA/w==\x1b\\\
              \x1b_Ga=f,f=32,i=2,s=1,v=1,z=40,q=2;AAD//w==\x1b\\\
              \x1b_Ga=a,i=2,r=1,z=30,q=2\x1b\\\
              \x1b_Ga=a,i=2,s=3,q=2\x1b\\",
        );
        assert_eq!(terminal.kitty_graphics_animation_tick(0).unwrap(), Some(30));
        let first = build(&mut builder, &terminal, 1);
        assert_eq!(first.images[0].image.rgba, [255, 0, 0, 255]);
        assert_eq!(
            terminal.kitty_graphics_animation_tick(30).unwrap(),
            Some(70)
        );
        let second = build(&mut builder, &terminal, 2);
        assert_eq!(second.images[0].image.rgba, [0, 0, 255, 255]);
        assert_ne!(
            first.images[0].image.generation,
            second.images[0].image.generation
        );
    }
}
