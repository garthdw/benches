use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable, cast_slice};
use crossbeam_channel::{Receiver, Sender, unbounded};
use libghostty_vt::render::{CursorVisualStyle, ImageLayer};
use libghostty_vt::style::Underline;
use rune::Waker;
use rune::color::srgb_decode;
use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::EffectSite;

use crate::LogErr;
use crate::constraint::{Constraint, is_graphics_element, is_symbol};
use crate::font::{FnvMap, Font, FontStyle, GlyphKind, ShapedRun};
use crate::grid::{Cell, Grid, Metrics, Scrollbar, Surface};
use crate::images::{Image, Placement};
use crate::session::Session;

/// Also the width of the scrollbar's strip, which takes the right padding.
pub const PADDING_POINTS: f32 = 6.0;
const SCROLLBAR_POINTS: f32 = 3.0;
const WIDE_SCROLLBAR_POINTS: f32 = 5.0;
const ATLAS_SIZE: u32 = 2048;
const MODE_SOLID: u32 = 0;
const MODE_COVERAGE: u32 = 1;
const MODE_COLOR: u32 = 2;
/// A colour glyph whose sRGB texels must be decoded for a linear target.
const MODE_COLOR_LINEAR: u32 = 3;
/// An image texel, which `fs_image` premultiplies; the linear variant decodes it first.
const MODE_IMAGE: u32 = 4;
const MODE_IMAGE_LINEAR: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Quad {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 3],
    mode: u32,
}

fn color(rgb: [u8; 3], srgb_blend: bool) -> [f32; 3] {
    rgb.map(|channel| {
        let encoded = f32::from(channel) / 255.0;
        if srgb_blend {
            encoded
        } else {
            srgb_decode(encoded)
        }
    })
}

/// Cell `index` of a run shaped together, for a cell that differs from standing alone. Cells
/// are the same when their glyphs are, wherever the run sits, so the signature alone is the key.
#[derive(Clone)]
struct ShapedCell {
    run: Rc<str>,
    index: u16,
    signature: u64,
}

impl PartialEq for ShapedCell {
    fn eq(&self, other: &Self) -> bool {
        self.signature == other.signature
    }
}

impl Eq for ShapedCell {}

impl std::hash::Hash for ShapedCell {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.signature.hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum GlyphText {
    Character(char),
    Cluster(Arc<str>),
    Shaped(ShapedCell),
    Underline(Underline),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct GlyphKey {
    text: GlyphText,
    style: FontStyle,
    cells: u32,
}

#[derive(Clone, Copy)]
struct AtlasGlyph {
    position: [u32; 2],
    kind: GlyphKind,
}

/// Two, as more slowed a CJK flood on a 6+2 core machine without shortening frames.
const WORKERS: usize = 2;
/// How long a frame waits for glyphs it asked for before drawing without them, so ordinary
/// screens never show missing text while a screen of new glyphs can't stall every surface.
const GLYPH_WAIT: Duration = Duration::from_millis(4);

/// What a worker rasterises; `GlyphText` without its `Rc`, which can't leave the UI thread.
enum JobText {
    Character(char),
    Cluster(String),
    Shaped { run: String, index: u16 },
    Underline(Underline),
}

impl From<&GlyphText> for JobText {
    fn from(text: &GlyphText) -> Self {
        match text {
            GlyphText::Character(character) => Self::Character(*character),
            GlyphText::Cluster(cluster) => Self::Cluster(cluster.to_string()),
            GlyphText::Shaped(cell) => Self::Shaped {
                run: cell.run.to_string(),
                index: cell.index,
            },
            GlyphText::Underline(underline) => Self::Underline(*underline),
        }
    }
}

struct Job {
    id: u64,
    text: JobText,
    style: FontStyle,
    cells: u32,
}

struct Rasterized {
    id: u64,
    kind: Option<GlyphKind>,
    bitmap: Vec<u8>,
}

fn rasterize(
    font: &Font,
    text: &JobText,
    style: FontStyle,
    cells: u32,
    out: &mut Vec<u8>,
) -> Option<GlyphKind> {
    let mut utf8 = [0; 4];
    match text {
        JobText::Character(character) => {
            font.rasterize(character.encode_utf8(&mut utf8), style, cells, out)
        }
        JobText::Cluster(cluster) => font.rasterize(cluster, style, cells, out),
        JobText::Shaped { run, index } => {
            font.rasterize_shaped(run, style, usize::from(*index), out)
        }
        JobText::Underline(underline) => {
            font.rasterize_underline(*underline, out);
            Some(GlyphKind::Coverage)
        }
    }
}

fn load_font(config: &crate::config::FontConfig, size: f64, scale: f32) -> anyhow::Result<Font> {
    let mut font = Font::with_symbols(
        &config.family,
        size,
        scale,
        config.symbols_family.as_deref(),
        &config.fallback_families,
        &config.features,
    )?;
    font.break_runs_at_cursor = config.shaping_breaks_at_cursor;
    if let Some(modifier) = config.adjust_icon_height {
        font.adjust_icon_height(modifier);
    }
    Ok(font)
}

/// Rasterises glyphs off the UI thread, each worker with its own `Font`, as CoreText objects and
/// the font's caches stay on one thread. Dropping it stops the workers.
struct Rasterizer {
    jobs: Sender<Job>,
    done: Receiver<Rasterized>,
    /// Set once a result is waiting, so a burst of glyphs wakes the window once.
    woken: Arc<AtomicBool>,
}

impl Rasterizer {
    fn new(config: &crate::config::FontConfig, size: f64, scale: f32, wake: Waker) -> Self {
        let (jobs, queued) = unbounded::<Job>();
        let (finished, done) = unbounded();
        let woken = Arc::new(AtomicBool::new(false));
        for _ in 0..WORKERS {
            let (queued, finished, config, wake, woken) = (
                queued.clone(),
                finished.clone(),
                config.clone(),
                wake.clone(),
                woken.clone(),
            );
            thread::spawn(move || {
                // Utility QoS leaves the performance cores to the PTY readers and actors, which a
                // flood keeps busy; a glyph that lands a frame later costs less than slower output.
                // SAFETY: changes only the calling thread's QoS class.
                if unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0)
                } != 0
                {
                    eprintln!("agentd: lower the glyph rasteriser's priority failed");
                }
                let Some(font) = load_font(&config, size, scale).log_err("load font to rasterise")
                else {
                    return;
                };
                for job in queued {
                    let mut bitmap = Vec::new();
                    let kind = rasterize(&font, &job.text, job.style, job.cells, &mut bitmap);
                    if finished
                        .send(Rasterized {
                            id: job.id,
                            kind,
                            bitmap,
                        })
                        .is_err()
                    {
                        break;
                    }
                    if !woken.swap(true, Ordering::AcqRel) {
                        wake();
                    }
                }
            });
        }
        Self { jobs, done, woken }
    }
}

#[derive(Clone, Copy)]
enum Wait {
    /// Only for glyphs already finished.
    Ready,
    Until(Instant),
    /// For every requested glyph, as a headless render has a single frame to draw.
    All,
}

impl Wait {
    fn expired(self) -> bool {
        matches!(self, Self::Until(deadline) if Instant::now() >= deadline)
    }
}

enum AtlasSlot {
    Pending,
    Ready(Option<AtlasGlyph>),
}

struct Atlas {
    slots: FnvMap<GlyphKey, AtlasSlot>,
    /// Glyphs out with the rasterizer, by job id.
    requested: HashMap<u64, GlyphKey>,
    next_job: u64,
    next: [u32; 2],
    epoch: u64,
    /// Whether a lookup since this was last cleared found a glyph not rasterised yet.
    missing: bool,
    /// Ready plain ASCII glyphs by codepoint, so the commonest cells skip hashing their key.
    ascii: [Option<AtlasGlyph>; 128],
}

impl Atlas {
    fn new(epoch: u64) -> Self {
        Self {
            slots: FnvMap::default(),
            requested: HashMap::new(),
            next_job: 0,
            next: [1, 1],
            epoch,
            missing: false,
            ascii: [None; 128],
        }
    }

    /// Empties the texture; glyphs still out with the rasterizer land in the new one.
    fn reset(&mut self) {
        self.slots
            .retain(|_, slot| matches!(slot, AtlasSlot::Pending));
        self.next = [1, 1];
        self.ascii = [None; 128];
        self.epoch += 1;
    }

    /// The glyph if it's ready; otherwise asks `jobs` for it, once, and notes it missing.
    fn glyph(&mut self, jobs: &Sender<Job>, key: GlyphKey) -> Option<AtlasGlyph> {
        let ascii = match key.text {
            GlyphText::Character(character)
                if character.is_ascii() && key.style == FontStyle::default() && key.cells == 1 =>
            {
                self.ascii.get_mut(character as usize)
            }
            _ => None,
        };
        if let Some(Some(glyph)) = ascii {
            return Some(*glyph);
        }
        match self.slots.get(&key) {
            Some(AtlasSlot::Ready(slot)) => {
                if let Some(cached) = ascii {
                    *cached = *slot;
                }
                return *slot;
            }
            Some(AtlasSlot::Pending) => {}
            None => {
                let id = self.next_job;
                self.next_job += 1;
                let job = Job {
                    id,
                    text: JobText::from(&key.text),
                    style: key.style,
                    cells: key.cells,
                };
                if jobs.send(job).log_err("queue glyph").is_some() {
                    self.slots.insert(key.clone(), AtlasSlot::Pending);
                    self.requested.insert(id, key);
                }
            }
        }
        self.missing = true;
        None
    }

    /// Uploads every finished glyph, waiting as `wait` says for the rest. Returns whether any landed.
    fn receive(
        &mut self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        font: &Font,
        rasterizer: &Rasterizer,
        wait: Wait,
    ) -> bool {
        rasterizer.woken.store(false, Ordering::Release);
        let mut landed = false;
        loop {
            let result = match wait {
                _ if self.requested.is_empty() => break,
                Wait::Ready => rasterizer.done.try_recv().ok(),
                Wait::Until(deadline) => rasterizer.done.recv_deadline(deadline).ok(),
                Wait::All => rasterizer.done.recv().ok(),
            };
            let Some(result) = result else {
                break;
            };
            landed |= self.place(queue, texture, font, result);
        }
        landed
    }

    fn place(
        &mut self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        font: &Font,
        result: Rasterized,
    ) -> bool {
        let Some(key) = self.requested.remove(&result.id) else {
            return false;
        };
        let (width, height) = (font.cell_width * key.cells, font.cell_height);
        if self.next[0] + width + 1 > ATLAS_SIZE {
            self.next = [1, self.next[1] + height + 1];
        }
        if self.next[1] + height + 1 > ATLAS_SIZE {
            self.reset();
        }
        let slot = result.kind.map(|kind| AtlasGlyph {
            position: self.next,
            kind,
        });
        if let Some(AtlasGlyph {
            position: [x, y], ..
        }) = slot
        {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &result.bitmap,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            self.next[0] += width + 1;
        }
        self.slots.insert(key, AtlasSlot::Ready(slot));
        true
    }
}

fn glyph_key(grid: &Grid, index: usize, cell: &Cell) -> Option<GlyphKey> {
    let text = match grid.cluster(cell) {
        Some(cluster) => GlyphText::Cluster(cluster.clone()),
        None if cell.spacer || cell.character == ' ' => return None,
        None => GlyphText::Character(cell.character),
    };
    Some(GlyphKey {
        text,
        style: FontStyle {
            bold: cell.bold,
            italic: cell.italic,
        },
        cells: if cell.wide || spills_into_next(grid, index, cell) {
            2
        } else {
            1
        },
    })
}

/// Ghostty's `constraintWidth`: a symbol may use the next cell when that is blank, unless the row
/// ends there or a symbol other than a graphics element precedes, so that a run of icons stays
/// aligned. A rule capped at one cell, as Powerline's is, keeps its one-cell key: Ghostty clamps
/// the width there, and agentd draws its sprites only in a single cell.
fn spills_into_next(grid: &Grid, index: usize, cell: &Cell) -> bool {
    let cols = usize::from(grid.cols).max(1);
    cell.cluster.is_none()
        && is_symbol(cell.character)
        && Constraint::of(cell.character)
            .is_some_and(|constraint| constraint.max_constraint_width > 1)
        && !(index + 1).is_multiple_of(cols)
        && grid
            .cells
            .get(index + 1)
            .is_some_and(|next| matches!(next.character, ' ' | '\u{2002}'))
        && (index.is_multiple_of(cols) || {
            let previous = grid.cells[index - 1].character;
            !is_symbol(previous) || is_graphics_element(previous)
        })
}

/// A padlock filling most of the cell: a body with a shackle whose posts run into it.
fn lock_edges(left: f32, top: f32, width: f32, height: f32, thickness: f32) -> [[f32; 4]; 4] {
    let stroke = (width * 0.14).max(thickness);
    let body_width = width * 0.8;
    let body_left = left + (width - body_width) / 2.0;
    let body_top = top + height * 0.45;
    let shackle_width = body_width * 0.66;
    let shackle_left = left + (width - shackle_width) / 2.0;
    let shackle_top = top + height * 0.05;
    let post = body_top - shackle_top;
    [
        [
            body_left,
            body_top,
            body_width,
            top + height * 0.95 - body_top,
        ],
        [shackle_left, shackle_top, shackle_width, stroke],
        [shackle_left, shackle_top, stroke, post],
        [
            shackle_left + shackle_width - stroke,
            shackle_top,
            stroke,
            post,
        ],
    ]
}

/// Neighbouring cells shaped together: the same style, on one row, identical but for the background (selection included), and
/// not the cursor, which splits a ligature as Ghostty does.
struct Run {
    start: usize,
    end: usize,
    /// `None` for a lone cell, which has nothing to shape with.
    shaped: Option<Rc<ShapedRun>>,
}

fn joins_run(cell: &Cell, font: &Font) -> bool {
    cell.cluster.is_none() && !cell.wide && !cell.spacer && font.joins_run(cell.character)
}

/// The end of the row `start` is on, without its trailing blanks: runs never reach past it.
fn text_end(grid: &Grid, start: usize) -> usize {
    let cols = usize::from(grid.cols).max(1);
    let row_end = ((start / cols + 1) * cols).min(grid.cells.len());
    let blanks = grid.cells[start..row_end]
        .iter()
        .rev()
        .take_while(|cell| cell.character == ' ' && cell.cluster.is_none())
        .count();
    row_end - blanks
}

fn find_run(
    grid: &Grid,
    start: usize,
    mut limit: usize,
    cursor: Option<usize>,
    font: &Font,
    text: &mut String,
) -> Run {
    let first = &grid.cells[start];
    if let Some(cursor) = cursor.filter(|&cursor| cursor > start && cursor < limit) {
        limit = cursor;
    }
    let mut end = grid.cells[start..limit]
        .windows(2)
        .position(|pair| {
            !joins_run(&pair[1], font)
                || !pair[0].shapes_with(&pair[1])
                // Ghostty never ligates these, whatever the font says.
                || matches!(
                    (pair[0].character, pair[1].character),
                    ('f', 'l' | 'i') | ('s', 't')
                )
        })
        .map_or(limit, |offset| start + 1 + offset);
    // Trailing blanks add nothing to shape, and would give every line its own cache entry.
    while end - start > 1 && grid.cells[end - 1].character == ' ' {
        end -= 1;
    }
    if end - start > 1 {
        text.clear();
        text.extend(grid.cells[start..end].iter().map(|cell| cell.character));
    }
    let style = FontStyle {
        bold: first.bold,
        italic: first.italic,
    };
    Run {
        start,
        end,
        shaped: (end - start > 1).then(|| font.shaped_cells(text, style)),
    }
}

/// The quad for `placement`, clipped to `area`, the cells' rectangle, so an image that starts
/// beyond the viewport doesn't spill into the padding.
fn image_quad(
    placement: &Placement,
    origin: (f32, f32),
    metrics: Metrics,
    area: [f32; 4],
    srgb_blend: bool,
) -> Option<Quad> {
    let [width, height] = placement.size.map(|pixels| pixels as f32);
    let left = origin.0
        + placement.col as f32 * f32::from(metrics.cell_width)
        + placement.offset[0] as f32;
    let top = origin.1
        + placement.row as f32 * f32::from(metrics.cell_height)
        + placement.offset[1] as f32;
    let (x0, y0) = (left.max(area[0]), top.max(area[1]));
    let (x1, y1) = (
        (left + width).min(area[0] + area[2]),
        (top + height).min(area[1] + area[3]),
    );
    if width <= 0.0 || height <= 0.0 || x1 <= x0 || y1 <= y0 {
        return None;
    }
    let [source_x, source_y, source_width, source_height] =
        placement.source.map(|pixels| pixels as f32);
    let (image_width, image_height) = (placement.image.width as f32, placement.image.height as f32);
    Some(Quad {
        rect: [x0, y0, x1 - x0, y1 - y0],
        uv: [
            (source_x + (x0 - left) / width * source_width) / image_width,
            (source_y + (y0 - top) / height * source_height) / image_height,
            (x1 - x0) / width * source_width / image_width,
            (y1 - y0) / height * source_height / image_height,
        ],
        color: [0.0; 3],
        mode: if srgb_blend {
            MODE_IMAGE
        } else {
            MODE_IMAGE_LINEAR
        },
    })
}

fn solid(rect: [f32; 4], fill: [u8; 3], srgb_blend: bool) -> Quad {
    Quad {
        rect,
        uv: [0.0; 4],
        color: color(fill, srgb_blend),
        mode: MODE_SOLID,
    }
}

fn build_quads(
    grid: &Grid,
    rect: [f32; 4],
    metrics: Metrics,
    font: &Font,
    srgb_blend: bool,
    quads: &mut Vec<Quad>,
    mut glyph: impl FnMut(GlyphKey) -> (Option<AtlasGlyph>, u64),
) -> usize {
    let solid = |rect, fill| solid(rect, fill, srgb_blend);
    let (width, height) = (
        f32::from(metrics.cell_width),
        f32::from(metrics.cell_height),
    );
    let origin = metrics.origin(rect);
    let cols = usize::from(grid.cols);
    let cell_rect =
        |index: usize| metrics.cell_rect(origin, (index % cols) as u16, (index / cols) as u16);

    quads.clear();
    quads.push(solid(rect, grid.bg));
    for (index, cell) in grid.cells.iter().enumerate() {
        if let Some(cell_bg) = cell.bg {
            quads.push(solid(cell_rect(index), cell_bg));
        }
    }
    let cursor = grid
        .cursor
        .map(|(x, y)| usize::from(y) * cols + usize::from(x));
    let thickness = font.line_thickness as f32;
    if let Some(index) = cursor {
        let [left, top, ..] = cell_rect(index);
        let wide = grid.cells.get(index).is_some_and(|cell| cell.wide);
        let cursor_width = if wide { 2.0 * width } else { width };
        let edges: &[[f32; 4]] = match grid.cursor_style {
            _ if grid.password_input => &lock_edges(left, top, cursor_width, height, thickness),
            CursorVisualStyle::Bar => &[[left, top, thickness, height]],
            CursorVisualStyle::Underline => &[[
                left,
                top + font.underline_top as f32,
                cursor_width,
                thickness,
            ]],
            CursorVisualStyle::BlockHollow => &[
                [left, top, cursor_width, thickness],
                [left, top + height - thickness, cursor_width, thickness],
                [left, top, thickness, height],
                [left + cursor_width - thickness, top, thickness, height],
            ],
            _ => &[[left, top, cursor_width, height]],
        };
        for edge in edges {
            quads.push(solid(*edge, grid.cursor_color));
        }
    }
    let run_cursor = cursor.filter(|_| font.break_runs_at_cursor);
    let inverted =
        cursor.filter(|_| grid.cursor_style == CursorVisualStyle::Block && !grid.password_input);
    let glyphs_start = quads.len();
    let underline_top = font.underline_top as f32;
    let textured = |found: AtlasGlyph, left: f32, top: f32, cells: f32, fg: [u8; 3]| {
        let [x, y] = found.position;
        Quad {
            rect: [left, top, width * cells, height],
            uv: [
                x as f32 / ATLAS_SIZE as f32,
                y as f32 / ATLAS_SIZE as f32,
                width * cells / ATLAS_SIZE as f32,
                height / ATLAS_SIZE as f32,
            ],
            color: color(fg, srgb_blend),
            mode: match (found.kind, srgb_blend) {
                (GlyphKind::Coverage, _) => MODE_COVERAGE,
                (GlyphKind::Color, true) => MODE_COLOR,
                (GlyphKind::Color, false) => MODE_COLOR_LINEAR,
            },
        }
    };
    // A reset wipes slots already handed out, so redo the glyphs once against the emptied atlas.
    for _ in 0..2 {
        quads.truncate(glyphs_start);
        let mut first_epoch = None;
        let mut reset = false;
        let mut run: Option<Run> = None;
        let mut run_text = String::new();
        let mut row_text_end = (usize::MAX, 0);
        let mut lookup = |key| {
            let (slot, epoch) = glyph(key);
            reset |= *first_epoch.get_or_insert(epoch) != epoch;
            slot
        };
        for (index, cell) in grid.cells.iter().enumerate() {
            let [left, top, ..] = cell_rect(index);
            let fg = if inverted == Some(index) {
                cell.bg.unwrap_or(grid.bg)
            } else {
                cell.fg
            };
            let underline_fill = cell.underline_color.unwrap_or(fg);
            let line = |y: f32, fill| solid([left, top + y, width, thickness], fill);
            match cell.underline {
                Underline::Single => quads.push(line(underline_top, underline_fill)),
                Underline::Double => {
                    quads.push(line(underline_top, underline_fill));
                    quads.push(line(underline_top + 2.0 * thickness, underline_fill));
                }
                kind @ (Underline::Curly | Underline::Dotted | Underline::Dashed) => {
                    let key = GlyphKey {
                        text: GlyphText::Underline(kind),
                        style: FontStyle::default(),
                        cells: 1,
                    };
                    if let Some(found) = lookup(key) {
                        quads.push(textured(found, left, top, 1.0, underline_fill));
                    }
                }
                _ => {}
            }
            if cell.strikethrough {
                quads.push(line(font.strike_top as f32, fg));
            }
            if cell.overline {
                quads.push(line(0.0, fg));
            }
            if run.as_ref().is_none_or(|run| index >= run.end)
                && Some(index) != run_cursor
                && cell.character != ' '
                && joins_run(cell, font)
            {
                let cols = usize::from(grid.cols).max(1);
                if row_text_end.0 != index / cols {
                    row_text_end = (index / cols, text_end(grid, index));
                }
                run = Some(find_run(
                    grid,
                    index,
                    row_text_end.1,
                    run_cursor,
                    font,
                    &mut run_text,
                ));
            }
            let shaped = run
                .as_ref()
                .filter(|run| (run.start..run.end).contains(&index))
                .and_then(|run| {
                    let shaped = run.shaped.as_ref()?;
                    let signature = shaped.signature(index - run.start)?;
                    Some(GlyphKey {
                        text: GlyphText::Shaped(ShapedCell {
                            run: shaped.text.clone(),
                            index: (index - run.start) as u16,
                            signature,
                        }),
                        style: FontStyle {
                            bold: cell.bold,
                            italic: cell.italic,
                        },
                        cells: 1,
                    })
                });
            let Some(key) = shaped.or_else(|| glyph_key(grid, index, cell)) else {
                continue;
            };
            let cells = key.cells as f32;
            if let Some(found) = lookup(key) {
                quads.push(textured(found, left, top, cells, fg));
            }
        }
        if !reset {
            break;
        }
    }
    glyphs_start
}

/// Image textures by `(id, generation)`, uploaded once and shared by every slot, so a keystroke's
/// rebuild doesn't upload again. A texture no frame used is dropped.
struct ImageTextures {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cache: HashMap<(u32, u64), (wgpu::BindGroup, u64)>,
    frame: u64,
}

impl ImageTextures {
    fn new(device: &wgpu::Device) -> Self {
        let entry = |binding, visibility, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty,
            count: None,
        };
        Self {
            layout: device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("terminal images"),
                entries: &[
                    entry(
                        0,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                    ),
                    entry(
                        1,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    ),
                ],
            }),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            cache: HashMap::new(),
            frame: 0,
        }
    }

    /// `None` for an image too large for the device, which is then not drawn.
    fn bind_group(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &Image,
    ) -> Option<wgpu::BindGroup> {
        let key = (image.id, image.generation);
        if let Some((bind_group, used)) = self.cache.get_mut(&key) {
            *used = self.frame;
            return Some(bind_group.clone());
        }
        if image.width.max(image.height) > device.limits().max_texture_dimension_2d {
            eprintln!(
                "agentd: image {}x{} is too large to draw",
                image.width, image.height
            );
            return None;
        }
        let size = wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        // Not an sRGB format: filtering and blending stay in the encoded space, as the glyph atlas does.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terminal image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &image.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * 4),
                rows_per_image: Some(image.height),
            },
            size,
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &texture.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.cache.insert(key, (bind_group.clone(), self.frame));
        Some(bind_group)
    }

    /// Keeps a texture a slot still draws from an earlier build.
    fn touch(&mut self, key: (u32, u64)) {
        if let Some((_, used)) = self.cache.get_mut(&key) {
            *used = self.frame;
        }
    }

    /// Ends a frame, dropping the textures it didn't use.
    fn sweep(&mut self) {
        let frame = self.frame;
        self.cache.retain(|_, (_, used)| *used == frame);
        self.frame += 1;
    }
}

struct ImageDraw {
    layer: ImageLayer,
    key: (u32, u64),
    bind_group: wgpu::BindGroup,
}

/// The thumb, centred in the right padding beside the rows the grid fills, as the pointer finds it there.
fn scrollbar_thumb(grid: &Grid, rect: [f32; 4], metrics: Metrics) -> Option<[f32; 4]> {
    let scrollbar = grid.scrollbar.filter(|scrollbar| scrollbar.opacity > 0.0)?;
    let rows = grid.cells.len() / usize::from(grid.cols).max(1);
    let (track, min) = Scrollbar::track(rows, f32::from(metrics.cell_height), metrics.scale);
    let (top, height) = scrollbar.thumb(track, min);
    let points = if scrollbar.wide {
        WIDE_SCROLLBAR_POINTS
    } else {
        SCROLLBAR_POINTS
    };
    let width = (points * metrics.scale).round();
    let left = (rect[0] + rect[2] - (metrics.padding + width) / 2.0).round();
    Some([left, metrics.origin(rect).1 + top, width, height])
}

/// Two fifths of the way from the background toward white or black, whichever contrasts,
/// scaled by `opacity`. The thumb lies on the plain background, so this fades it with no blending.
fn thumb_color(bg: [u8; 3], opacity: f32) -> [u8; 3] {
    let dark = bg.iter().map(|&channel| u16::from(channel)).sum::<u16>() < 3 * 128;
    let toward = if dark { 255.0 } else { 0.0 };
    bg.map(|channel| {
        let channel = f32::from(channel);
        (channel + (toward - channel) * 0.4 * opacity).round() as u8
    })
}

struct Slot {
    quads: wgpu::Buffer,
    scratch: Vec<Quad>,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Session id, generation, rect, atlas epoch and blend mode; generations restart per session.
    built: Option<(u64, u64, [f32; 4], u64, bool)>,
    /// Where each part of `scratch` ends: the cell backgrounds and cursor, the text and its
    /// decorations, and the scrollbar. The background fill is the first quad, and the images the rest.
    bounds: [u32; 3],
    /// In drawing order; draw `i` is the quad at `bounds[2] + i`.
    images: Vec<ImageDraw>,
}

struct Gpu {
    key: DeviceKey,
    layout: wgpu::BindGroupLayout,
    module: wgpu::ShaderModule,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    atlas: Atlas,
    /// By target format: the cells' pipeline, then the images'.
    pipelines: Vec<(
        wgpu::TextureFormat,
        wgpu::RenderPipeline,
        wgpu::RenderPipeline,
    )>,
    images: ImageTextures,
    slots: HashMap<u32, Slot>,
}

impl Gpu {
    fn new(device: &wgpu::Device, key: DeviceKey) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("terminal atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(
                    2,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        Self {
            key,
            layout,
            module: device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("terminal cells"),
                source: wgpu::ShaderSource::Wgsl(include_str!("cell.wgsl").into()),
            }),
            view: texture.create_view(&Default::default()),
            texture,
            sampler: device.create_sampler(&Default::default()),
            atlas: Atlas::new(0),
            pipelines: Vec::new(),
            images: ImageTextures::new(device),
            slots: HashMap::new(),
        }
    }

    fn ensure_pipeline(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if self.pipelines.iter().any(|(built, ..)| *built == format) {
            return;
        }
        let build = |groups: &[Option<&wgpu::BindGroupLayout>], fragment: &str| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: groups,
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("terminal cells"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &self.module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: size_of::<Quad>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x3, 3 => Uint32],
                    })],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.module,
                    entry_point: Some(fragment),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let cells = build(&[Some(&self.layout)], "fs");
        let images = build(&[Some(&self.layout), Some(&self.images.layout)], "fs_image");
        self.pipelines.push((format, cells, images));
    }
}

fn new_slot(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> Slot {
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    Slot {
        quads: quad_buffer(device, 4096 * size_of::<Quad>() as u64),
        scratch: Vec::new(),
        uniforms,
        bind_group,
        built: None,
        bounds: [0; 3],
        images: Vec::new(),
    }
}

fn quad_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("terminal quads"),
        size,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// What the compositor reads from the app.
pub trait TerminalSource: 'static {
    /// The configured font and the size to draw it at, which zoom changes.
    fn font(&self) -> (&crate::config::FontConfig, f64);
    /// Resizes every session sharing the area of effect `mark` to `surface`.
    fn resize(&self, mark: u32, surface: Surface);
    /// The session to draw in the area of effect `mark`.
    fn drawn_session(&self, mark: u32) -> Option<&Session>;
    /// Wakes the window this compositor draws, for glyphs that finish after their frame.
    fn waker(&self) -> Waker;
}

struct LoadedFont {
    scale: f32,
    size: f64,
    config: crate::config::FontConfig,
    font: Font,
    rasterizer: Rasterizer,
}

#[derive(Default)]
pub struct TerminalCompositor {
    gpu: Option<Gpu>,
    font: Option<LoadedFont>,
}

impl TerminalCompositor {
    fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: &impl TerminalSource,
        pass: &mut wgpu::RenderPass<'_>,
        site: &EffectSite<'_>,
    ) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let (config, size) = source.font();
        if self.font.as_ref().is_none_or(|loaded| {
            (loaded.scale, loaded.size, &loaded.config) != (site.scale, size, config)
        }) {
            match load_font(config, size, site.scale) {
                Ok(font) => {
                    // Glyphs still out with the old rasterizer never come back, so none stay pending.
                    gpu.atlas = Atlas::new(gpu.atlas.epoch + 1);
                    self.font = Some(LoadedFont {
                        scale: site.scale,
                        size,
                        config: config.clone(),
                        font,
                        rasterizer: Rasterizer::new(config, size, site.scale, source.waker()),
                    });
                }
                Err(error) => {
                    eprintln!("agentd: load font: {error:#}");
                    return;
                }
            }
        }
        let Some(LoadedFont {
            font, rasterizer, ..
        }) = &self.font
        else {
            return;
        };
        gpu.atlas
            .receive(queue, &gpu.texture, font, rasterizer, Wait::Ready);
        gpu.ensure_pipeline(device, site.format);
        let slot = gpu
            .slots
            .entry(site.mark)
            .or_insert_with(|| new_slot(device, &gpu.layout, &gpu.view, &gpu.sampler));

        let rect = site.instance.rect;
        let metrics = Metrics {
            cell_width: font.cell_width as u16,
            cell_height: font.cell_height as u16,
            padding: PADDING_POINTS * site.scale,
            scale: site.scale,
        };
        let surface = Surface::fit((rect[2], rect[3]), metrics);
        source.resize(site.mark, surface);
        let session = source.drawn_session(site.mark);

        let grid = session.map_or_else(Default::default, Session::grid);
        let mut built = (
            session.map_or(0, Session::id),
            grid.generation,
            rect,
            gpu.atlas.epoch,
            site.srgb_blend,
        );
        if slot.built != Some(built) {
            let wait = if rune::is_headless() {
                Wait::All
            } else {
                Wait::Until(Instant::now() + GLYPH_WAIT)
            };
            let (atlas, texture) = (&mut gpu.atlas, &gpu.texture);
            let glyphs_start = loop {
                atlas.missing = false;
                let glyphs_start = build_quads(
                    &grid,
                    rect,
                    metrics,
                    font,
                    site.srgb_blend,
                    &mut slot.scratch,
                    |key| (atlas.glyph(&rasterizer.jobs, key), atlas.epoch),
                );
                // Glyphs keep landing while the workers run, so past the deadline the frame draws what it has.
                if !atlas.missing
                    || wait.expired()
                    || !atlas.receive(queue, texture, font, rasterizer, wait)
                {
                    break glyphs_start;
                }
            };
            let text_end = slot.scratch.len() as u32;
            if let Some(scrollbar) = grid.scrollbar
                && let Some(thumb) = scrollbar_thumb(&grid, rect, metrics)
            {
                slot.scratch.push(solid(
                    thumb,
                    thumb_color(grid.bg, scrollbar.opacity),
                    site.srgb_blend,
                ));
            }
            slot.bounds = [glyphs_start as u32, text_end, slot.scratch.len() as u32];
            let origin = metrics.origin(rect);
            let rows = grid.cells.len() / usize::from(grid.cols).max(1);
            let area = [
                origin.0,
                origin.1,
                f32::from(grid.cols) * f32::from(metrics.cell_width),
                rows as f32 * f32::from(metrics.cell_height),
            ];
            slot.images.clear();
            for placement in grid.images.iter() {
                if let Some(quad) = image_quad(placement, origin, metrics, area, site.srgb_blend)
                    && let Some(bind_group) = gpu.images.bind_group(device, queue, &placement.image)
                {
                    slot.scratch.push(quad);
                    slot.images.push(ImageDraw {
                        layer: placement.layer,
                        key: (placement.image.id, placement.image.generation),
                        bind_group,
                    });
                }
            }
            let bytes: &[u8] = cast_slice(&slot.scratch);
            if bytes.len() as u64 > slot.quads.size() {
                slot.quads = quad_buffer(device, (bytes.len() as u64).next_power_of_two());
            }
            queue.write_buffer(&slot.quads, 0, bytes);
            // Left unbuilt while glyphs are missing, so the frame their arrival wakes redraws it.
            built.3 = atlas.epoch;
            slot.built = (!atlas.missing).then_some(built);
        }

        let frame = [
            site.origin[0],
            site.origin[1],
            site.size[0] as f32,
            site.size[1] as f32,
        ];
        queue.write_buffer(&slot.uniforms, 0, cast_slice(&frame));
        for draw in &slot.images {
            gpu.images.touch(draw.key);
        }
        let Some((_, cells, images)) = gpu
            .pipelines
            .iter()
            .find(|(built, ..)| *built == site.format)
        else {
            return;
        };
        pass.set_vertex_buffer(0, slot.quads.slice(..));
        let draw_cells = |pass: &mut wgpu::RenderPass<'_>, range: Range<u32>| {
            if !range.is_empty() {
                pass.set_pipeline(cells);
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.draw(0..4, range);
            }
        };
        let draw_images = |pass: &mut wgpu::RenderPass<'_>, layer: ImageLayer| {
            pass.set_pipeline(images);
            pass.set_bind_group(0, &slot.bind_group, &[]);
            for (offset, draw) in slot.images.iter().enumerate() {
                if draw.layer == layer {
                    pass.set_bind_group(1, &draw.bind_group, &[]);
                    let instance = slot.bounds[2] + offset as u32;
                    pass.draw(0..4, instance..instance + 1);
                }
            }
        };
        // Images below the backgrounds still lie over the fill, which is the first quad.
        let [glyphs_start, text_end, chrome_end] = slot.bounds;
        draw_cells(pass, 0..1.min(glyphs_start));
        draw_images(pass, ImageLayer::BelowBackground);
        draw_cells(pass, 1.min(glyphs_start)..glyphs_start);
        draw_images(pass, ImageLayer::BelowText);
        draw_cells(pass, glyphs_start..text_end);
        draw_images(pass, ImageLayer::AboveText);
        draw_cells(pass, text_end..chrome_end);
    }
}

impl<S: TerminalSource> Compositor<S> for TerminalCompositor {
    fn render(&mut self, frame: &mut Frame<'_, S>) {
        let key = frame.device_key();
        if self.gpu.as_ref().is_none_or(|gpu| gpu.key != key) {
            self.gpu = Some(Gpu::new(frame.device, key));
        }
        let (device, queue, size, clear) = (
            frame.device,
            frame.queue,
            frame.size,
            frame.rt.clear_color(),
        );
        frame.ui.prepare(
            device,
            queue,
            &mut frame.rt.text.atlas,
            &frame.rt.instances,
            size,
        );
        let source = &frame.rt.state;
        frame
            .ui
            .draw_with(frame.encoder, frame.target, clear, &mut |pass, site| {
                self.draw(device, queue, source, pass, site)
            });
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.images.sweep();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn an_unseen_glyph_is_queued_once_and_reported_missing() {
        let (jobs, queued) = crossbeam_channel::unbounded();
        let mut atlas = Atlas::new(0);
        let key = GlyphKey {
            text: GlyphText::Character('世'),
            style: FontStyle::default(),
            cells: 2,
        };
        assert!(atlas.glyph(&jobs, key.clone()).is_none());
        assert!(atlas.glyph(&jobs, key).is_none());
        assert!(atlas.missing);
        assert_eq!(queued.len(), 1);
    }

    #[test]
    fn a_reset_forgets_ready_ascii_glyphs() {
        let (jobs, queued) = crossbeam_channel::unbounded();
        let mut atlas = Atlas::new(0);
        let key = GlyphKey {
            text: GlyphText::Character('a'),
            style: FontStyle::default(),
            cells: 1,
        };
        atlas
            .slots
            .insert(key.clone(), AtlasSlot::Ready(Some(fake_glyph([16, 0]))));
        assert!(atlas.glyph(&jobs, key.clone()).is_some());
        atlas.reset();
        assert!(atlas.glyph(&jobs, key).is_none());
        assert_eq!(queued.len(), 1);
    }

    #[test]
    fn a_cluster_finds_its_ready_glyph_in_a_later_grid() {
        let (jobs, queued) = crossbeam_channel::unbounded();
        let mut atlas = Atlas::new(0);
        let first = Grid::from_vt("e\u{301}".as_bytes(), 10, 1).unwrap();
        let key = glyph_key(&first, 0, &first.cells[0]).unwrap();
        assert!(atlas.glyph(&jobs, key).is_none());
        for slot in atlas.slots.values_mut() {
            *slot = AtlasSlot::Ready(Some(fake_glyph([16, 0])));
        }
        let second = Grid::from_vt("e\u{301}".as_bytes(), 10, 1).unwrap();
        let key = glyph_key(&second, 0, &second.cells[0]).unwrap();
        assert!(atlas.glyph(&jobs, key).is_some());
        assert_eq!(queued.len(), 1);
    }

    #[test]
    fn a_worker_rasterises_as_the_ui_thread_would() {
        let config = crate::config::Config::default().font;
        let rasterizer = Rasterizer::new(&config, 18.0, 2.0, Arc::new(|| {}));
        let font = load_font(&config, 18.0, 2.0).unwrap();
        for (id, text, cells) in [
            (1, JobText::Character('a'), 1),
            (2, JobText::Character('世'), 2),
        ] {
            let mut expected = Vec::new();
            let expected_kind = rasterize(&font, &text, FontStyle::default(), cells, &mut expected);
            rasterizer
                .jobs
                .send(Job {
                    id,
                    text,
                    style: FontStyle::default(),
                    cells,
                })
                .unwrap();
            let done = rasterizer.done.recv().unwrap();
            assert_eq!((done.id, done.kind), (id, expected_kind));
            assert!(done.bitmap == expected);
        }
    }

    fn fake_glyph(position: [u32; 2]) -> AtlasGlyph {
        AtlasGlyph {
            position,
            kind: GlyphKind::Coverage,
        }
    }

    fn metrics(font: &Font, padding: f32, scale: f32) -> Metrics {
        Metrics {
            cell_width: font.cell_width as u16,
            cell_height: font.cell_height as u16,
            padding,
            scale,
        }
    }

    fn quads_for(grid: &Grid, scale: f32) -> (Vec<Quad>, Font) {
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, scale).unwrap();
        let mut quads = Vec::new();
        build_quads(
            grid,
            [0.0, 0.0, 800.0, 600.0],
            metrics(&font, 0.0, scale),
            &font,
            true,
            &mut quads,
            |_| (Some(fake_glyph([0, 0])), 0),
        );
        (quads, font)
    }

    /// `cargo test --release -- --ignored rebuild_cost --nocapture`
    #[test]
    #[ignore]
    fn rebuild_cost() {
        let (cols, rows) = (250u16, 70u16);
        let mut bytes = Vec::new();
        for row in 0..rows {
            bytes.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            let line = format!(
                "\x1b[3{}m-rw-r--r-- \x1b[1;3{}m{:>4}\x1b[0m garth staff \x1b[4{};30m{:>8}\x1b[0m Nov 12 \x1b[4mfile_{row}.rs\x1b[0m != -> => src/render.rs fn build_quads(grid: &Grid) {{ let x = a >= b; }} // 0123456789 \x1b[7mrev\x1b[0m",
                row % 7 + 1,
                row % 6 + 1,
                row,
                row % 6 + 1,
                row * 977,
            );
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(line.as_bytes());
        }
        let grid = Grid::from_vt(&bytes, cols, rows).unwrap();
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 2.0).unwrap();
        let metrics = metrics(&font, 0.0, 2.0);
        let mut atlas = HashMap::new();
        let mut quads = Vec::new();
        let mut run = |quads: &mut Vec<Quad>| {
            build_quads(
                &grid,
                [0.0, 0.0, 4000.0, 2000.0],
                metrics,
                &font,
                true,
                quads,
                |key| {
                    let next = atlas.len() as u32;
                    let glyph = *atlas
                        .entry(key)
                        .or_insert_with(|| fake_glyph([next % 64, next / 64]));
                    (Some(glyph), 0)
                },
            );
        };
        run(&mut quads);
        let iterations = 200;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            run(&mut quads);
        }
        let per = start.elapsed() / iterations;
        println!("{cols}x{rows} full rebuild: {per:?}, {} quads", quads.len());
    }

    /// The run cells `build_quads` asks the atlas for, as `(run, index)`.
    fn shaped_keys(bytes: &[u8], features: &[&str]) -> Vec<(String, u16)> {
        shaped_keys_in(bytes, 8, features)
    }

    fn shaped_keys_in(bytes: &[u8], cols: u16, features: &[&str]) -> Vec<(String, u16)> {
        shaped_keys_with(bytes, cols, features, true)
    }

    fn shaped_keys_with(
        bytes: &[u8],
        cols: u16,
        features: &[&str],
        break_at_cursor: bool,
    ) -> Vec<(String, u16)> {
        let features: Vec<String> = features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect();
        let mut font = Font::with_symbols(
            crate::config::DEFAULT_FONT_FAMILY,
            18.0,
            2.0,
            None,
            &[],
            &features,
        )
        .unwrap();
        font.break_runs_at_cursor = break_at_cursor;
        let grid = Grid::from_vt(bytes, cols, 2).unwrap();
        let mut keys = Vec::new();
        build_quads(
            &grid,
            [0.0, 0.0, 800.0, 600.0],
            metrics(&font, 0.0, 2.0),
            &font,
            true,
            &mut Vec::new(),
            |key| {
                if let GlyphText::Shaped(cell) = key.text {
                    keys.push((cell.run.to_string(), cell.index));
                }
                (Some(fake_glyph([0, 0])), 0)
            },
        );
        keys
    }

    #[test]
    fn a_ligature_asks_for_its_cells_shaped_with_their_neighbours() {
        let keys = shaped_keys(b"a != b\x1b[2;1H", &[]);
        assert_eq!(keys, [("a != b".to_owned(), 2), ("a != b".to_owned(), 3)]);
        assert!(shaped_keys(b"a != b\x1b[2;1H", &["-liga"]).is_empty());
    }

    #[test]
    fn the_cursor_breaks_a_ligature() {
        assert!(shaped_keys(b"a != b\x1b[1;3H", &[]).is_empty());
        assert!(shaped_keys(b"a != b\x1b[1;4H", &[]).is_empty());
    }

    #[test]
    fn a_ligature_inside_a_long_token_still_shapes() {
        // The cells at 15 and 16 once fell either side of a fixed chunk boundary.
        let token = b"abcdeghjklmnoqr!=uvwxyz";
        let keys = shaped_keys_in(token, 40, &[]);
        let run = String::from_utf8_lossy(token).into_owned();
        assert!(keys.contains(&(run.clone(), 15)) && keys.contains(&(run, 16)));
    }

    #[test]
    fn non_ascii_cells_join_a_run_and_are_indexed_by_cell() {
        let keys = shaped_keys("→é!=".as_bytes(), &[]);
        for index in [2, 3] {
            assert!(keys.contains(&("→é!=".to_owned(), index)), "{keys:?}");
        }
    }

    #[test]
    fn a_style_change_breaks_a_ligature_but_a_background_change_does_not() {
        assert!(shaped_keys(b"\x1b[31m!\x1b[32m=\x1b[0m\x1b[2;1H", &[]).is_empty());
        assert!(shaped_keys(b"\x1b[4m!\x1b[4:3m=\x1b[0m\x1b[2;1H", &[]).is_empty());
        assert!(shaped_keys(b"\x1b[9m!\x1b[0m=\x1b[2;1H", &[]).is_empty());
        assert_eq!(
            shaped_keys(b"\x1b[41m!\x1b[42m=\x1b[0m\x1b[2;1H", &[]).len(),
            2
        );
    }

    #[test]
    fn fl_fi_and_st_are_never_shaped_together() {
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 2.0).unwrap();
        let grid = Grid::from_vt(b"flfist", 8, 1).unwrap();
        let mut text = String::new();
        let ends: Vec<usize> = (0..6)
            .step_by(2)
            .map(|start| find_run(&grid, start, text_end(&grid, start), None, &font, &mut text).end)
            .collect();
        assert_eq!(ends, [1, 3, 5]);
    }

    #[test]
    fn a_constrained_glyph_spills_only_into_a_blank_cell_after_a_non_icon() {
        let widths = |text: &str| -> Vec<u32> {
            let grid = Grid::from_vt(text.as_bytes(), 4, 1).unwrap();
            (0..4)
                .filter_map(|index| {
                    glyph_key(&grid, index, &grid.cells[index]).map(|key| key.cells)
                })
                .collect()
        };
        assert_eq!(widths("a\u{E0C6} "), [1, 2]);
        assert_eq!(widths("a\u{E0C6}b"), [1, 1, 1]);
        assert_eq!(widths("\u{F432}\u{E0C6} "), [1, 1]);
        assert_eq!(widths("abc\u{E0C6}"), [1, 1, 1, 1]);
        assert_eq!(widths("\u{E0B0} "), [1]);
        assert_eq!(widths("\u{F418} "), [2]);
        assert_eq!(widths("\u{F418}b"), [1, 1]);
        assert_eq!(widths("\u{F418}\u{F07B} "), [1, 1]);
        assert_eq!(widths("\u{4E16} "), [2]);
    }

    #[test]
    fn a_symbol_spills_as_ghostty_does() {
        let widths = |text: &str| -> Vec<u32> {
            let grid = Grid::from_vt(text.as_bytes(), 4, 1).unwrap();
            (0..4)
                .filter_map(|index| {
                    glyph_key(&grid, index, &grid.cells[index]).map(|key| key.cells)
                })
                .collect()
        };
        assert_eq!(widths("\u{2192} "), [2]);
        assert_eq!(widths("a\u{2605} "), [1, 2]);
        assert_eq!(widths("\u{2713}b"), [1, 1]);
        assert_eq!(widths("\u{2192}\u{2192} "), [1, 1]);
        assert_eq!(widths("\u{2192}\u{F418} "), [1, 1]);
        assert_eq!(widths("\u{F418}\u{2192} "), [1, 1]);
        assert_eq!(widths("abc\u{2605}"), [1, 1, 1, 1]);
        assert_eq!(widths("\u{2500}\u{2192} "), [1, 2]);
        assert_eq!(widths("\u{E0B0}\u{2192} "), [1, 2]);
        assert_eq!(widths("\u{2500} "), [1]);
        assert_eq!(widths("\u{23FB} "), [1]);
        assert_eq!(widths("\u{1F600} "), [2]);
    }

    fn run_end(bytes: &[u8], start: usize) -> usize {
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 2.0).unwrap();
        let grid = Grid::from_vt(bytes, 10, 1).unwrap();
        find_run(
            &grid,
            start,
            text_end(&grid, start),
            None,
            &font,
            &mut String::new(),
        )
        .end
    }

    #[test]
    fn a_run_spans_spaces_but_drops_the_trailing_ones() {
        assert_eq!(run_end(b"ab cd  ef", 0), 9);
        assert_eq!(run_end(b"ab cd   ", 0), 5);
        assert_eq!(run_end(b"ab", 0), 2);
    }

    #[test]
    fn a_run_starts_at_its_first_character_so_indentation_doesnt_matter() {
        let keys = shaped_keys(b"  a != b\x1b[2;1H", &[]);
        assert_eq!(keys, [("a != b".to_owned(), 2), ("a != b".to_owned(), 3)]);
    }

    #[test]
    fn a_selection_edge_breaks_a_ligature() {
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 2.0).unwrap();
        let mut grid = Grid::from_vt(b"a != b\x1b[2;1H", 8, 2).unwrap();
        let mut cells = grid.cells.to_vec();
        cells[2].selected = true;
        grid.cells = cells.into();
        let mut keys = 0;
        build_quads(
            &grid,
            [0.0, 0.0, 800.0, 600.0],
            metrics(&font, 0.0, 2.0),
            &font,
            true,
            &mut Vec::new(),
            |key| {
                keys += usize::from(matches!(key.text, GlyphText::Shaped { .. }));
                (Some(fake_glyph([0, 0])), 0)
            },
        );
        assert_eq!(keys, 0);
    }

    #[test]
    fn without_shaping_break_the_cursor_leaves_a_ligature_whole() {
        let keys = shaped_keys_with(b"a != b\x1b[1;3H", 8, &[], false);
        assert_eq!(keys, [("a != b".to_owned(), 2), ("a != b".to_owned(), 3)]);
    }

    #[test]
    fn a_run_stops_at_the_end_of_a_row() {
        assert!(shaped_keys(b"      !=", &[]).is_empty());
    }

    #[test]
    fn a_wide_glyph_spans_two_cells_and_its_spacer_draws_nothing() {
        let grid = Grid::from_vt("世".as_bytes(), 4, 1).unwrap();
        let (quads, font) = quads_for(&grid, 1.0);
        let glyphs: Vec<&Quad> = quads
            .iter()
            .filter(|quad| quad.mode == MODE_COVERAGE)
            .collect();
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].rect[2], font.cell_width as f32 * 2.0);
    }

    fn built_quads(bytes: &[u8]) -> (Vec<Quad>, Font) {
        quads_for(&Grid::from_vt(bytes, 4, 1).unwrap(), 2.0)
    }

    fn solid_lines(quads: &[Quad], font: &Font) -> usize {
        quads
            .iter()
            .filter(|quad| quad.mode == MODE_SOLID && quad.rect[3] == font.line_thickness as f32)
            .count()
    }

    #[test]
    fn straight_decorations_are_solid_lines() {
        let (single, font) = built_quads(b"\x1b[4mx");
        assert_eq!(solid_lines(&single, &font), 1);
        let (double, font) = built_quads(b"\x1b[4:2mx");
        assert_eq!(solid_lines(&double, &font), 2);
        let (strike_and_over, font) = built_quads(b"\x1b[9;53mx");
        assert_eq!(solid_lines(&strike_and_over, &font), 2);
    }

    #[test]
    fn a_curly_underline_is_a_sprite_below_its_glyph() {
        let (quads, _) = built_quads(b"\x1b[4:3mx");
        let glyphs = quads
            .iter()
            .filter(|quad| quad.mode == MODE_COVERAGE)
            .count();
        assert_eq!(glyphs, 2);
    }

    #[test]
    fn a_blank_cell_keeps_its_underline() {
        let (quads, font) = built_quads(b"\x1b[4m ");
        assert_eq!(solid_lines(&quads, &font), 1);
    }

    fn cursor_quads(grid: &Grid) -> (Vec<Quad>, Font) {
        let (quads, font) = quads_for(grid, 2.0);
        // The first quad fills the terminal; the rest before the glyph are the cursor.
        let cursor = quads
            .iter()
            .skip(1)
            .take_while(|quad| quad.mode == MODE_SOLID)
            .copied()
            .collect();
        (cursor, font)
    }

    #[test]
    fn each_cursor_style_has_its_own_shape() {
        let shape = |bytes| cursor_quads(&Grid::from_vt(bytes, 4, 1).unwrap());
        let (bar, font) = shape(b"\x1b[6 q");
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].rect[2], font.line_thickness as f32);
        let (underline, font) = shape(b"\x1b[4 q");
        assert_eq!(underline[0].rect[3], font.line_thickness as f32);
        let (block, font) = shape(b"");
        assert_eq!(block[0].rect[3], font.cell_height as f32);
    }

    #[test]
    fn a_password_prompt_draws_a_lock_in_place_of_the_cursor() {
        let mut grid = Grid::from_vt(b"", 4, 1).unwrap();
        grid.password_input = true;
        let (lock, font) = cursor_quads(&grid);
        assert_eq!(lock.len(), 4);
        assert!(lock[0].rect[2] < font.cell_width as f32);
    }

    #[test]
    fn a_hollow_cursor_is_an_outline() {
        let mut grid = Grid::from_vt(b"", 4, 1).unwrap();
        grid.cursor_style = CursorVisualStyle::BlockHollow;
        assert_eq!(cursor_quads(&grid).0.len(), 4);
    }

    #[test]
    fn a_block_cursor_covers_both_cells_of_a_wide_glyph() {
        let grid = Grid::from_vt("世\x1b[1;1H".as_bytes(), 4, 1).unwrap();
        let (block, font) = cursor_quads(&grid);
        assert_eq!(block[0].rect[2], 2.0 * font.cell_width as f32);
    }

    fn glyph_colour(bytes: &[u8]) -> [f32; 3] {
        let (quads, _) = built_quads(bytes);
        quads
            .iter()
            .find(|quad| quad.mode == MODE_COVERAGE)
            .unwrap()
            .color
    }

    #[test]
    fn only_a_block_cursor_recolours_the_glyph_under_it() {
        let plain = glyph_colour(b"x\x1b[1;3H");
        assert_ne!(glyph_colour(b"x\x1b[1;1H"), plain);
        assert_eq!(glyph_colour(b"x\x1b[6 q\x1b[1;1H"), plain);
    }

    #[test]
    fn the_thumb_sits_in_the_right_padding_at_the_bottom_of_the_rows() {
        let rect = [0.0, 0.0, 300.0, 200.0];
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 12.0,
            scale: 2.0,
        };
        assert_eq!(
            scrollbar_thumb(&Grid::from_vt(b"hi", 20, 5).unwrap(), rect, metrics),
            None
        );
        let mut grid = Grid::from_vt("line\r\n".repeat(30).as_bytes(), 20, 5).unwrap();
        assert_eq!(scrollbar_thumb(&grid, rect, metrics), None, "hidden");
        let mut thumb = |opacity, wide| {
            if let Some(scrollbar) = &mut grid.scrollbar {
                (scrollbar.opacity, scrollbar.wide) = (opacity, wide);
            }
            scrollbar_thumb(&grid, rect, metrics).unwrap()
        };
        let [left, top, width, height] = thumb(0.5, false);
        assert!(left >= 300.0 - 12.0 && left + width <= 300.0);
        assert_eq!(top + height, 12.0 + 5.0 * 20.0);
        let [wide_left, _, wide_width, _] = thumb(1.0, true);
        assert!(wide_width > width && wide_left >= 300.0 - 12.0 && wide_left + wide_width <= 300.0);
    }

    #[test]
    fn the_thumb_contrasts_with_any_background() {
        assert_eq!(thumb_color([0; 3], 1.0), [102; 3]);
        assert_eq!(thumb_color([255; 3], 1.0), [153; 3]);
        assert_eq!(thumb_color([0; 3], 0.5), [51; 3]);
    }

    #[test]
    #[ignore = "benchmark: cargo test --release bench_ -- --ignored --nocapture"]
    fn bench_build_quads() {
        let mut bytes = Vec::new();
        for row in 0..60 {
            bytes.extend(
                format!(
                    "\x1b[3{}mline {row} the quick brown fox \x1b[1;4{}mjumps\x1b[0m over 世界 the lazy dog e\u{301} \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} a\u{308}o\u{308} \x1b[3mitalic {row}\x1b[0m\r\n",
                    row % 8,
                    row % 7
                )
                .as_bytes(),
            );
        }
        let grid = Grid::from_vt(&bytes, 200, 60).unwrap();
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 2.0).unwrap();
        let (jobs, _queued) = crossbeam_channel::unbounded();
        let mut atlas = Atlas::new(0);
        let mut quads = Vec::new();
        let build = |atlas: &mut Atlas, quads: &mut Vec<Quad>| {
            build_quads(
                &grid,
                [0.0, 0.0, 2000.0, 1500.0],
                metrics(&font, 0.0, 2.0),
                &font,
                true,
                quads,
                |key| (atlas.glyph(&jobs, key), atlas.epoch),
            );
        };
        build(&mut atlas, &mut quads);
        for (index, slot) in atlas.slots.values_mut().enumerate() {
            *slot = AtlasSlot::Ready(Some(fake_glyph([index as u32 * 16, 0])));
        }
        atlas.requested.clear();
        // The fastest batch, as other load on the machine only ever slows one down.
        let frames = 50u32;
        let fastest = (0..40)
            .map(|_| {
                let start = std::time::Instant::now();
                for _ in 0..frames {
                    build(&mut atlas, &mut quads);
                }
                start.elapsed() / frames
            })
            .min()
            .unwrap_or_default();
        println!(
            "build_quads 200x60: {} ns/frame, {} quads, {} clusters",
            fastest.as_nanos(),
            quads.len(),
            grid.clusters.len()
        );
    }

    /// A flood: every frame brings lines the run cache hasn't seen, so most runs are shaped anew.
    #[test]
    #[ignore = "benchmark: cargo test --release bench_ -- --ignored --nocapture"]
    fn bench_shaping_flood() {
        let (cols, rows) = (150u16, 60u16);
        let grids: Vec<Grid> = (0..100)
            .map(|frame| {
                let mut bytes = Vec::new();
                for row in 0..u32::from(rows) {
                    let line = frame * u32::from(rows) + row;
                    let text = if row % 2 == 0 {
                        format!(
                            "\x1b[1;3{}m{line}\x1b[0m \x1b[38;5;{}mstaff\x1b[0m \x1b[4;44mfile-{line}.rs\x1b[0m dim \x1b[7mrev\x1b[0m 世界 é -> !=",
                            line % 8,
                            line % 256
                        )
                    } else {
                        format!("{line} the quick brown fox jumps over the lazy dog 0123456789 abcdefghij")
                    };
                    bytes.extend(format!("\x1b[{};1H{text}", row + 1).as_bytes());
                }
                Grid::from_vt(&bytes, cols, rows).unwrap()
            })
            .collect();
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 8.0, 2.0).unwrap();
        let mut quads = Vec::new();
        let start = std::time::Instant::now();
        for grid in &grids {
            build_quads(
                grid,
                [0.0, 0.0, 2000.0, 1500.0],
                metrics(&font, 0.0, 2.0),
                &font,
                true,
                &mut quads,
                |_| (Some(fake_glyph([0, 0])), 0),
            );
        }
        println!(
            "build_quads {cols}x{rows} flood: {} us/frame",
            start.elapsed().as_micros() / grids.len() as u128
        );
    }

    fn placement(col: i32, row: i32, size: [u32; 2]) -> Placement {
        Placement {
            image: Arc::new(Image {
                id: 1,
                generation: 1,
                width: 4,
                height: 4,
                rgba: vec![0; 64],
            }),
            layer: ImageLayer::AboveText,
            col,
            row,
            offset: [0, 0],
            size,
            source: [0, 0, 4, 4],
        }
    }

    #[test]
    fn an_image_past_the_top_of_the_viewport_is_clipped_with_its_texture_coordinates() {
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 0.0,
            scale: 1.0,
        };
        let area = [5.0, 7.0, 100.0, 100.0];
        // Two cells tall, starting one row above the viewport, so half of it shows.
        let quad =
            image_quad(&placement(1, -1, [20, 40]), (5.0, 7.0), metrics, area, true).unwrap();
        assert_eq!(quad.rect, [15.0, 7.0, 20.0, 20.0]);
        assert_eq!(quad.uv, [0.0, 0.5, 1.0, 0.5]);
        assert_eq!(quad.mode, MODE_IMAGE);
        let linear = image_quad(&placement(0, 0, [20, 40]), (5.0, 7.0), metrics, area, false);
        assert_eq!(linear.unwrap().mode, MODE_IMAGE_LINEAR);
        assert!(image_quad(&placement(0, -2, [20, 40]), (5.0, 7.0), metrics, area, true).is_none());
    }

    #[test]
    fn a_cropped_image_samples_only_its_source_rectangle() {
        let metrics = Metrics {
            cell_width: 10,
            cell_height: 20,
            padding: 0.0,
            scale: 1.0,
        };
        let mut cropped = placement(0, 0, [10, 20]);
        cropped.source = [2, 0, 2, 4];
        let quad = image_quad(
            &cropped,
            (0.0, 0.0),
            metrics,
            [0.0, 0.0, 100.0, 100.0],
            true,
        )
        .unwrap();
        assert_eq!(quad.uv, [0.5, 0.0, 0.5, 1.0]);
    }

    #[test]
    fn a_reset_during_a_build_leaves_no_quad_on_a_wiped_slot() {
        let grid = Grid::message("abc");
        let font = Font::new(crate::config::DEFAULT_FONT_FAMILY, 18.0, 1.0).unwrap();
        let mut slots: HashMap<GlyphKey, [u32; 2]> = HashMap::new();
        let mut epoch = 0;
        let mut quads = Vec::new();
        build_quads(
            &grid,
            [0.0, 0.0, 800.0, 600.0],
            metrics(&font, 0.0, 1.0),
            &font,
            true,
            &mut quads,
            |key| {
                if !slots.contains_key(&key) {
                    if slots.len() == 2 && epoch == 0 {
                        slots.clear();
                        epoch = 1;
                    }
                    let position = [slots.len() as u32 * 16, 0];
                    slots.insert(key.clone(), position);
                }
                (slots.get(&key).copied().map(fake_glyph), epoch)
            },
        );
        let glyph_positions: Vec<[u32; 2]> = quads
            .iter()
            .filter(|quad| quad.uv[2] != 0.0)
            .map(|quad| [quad.uv[0].to_bits(), quad.uv[1].to_bits()])
            .collect();
        assert_eq!(glyph_positions.len(), 3);
        assert_eq!(glyph_positions.iter().collect::<HashSet<_>>().len(), 3);
    }
}
