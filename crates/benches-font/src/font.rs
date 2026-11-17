pub mod constraint;
mod nerd_font_table;
pub mod sprite;

#[cfg(target_os = "macos")]
mod coretext;
#[cfg(target_os = "macos")]
use coretext::CoreText as Os;

#[cfg(not(target_os = "macos"))]
compile_error!("benches-font has a CoreText backend only");

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::rc::Rc;

use anyhow::{Context, Result};

use crate::constraint::{Bounds, Constraint, Metrics, Modifier, Size, is_private_use};
use crate::sprite::TiledUnderline;

pub const DEFAULT_FONT_FAMILY: &str = "Monaspace Neon";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlyphKind {
    Coverage,
    Color,
}

/// One OpenType feature setting, as Ghostty's `font-feature` writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontFeature {
    tag: [u8; 4],
    value: i32,
}

impl FontFeature {
    /// `calt`, `+calt`, `-calt`, `ss01=2`, each optionally quoted.
    fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim().trim_matches('"');
        let (spec, default) = match spec.strip_prefix('-') {
            Some(rest) => (rest, 0),
            None => (spec.strip_prefix('+').unwrap_or(spec), 1),
        };
        let (tag, value) = match spec.split_once('=') {
            Some((tag, value)) => (tag, value.trim().parse().ok()?),
            None => (spec, default),
        };
        let tag: [u8; 4] = tag.trim().trim_matches('"').as_bytes().try_into().ok()?;
        tag.is_ascii().then_some(Self { tag, value })
    }
}

/// Every feature in `specs`, whose entries may also hold comma-separated lists.
/// Entries that aren't valid are reported and skipped.
pub fn parse_features(specs: &[String]) -> Vec<FontFeature> {
    specs
        .iter()
        .flat_map(|spec| spec.split(','))
        .filter(|spec| !spec.trim().is_empty())
        .filter_map(|spec| {
            let feature = FontFeature::parse(spec);
            if feature.is_none() {
                eprintln!("benches: ignoring invalid font feature {spec:?}");
            }
            feature
        })
        .collect()
}

/// A face's measurements in device pixels.
struct FaceMetrics {
    ascent: f64,
    descent: f64,
    leading: f64,
    underline_position: f64,
    underline_thickness: f64,
    x_height: f64,
    cap_height: f64,
}

/// A glyph id and its origin, in pixels from the start of the run and up from the baseline.
#[derive(Clone, Copy)]
struct Glyph {
    id: u16,
    x: f64,
    y: f64,
}

/// Where a glyph's ink starts and ends horizontally, if it has any.
type InkExtent = Option<(f64, f64)>;

/// Maps a point `p` of the drawing to `offset + scale * (p - origin)` on the canvas.
struct Placement {
    offset: (f64, f64),
    scale: (f64, f64),
    origin: (f64, f64),
}

/// The system font services a `Font` is built on. Canvases are premultiplied RGBA with the top
/// row first, and draw white with font smoothing off, so that `rgb != alpha` means colour.
trait Backend {
    type Face: Clone;
    /// Text shaped with the system's fallback cascade for characters the face lacks.
    type Line;
    type Canvas<'a>;

    fn new() -> Self;

    /// `None` when `family` isn't installed, rather than whatever the system substitutes.
    fn installed_face(&self, family: &str, pixels: f64) -> Option<Self::Face>;
    /// `None` when the family has no such style.
    fn styled(&self, face: &Self::Face, style: FontStyle) -> Option<Self::Face>;
    fn embedded_face(&self, bytes: &'static [u8], pixels: f64) -> Result<Self::Face>;
    /// The face shaping with `features` on top of the font's defaults. A shaper that takes features
    /// per call keeps them with the face.
    fn with_features(face: Self::Face, features: &[FontFeature]) -> Self::Face;
    /// The face's own glyph for `character`, without fallback.
    fn glyph(face: &Self::Face, character: char) -> Option<u16>;
    fn advance(face: &Self::Face, glyph: u16) -> f64;
    fn metrics(face: &Self::Face) -> FaceMetrics;

    /// The glyphs of `text` as `line` shapes it. Only glyphs from the face itself draw correctly
    /// through `draw_glyphs`, so callers shape only characters the face has.
    fn shape(face: &Self::Face, text: &str) -> Option<Vec<Glyph>>;
    fn glyph_ink(face: &Self::Face, glyph: u16) -> InkExtent;
    fn line(face: &Self::Face, text: &str) -> Option<Self::Line>;
    fn line_width(line: &Self::Line) -> f64;

    /// A canvas drawing over `out`, which callers clear first, or `None` if `out` is smaller than `width` by `height` pixels.
    fn canvas<'a>(&self, out: &'a mut [u8], width: u32, height: u32) -> Option<Self::Canvas<'a>>;
    /// The line's ink, with `y` measured up from its baseline.
    fn line_ink(canvas: &Self::Canvas<'_>, line: &Self::Line) -> Bounds;
    fn draw_line(canvas: &mut Self::Canvas<'_>, line: &Self::Line, placement: Placement);
    /// Each glyph at its origin, in pixels up from the bottom of the canvas.
    fn draw_glyphs(
        canvas: &mut Self::Canvas<'_>,
        face: &Self::Face,
        glyphs: impl IntoIterator<Item = Glyph>,
    );

    #[cfg(test)]
    fn postscript_name(face: &Self::Face) -> String;
}

type Face = <Os as Backend>::Face;

/// A monospaced face rasterised into whole-cell RGBA bitmaps, in device pixels.
pub struct Font {
    backend: Os,
    /// Indexed by `bold + 2 * italic`.
    faces: [Face; 4],
    symbols: Face,
    pub cell_width: u32,
    pub cell_height: u32,
    pub line_thickness: u32,
    /// Ghostty's `font-shaping-break = cursor`, applied by the renderer.
    pub break_runs_at_cursor: bool,
    pub underline_top: u32,
    pub strike_top: u32,
    baseline: f64,
    metrics: Metrics,
    /// A cell's advance before rounding up to whole pixels.
    advance: f64,
    /// Per style, which cells of a run shape differently from standing alone.
    runs: RefCell<[RunCache; 4]>,
    /// The glyph a character shapes to alone, per style index.
    alone: RefCell<FnvMap<(usize, char), Option<u16>>>,
    joins: RefCell<FnvMap<char, bool>>,
    /// The horizontal extent of a glyph's ink from its origin, per style index and glyph.
    ink: RefCell<FnvMap<(usize, u16), InkExtent>>,
}

type RunCache = FnvMap<Rc<str>, Rc<ShapedRun>>;

/// A run shaped as a whole, and which of its cells differ from standing alone.
pub struct ShapedRun {
    pub text: Rc<str>,
    /// Per cell, a hash of the glyphs that paint it, or 0 for a cell drawn as it would be alone.
    /// Equal signatures draw equal bitmaps, so the atlas shares them between runs.
    signatures: Box<[u64]>,
}

impl ShapedRun {
    pub fn signature(&self, index: usize) -> Option<u64> {
        self.signatures
            .get(index)
            .copied()
            .filter(|&hash| hash != 0)
    }
}

/// FNV-1a, because the shaping caches and glyph atlas hash every word or cell of every rebuild and
/// their keys are short.
#[derive(Default)]
pub struct Fnv(u64);

pub type FnvMap<K, V> = HashMap<K, V, BuildHasherDefault<Fnv>>;

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = hash;
    }
}

/// Runs cached before the cache is emptied; words are the usual run, so this bounds memory.
const RUN_CACHE_LIMIT: usize = 4096;

const SYMBOLS_FONT: &[u8] = include_bytes!("../assets/SymbolsNerdFont-Regular.ttf");

fn has_icons(face: &Face) -> bool {
    // A Powerline separator and a Font Awesome icon.
    ['\u{E0B0}', '\u{F432}']
        .into_iter()
        .all(|character| Os::glyph(face, character).is_some())
}

/// The face for private-use icons: the configured family, else the first Ghostty fallback, else the
/// main font, each only when it has the icons, else the bundled Symbols Nerd Font.
fn icon_face(
    backend: &Os,
    main: &Face,
    pixels: f64,
    configured: Option<&str>,
    fallbacks: &[String],
) -> Result<Face> {
    let with_icons = |family: &str| backend.installed_face(family, pixels).filter(has_icons);
    if let Some(family) = configured {
        if let Some(font) = with_icons(family) {
            return Ok(font);
        }
        eprintln!(
            "benches: symbols family {family} isn't installed or lacks icons; using the next choice"
        );
    }
    if let Some(font) = fallbacks.iter().find_map(|family| with_icons(family)) {
        return Ok(font);
    }
    if has_icons(main) {
        return Ok(main.clone());
    }
    backend
        .embedded_face(SYMBOLS_FONT, pixels)
        .context("the bundled symbols font couldn't be loaded")
}

/// Regular, bold, italic and bold italic faces of `family`, or `None` if it isn't installed.
/// A style the family lacks reuses the regular face.
fn family_faces(backend: &Os, family: &str, pixels: f64) -> Option<[Face; 4]> {
    let regular = backend.installed_face(family, pixels)?;
    let [bold, italic, bold_italic] =
        [(true, false), (false, true), (true, true)].map(|(bold, italic)| {
            backend
                .styled(&regular, FontStyle { bold, italic })
                .unwrap_or_else(|| regular.clone())
        });
    Some([regular, bold, italic, bold_italic])
}

/// A placed glyph and where its ink starts and ends horizontally, if it has any.
struct Placed {
    glyph: Glyph,
    ink: InkExtent,
}

/// A glyph with any `rgb != alpha` pixel is colour, such as an emoji drawn through a constraint.
fn glyph_kind(pixels: &[u8]) -> GlyphKind {
    let is_color = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[0] != pixel[3] || pixel[1] != pixel[3] || pixel[2] != pixel[3]);
    if is_color {
        GlyphKind::Color
    } else {
        GlyphKind::Coverage
    }
}

impl Font {
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(family: &str, points: f64, scale: f32) -> Result<Self> {
        Self::with_symbols(family, points, scale, None, &[], &[])
    }

    pub fn with_symbols(
        family: &str,
        points: f64,
        scale: f32,
        symbols_family: Option<&str>,
        fallback_families: &[String],
        features: &[String],
    ) -> Result<Self> {
        let pixels = points * f64::from(scale);
        let backend = Os::new();
        let faces = family_faces(&backend, family, pixels)
            .or_else(|| {
                eprintln!(
                    "benches: font family {family} isn't installed; using {DEFAULT_FONT_FAMILY}"
                );
                family_faces(&backend, DEFAULT_FONT_FAMILY, pixels)
            })
            .with_context(|| format!("neither {family} nor {DEFAULT_FONT_FAMILY} is installed"))?;
        let features = parse_features(features);
        let faces = faces.map(|face| Os::with_features(face, &features));
        let regular = &faces[0];
        let advance = Os::advance(
            regular,
            Os::glyph(regular, 'M').context("the terminal font has no glyph for M")?,
        );
        let FaceMetrics {
            ascent,
            descent,
            leading,
            underline_position,
            underline_thickness,
            x_height,
            cap_height,
        } = Os::metrics(regular);
        // Cell and icon metrics follow Ghostty's `Metrics.calc` in `src/font/Metrics.zig`
        // (MIT, © Mitchell Hashimoto, Ghostty contributors).
        let face_height = ascent + descent + leading;
        let cell_height = face_height.ceil() as u32;
        let cell_width = advance.ceil() as u32;
        let baseline = (descent + leading / 2.0).round();
        let baseline_from_top = f64::from(cell_height) - baseline;
        let line_thickness = (underline_thickness.round() as u32).max(1);
        let half_line = f64::from(line_thickness) / 2.0;
        // Room for the second line of a double underline.
        let lowest_underline = cell_height.saturating_sub(3 * line_thickness);
        let symbols = icon_face(&backend, regular, pixels, symbols_family, fallback_families)?;
        Ok(Self {
            cell_width,
            cell_height,
            line_thickness,
            underline_top: ((baseline_from_top - underline_position - half_line).round() as u32)
                .min(lowest_underline),
            strike_top: (baseline_from_top - x_height / 2.0 - half_line).round() as u32,
            baseline,
            metrics: Metrics {
                cell_width: f64::from(cell_width),
                cell_height: f64::from(cell_height),
                face_width: advance,
                face_height,
                face_y: baseline - (leading / 2.0 + descent),
                icon_height: face_height,
                icon_height_single: (2.0 * cap_height + face_height) / 3.0,
            },
            break_runs_at_cursor: true,
            advance,
            runs: RefCell::default(),
            alone: RefCell::default(),
            joins: RefCell::default(),
            ink: RefCell::default(),
            faces,
            symbols,
            backend,
        })
    }

    fn face_index(style: FontStyle) -> usize {
        usize::from(style.bold) + 2 * usize::from(style.italic)
    }

    /// Ghostty's `adjust-icon-height`, applied to the icon heights the constraints size against.
    pub fn adjust_icon_height(&mut self, modifier: Modifier) {
        self.metrics.icon_height = modifier.apply(self.metrics.icon_height);
        self.metrics.icon_height_single = modifier.apply(self.metrics.icon_height_single);
    }

    /// Premultiplied RGBA for `text`, `cells * cell_width` by `cell_height` with the top row first.
    pub fn rasterize(
        &self,
        text: &str,
        style: FontStyle,
        cells: u32,
        out: &mut Vec<u8>,
    ) -> Option<GlyphKind> {
        // Constraints are sized for the chosen symbols face, and a system cascade needn't pick an
        // installed icon font for private-use codepoints (CoreText never does).
        let font = if text.chars().any(is_private_use) {
            &self.symbols
        } else {
            &self.faces[Self::face_index(style)]
        };
        let width = self.cell_width * cells;
        out.clear();
        out.resize((width * self.cell_height * 4) as usize, 0);
        let mut characters = text.chars();
        let single = characters.next().filter(|_| characters.next().is_none());
        if let Some(character) = single
            && cells == 1
            && sprite::draw(character, width, self.cell_height, self.line_thickness, out)
        {
            return Some(GlyphKind::Coverage);
        }
        let line = Os::line(font, text)?;
        let mut canvas = self.backend.canvas(out, width, self.cell_height)?;
        let placement = single
            .and_then(Constraint::of)
            .and_then(|constraint| {
                self.constrained(constraint, Os::line_ink(&canvas, &line), cells)
            })
            .unwrap_or_else(|| self.centred(Os::line_width(&line), width));
        Os::draw_line(&mut canvas, &line, placement);
        drop(canvas);
        Some(glyph_kind(out))
    }

    /// Fits a constrained glyph's ink to the box its constraint gives it, or `None` for no ink.
    fn constrained(&self, constraint: Constraint, ink: Bounds, cells: u32) -> Option<Placement> {
        if ink.width <= 0.0 || ink.height <= 0.0 {
            return None;
        }
        let glyph = Bounds {
            y: ink.y + self.baseline,
            ..ink
        };
        let mut target = constraint.constrain(glyph, self.metrics, cells);
        if constraint.size != Size::Stretch {
            // Centre the face in a pixel cell wider than its advance.
            target.x += (self.metrics.cell_width - self.metrics.face_width) / 2.0;
        }
        Some(Placement {
            offset: (target.x, target.y),
            scale: (target.width / glyph.width, target.height / glyph.height),
            origin: (glyph.x, ink.y),
        })
    }

    /// Centres a line in `width` pixels on the baseline, shrinking it to fit if it's wider.
    fn centred(&self, line_width: f64, width: u32) -> Placement {
        let available = f64::from(width);
        // Fallback faces such as CJK are narrower than two terminal cells.
        let centred = ((available - line_width) / 2.0).max(0.0);
        let shrink = (available / line_width).min(1.0);
        Placement {
            offset: (centred, self.baseline),
            scale: (shrink, shrink),
            origin: (0.0, 0.0),
        }
    }

    /// Whether `character` may be shaped with its neighbours. Only characters the main face draws
    /// itself qualify, which keeps a run in one font, and only those in the BMP, so a character
    /// is one UTF-16 unit. A space joins, to be shaped through; other whitespace, procedural
    /// glyphs and icons are drawn on their own.
    #[inline]
    pub fn joins_run(&self, character: char) -> bool {
        if character.is_ascii() {
            character.is_ascii_graphic() || character == ' '
        } else {
            self.joins_non_ascii(character)
        }
    }

    fn joins_non_ascii(&self, character: char) -> bool {
        *self.joins.borrow_mut().entry(character).or_insert_with(|| {
            !character.is_whitespace()
                && !character.is_control()
                && !is_private_use(character)
                && character.len_utf16() == 1
                && Os::glyph(&self.faces[0], character).is_some()
                && !sprite::draw(
                    character,
                    self.cell_width,
                    self.cell_height,
                    self.line_thickness,
                    &mut vec![0; (self.cell_width * self.cell_height * 4) as usize],
                )
        })
    }

    fn glyph_alone(&self, face: usize, character: char) -> Option<u16> {
        *self
            .alone
            .borrow_mut()
            .entry((face, character))
            .or_insert_with(|| {
                match Os::shape(&self.faces[face], character.encode_utf8(&mut [0; 4]))?[..] {
                    [glyph] => Some(glyph.id),
                    _ => None,
                }
            })
    }

    fn glyph_ink(&self, face: usize, glyph: u16) -> InkExtent {
        *self
            .ink
            .borrow_mut()
            .entry((face, glyph))
            .or_insert_with(|| Os::glyph_ink(&self.faces[face], glyph))
    }

    /// The glyphs of `shaped` placed in the cell grid, each with its ink's horizontal extent.
    /// A cell's glyph is centred in it and the shaper advances by the unrounded width.
    fn placed(&self, face: usize, shaped: &[Glyph]) -> Vec<Placed> {
        let ratio = f64::from(self.cell_width) / self.advance;
        let centred = (f64::from(self.cell_width) - self.advance) / 2.0;
        shaped
            .iter()
            .map(|glyph| {
                let x = glyph.x * ratio + centred;
                let ink = self
                    .glyph_ink(face, glyph.id)
                    .map(|(low, high)| (x + low * ratio, x + high * ratio));
                Placed {
                    glyph: Glyph {
                        id: glyph.id,
                        x,
                        y: glyph.y + self.baseline,
                    },
                    ink,
                }
            })
            .collect()
    }

    /// The placed glyphs whose ink reaches into cell `index`, by more than antialiasing's half pixel.
    fn painting<'a>(
        &self,
        placed: &'a [Placed],
        index: usize,
    ) -> impl Iterator<Item = &'a Placed> + use<'a> {
        let left = index as f64 * f64::from(self.cell_width);
        let right = left + f64::from(self.cell_width);
        placed.iter().filter(move |placed| {
            placed
                .ink
                .is_some_and(|(low, high)| low < right - 0.5 && high > left + 0.5)
        })
    }

    /// Which cells of `run` (see `joins_run`) come out differently from standing alone, such as
    /// the cells of a ligature. The rest can be drawn as plain cells, so the atlas stores nothing for them.
    pub fn shaped_cells(&self, run: &str, style: FontStyle) -> Rc<ShapedRun> {
        let face = Self::face_index(style);
        if let Some(cells) = self.runs.borrow()[face].get(run) {
            return cells.clone();
        }
        let cells = Rc::new(self.shape_run(run, face));
        let mut runs = self.runs.borrow_mut();
        if runs[face].len() >= RUN_CACHE_LIMIT {
            runs[face].clear();
        }
        runs[face].insert(cells.text.clone(), cells.clone());
        cells
    }

    fn shape_run(&self, run: &str, face: usize) -> ShapedRun {
        let length = run.chars().count();
        let text: Rc<str> = run.into();
        let Some(shaped) = Os::shape(&self.faces[face], run) else {
            return ShapedRun {
                text,
                signatures: vec![0; length].into(),
            };
        };
        // A monospaced font gives a ligature one glyph per character, though the shaper may
        // cluster them under the first. Fewer glyphs than characters is a true ligature, so no
        // cell stands alone.
        let in_order = shaped.len() == length;
        let differs: Vec<bool> = if in_order {
            run.chars()
                .zip(&shaped)
                .zip(0..)
                .map(|((character, glyph), cell): (_, u32)| {
                    self.glyph_alone(face, character) != Some(glyph.id)
                        || (glyph.x - f64::from(cell) * self.advance).abs() > 0.5
                })
                .collect()
        } else {
            vec![true; length]
        };
        if !differs.contains(&true) {
            return ShapedRun {
                text,
                signatures: vec![0; length].into(),
            };
        }
        let placed = self.placed(face, &shaped);
        let signatures = differs
            .iter()
            .enumerate()
            .map(|(index, differs)| {
                if !differs {
                    return 0;
                }
                // The glyphs that paint a cell, and where, say everything about its bitmap, so the
                // same ligature shares one atlas entry wherever it appears.
                let mut hasher = Fnv::default();
                face.hash(&mut hasher);
                let origin = index as f64 * f64::from(self.cell_width);
                for Placed { glyph, .. } in self.painting(&placed, index) {
                    (glyph.id, ((glyph.x - origin) * 8.0).round() as i32).hash(&mut hasher);
                }
                hasher.finish() | 1
            })
            .collect();
        ShapedRun { text, signatures }
    }

    /// The bitmap of cell `index` of `run`, shaped together with its neighbours, in the format
    /// of `rasterize`. Ink a glyph spills into a neighbouring cell goes with that cell.
    pub fn rasterize_shaped(
        &self,
        run: &str,
        style: FontStyle,
        index: usize,
        out: &mut Vec<u8>,
    ) -> Option<GlyphKind> {
        let face = Self::face_index(style);
        let font = &self.faces[face];
        let placed = self.placed(face, &Os::shape(font, run)?);
        out.clear();
        out.resize((self.cell_width * self.cell_height * 4) as usize, 0);
        let mut canvas = self
            .backend
            .canvas(out, self.cell_width, self.cell_height)?;
        let origin = index as f64 * f64::from(self.cell_width);
        let glyphs = self.painting(&placed, index).map(|placed| Glyph {
            x: placed.glyph.x - origin,
            ..placed.glyph
        });
        Os::draw_glyphs(&mut canvas, font, glyphs);
        Some(GlyphKind::Coverage)
    }

    /// One cell of a tiling underline, in the same format as `rasterize`.
    pub fn rasterize_underline(&self, kind: TiledUnderline, out: &mut Vec<u8>) {
        out.clear();
        out.resize((self.cell_width * self.cell_height * 4) as usize, 0);
        sprite::underline(
            kind,
            self.cell_width,
            self.cell_height,
            self.underline_top,
            self.line_thickness,
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGULAR: FontStyle = FontStyle {
        bold: false,
        italic: false,
    };

    /// Rows of the bitmap holding any ink.
    fn ink_height(font: &Font, pixels: &[u8], cells: u32) -> usize {
        pixels
            .chunks_exact((font.cell_width * cells * 4) as usize)
            .filter(|row| row.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0))
            .count()
    }

    #[test]
    fn plain_text_is_coverage_and_emoji_is_colour() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        assert_eq!(
            font.rasterize("A", REGULAR, 1, &mut pixels),
            Some(GlyphKind::Coverage)
        );
        assert!(ink_height(&font, &pixels, 1) > 0);
        assert_eq!(
            font.rasterize("\u{1F600}", REGULAR, 2, &mut pixels),
            Some(GlyphKind::Color)
        );
    }

    #[test]
    fn a_character_missing_from_the_terminal_font_falls_back_to_another_face() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        assert_eq!(
            font.rasterize("世", REGULAR, 2, &mut pixels),
            Some(GlyphKind::Coverage)
        );
        assert!(ink_height(&font, &pixels, 2) > 0);
    }

    #[test]
    fn a_zwj_sequence_shapes_into_one_glyph() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut single = Vec::new();
        font.rasterize("\u{1F468}", REGULAR, 2, &mut single);
        let mut family = Vec::new();
        font.rasterize(
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            REGULAR,
            2,
            &mut family,
        );
        // Three separate emoji would be shrunk to a third of the height to fit.
        assert!(ink_height(&font, &family, 2) * 10 >= ink_height(&font, &single, 2) * 7);
    }

    /// Columns of the bitmap holding any ink, first and last.
    fn ink_columns(font: &Font, pixels: &[u8], cells: u32) -> (usize, usize) {
        let width = (font.cell_width * cells) as usize;
        let columns: Vec<usize> = (0..width)
            .filter(|column| {
                pixels
                    .chunks_exact(width * 4)
                    .any(|row| row[column * 4 + 3] != 0)
            })
            .collect();
        (columns[0], columns[columns.len() - 1])
    }

    #[test]
    fn powerline_extra_squares_fill_the_cell_height_from_the_left_edge() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("\u{E0C6}", REGULAR, 2, &mut pixels);
        assert!(ink_height(&font, &pixels, 2) + 2 >= font.cell_height as usize);
        let (first, last) = ink_columns(&font, &pixels, 2);
        assert!(first <= 1, "starts at {first}");
        assert!(last >= font.cell_width as usize, "ends at {last}");
        font.rasterize("\u{E0C6}", REGULAR, 1, &mut pixels);
        assert!(ink_height(&font, &pixels, 1) + 2 >= font.cell_height as usize);
        assert!(ink_columns(&font, &pixels, 1).1 + 2 >= font.cell_width as usize);
    }

    fn icon_height_single(font: &Font) -> f64 {
        let metrics = Os::metrics(&font.faces[0]);
        (2.0 * metrics.cap_height + metrics.ascent + metrics.descent + metrics.leading) / 3.0
    }

    #[test]
    fn a_height_limited_icon_is_as_high_as_ghosttys_icon_height() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("\u{F0E7}", REGULAR, 1, &mut pixels);
        let height = ink_height(&font, &pixels, 1) as f64;
        let expected = icon_height_single(&font);
        assert!(
            (height - expected).abs() <= 1.5,
            "{height} high, expected {expected}"
        );
    }

    #[test]
    fn adjusting_the_icon_height_scales_both_icon_metrics() {
        let mut font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let (tall, single) = (font.metrics.icon_height, font.metrics.icon_height_single);
        font.adjust_icon_height(Modifier::Percent(0.5));
        assert_eq!(font.metrics.icon_height, tall / 2.0);
        assert_eq!(font.metrics.icon_height_single, single / 2.0);
        font.adjust_icon_height(Modifier::Absolute(3));
        assert_eq!(font.metrics.icon_height_single, single / 2.0 + 3.0);
    }

    #[test]
    fn a_height_limited_icon_shrinks_with_a_smaller_icon_height() {
        let mut font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("\u{F0E7}", REGULAR, 1, &mut pixels);
        let full = ink_height(&font, &pixels, 1);
        font.adjust_icon_height(Modifier::Percent(0.5));
        font.rasterize("\u{F0E7}", REGULAR, 1, &mut pixels);
        let half = ink_height(&font, &pixels, 1);
        assert!(half + 5 <= full, "{half} high, was {full}");
    }

    #[test]
    fn an_icon_is_larger_with_two_cells_than_when_squeezed_into_one() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let (mut squeezed, mut roomy) = (Vec::new(), Vec::new());
        font.rasterize("\u{F418}", REGULAR, 1, &mut squeezed);
        font.rasterize("\u{F418}", REGULAR, 2, &mut roomy);
        let width = |pixels: &[u8], cells| {
            let (first, last) = ink_columns(&font, pixels, cells);
            last - first
        };
        assert!(width(&roomy, 2) > width(&squeezed, 1) + 2);
        assert!(ink_height(&font, &roomy, 2) > ink_height(&font, &squeezed, 1) + 2);
    }

    #[test]
    fn a_regular_icon_stays_within_its_cell() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("\u{F432}", REGULAR, 1, &mut pixels);
        assert!(ink_height(&font, &pixels, 1) > 0);
        assert_eq!(
            pixels.len(),
            (font.cell_width * font.cell_height * 4) as usize
        );
    }

    #[test]
    fn box_drawing_is_procedural_and_touches_the_cell_edge() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("│", REGULAR, 1, &mut pixels);
        assert_eq!(
            ink_height(&font, &pixels, 1),
            font.cell_height as usize,
            "a vertical line must leave no gap between rows"
        );
    }

    #[test]
    fn decoration_lines_fit_inside_the_cell() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        assert!(font.underline_top + 3 * font.line_thickness <= font.cell_height);
        assert!(font.strike_top + font.line_thickness <= font.cell_height);
        assert!(font.strike_top < font.underline_top);
    }

    fn face_names(font: &Font) -> Vec<String> {
        font.faces.iter().map(Os::postscript_name).collect()
    }

    #[test]
    fn a_family_gives_its_four_styles() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        assert_eq!(
            face_names(&font),
            [
                "MonaspaceNeon-Regular",
                "MonaspaceNeon-Bold",
                "MonaspaceNeon-Italic",
                "MonaspaceNeon-BoldItalic"
            ]
        );
    }

    #[test]
    fn an_unknown_family_falls_back_to_monaspace_neon() {
        let font = Font::new("No Such Family", 13.0, 2.0).unwrap();
        assert_eq!(face_names(&font)[0], "MonaspaceNeon-Regular");
    }

    #[test]
    fn a_family_without_a_style_reuses_the_regular_face() {
        // Andale Mono has one face.
        let font = Font::new("Andale Mono", 13.0, 2.0).unwrap();
        let names = face_names(&font);
        assert!(names.iter().all(|name| name == &names[0]), "{names:?}");
    }

    #[test]
    fn private_use_icons_are_real_glyphs_rather_than_tofu() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let (mut separator, mut icon) = (Vec::new(), Vec::new());
        font.rasterize("\u{E0C6}", REGULAR, 1, &mut separator);
        font.rasterize("\u{F432}", REGULAR, 1, &mut icon);
        // Every missing private-use character draws the same placeholder.
        assert_ne!(separator, icon);
        assert_eq!(symbols_name(&font), "SymbolsNF");
    }

    fn symbols_name(font: &Font) -> String {
        Os::postscript_name(&font.symbols)
    }

    /// CI doesn't have Caskaydia; tests that need it skip.
    fn nerd_family_installed() -> bool {
        family_faces(&Os::new(), NERD_FAMILY, 26.0).is_some()
    }

    const NERD_FAMILY: &str = "CaskaydiaMono Nerd Font Mono";

    #[test]
    fn a_configured_nerd_family_draws_the_icons() {
        if !nerd_family_installed() {
            return;
        }
        let font = Font::with_symbols(DEFAULT_FONT_FAMILY, 13.0, 2.0, Some(NERD_FAMILY), &[], &[])
            .unwrap();
        assert_eq!(symbols_name(&font), "CaskaydiaMonoNFM-Regular");
    }

    #[test]
    fn a_missing_configured_family_falls_back_to_the_embedded_face() {
        let font = Font::with_symbols(
            DEFAULT_FONT_FAMILY,
            13.0,
            2.0,
            Some("No Such Family"),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(symbols_name(&font), "SymbolsNF");
    }

    #[test]
    fn a_family_without_icons_is_not_used_for_them() {
        let font = Font::with_symbols(
            DEFAULT_FONT_FAMILY,
            13.0,
            2.0,
            Some("Andale Mono"),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(symbols_name(&font), "SymbolsNF");
    }

    #[test]
    fn the_first_ghostty_fallback_with_icons_is_used() {
        if !nerd_family_installed() {
            return;
        }
        let fallbacks = ["No Such Family", "Andale Mono", NERD_FAMILY].map(str::to_owned);
        let font =
            Font::with_symbols(DEFAULT_FONT_FAMILY, 13.0, 2.0, None, &fallbacks, &[]).unwrap();
        assert_eq!(symbols_name(&font), "CaskaydiaMonoNFM-Regular");
    }

    #[test]
    fn a_configured_family_without_icons_falls_through_to_ghostty_fallbacks() {
        if !nerd_family_installed() {
            return;
        }
        let fallbacks = [NERD_FAMILY.to_owned()];
        let font = Font::with_symbols(
            DEFAULT_FONT_FAMILY,
            13.0,
            2.0,
            Some("Andale Mono"),
            &fallbacks,
            &[],
        )
        .unwrap();
        assert_eq!(symbols_name(&font), "CaskaydiaMonoNFM-Regular");
    }

    #[test]
    fn a_main_font_with_icons_draws_them() {
        if !nerd_family_installed() {
            return;
        }
        let font = Font::new(NERD_FAMILY, 13.0, 2.0).unwrap();
        assert_eq!(symbols_name(&font), "CaskaydiaMonoNFM-Regular");
    }

    #[test]
    fn the_bitmap_covers_every_requested_cell() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("A", REGULAR, 2, &mut pixels);
        assert_eq!(
            pixels.len(),
            (font.cell_width * 2 * font.cell_height * 4) as usize
        );
    }

    fn parsed(spec: &str) -> Vec<FontFeature> {
        parse_features(&[spec.to_owned()])
    }

    fn feature(tag: &[u8; 4], value: i32) -> FontFeature {
        FontFeature { tag: *tag, value }
    }

    #[test]
    fn features_parse_in_ghostty_syntax() {
        assert_eq!(parsed("calt"), [feature(b"calt", 1)]);
        assert_eq!(parsed("+liga"), [feature(b"liga", 1)]);
        assert_eq!(parsed("-calt"), [feature(b"calt", 0)]);
        assert_eq!(parsed("ss01=2"), [feature(b"ss01", 2)]);
        assert_eq!(parsed("\"ss02\""), [feature(b"ss02", 1)]);
        assert_eq!(
            parsed("-calt, ss01"),
            [feature(b"calt", 0), feature(b"ss01", 1)]
        );
    }

    #[test]
    fn invalid_features_are_skipped() {
        assert!(parsed("ligature").is_empty());
        assert!(parsed("ss01=x").is_empty());
        assert!(parsed("").is_empty());
        assert_eq!(parsed("nopes, -liga"), [feature(b"liga", 0)]);
    }

    fn cells(font: &Font, text: &str) -> Vec<Vec<u8>> {
        (0..text.chars().count())
            .map(|index| {
                let mut pixels = Vec::new();
                font.rasterize_shaped(text, REGULAR, index, &mut pixels)
                    .unwrap();
                pixels
            })
            .collect()
    }

    fn alone(font: &Font, text: &str) -> Vec<Vec<u8>> {
        text.chars()
            .map(|character| {
                let mut pixels = Vec::new();
                font.rasterize(&character.to_string(), REGULAR, 1, &mut pixels);
                pixels
            })
            .collect()
    }

    fn with_features(features: &[&str]) -> Font {
        let features: Vec<String> = features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect();
        Font::with_symbols(DEFAULT_FONT_FAMILY, 13.0, 2.0, None, &[], &features).unwrap()
    }

    fn differing(font: &Font, text: &str) -> Vec<bool> {
        let shaped = font.shaped_cells(text, REGULAR);
        (0..text.chars().count())
            .map(|index| shaped.signature(index).is_some())
            .collect()
    }

    #[test]
    fn a_run_shapes_across_spaces() {
        let font = with_features(&[]);
        assert_eq!(
            differing(&font, "a != b"),
            [false, false, true, true, false, false]
        );
    }

    #[test]
    fn a_ligature_has_the_same_signature_whatever_surrounds_it() {
        let font = with_features(&[]);
        let signatures = |text: &str| -> Vec<Option<u64>> {
            let shaped = font.shaped_cells(text, REGULAR);
            (0..text.chars().count())
                .map(|index| shaped.signature(index))
                .collect()
        };
        let alone = signatures("!=");
        assert!(alone.iter().all(Option::is_some));
        assert_eq!(&signatures("a != b")[2..4], &alone[..]);
        assert_eq!(&signatures("xx!=yy")[2..4], &alone[..]);
        assert_ne!(alone[0], alone[1]);
    }

    #[test]
    fn a_ligature_looks_the_same_in_a_long_run() {
        let font = with_features(&[]);
        assert_eq!(
            &cells(&font, "some text != more")[10..12],
            &cells(&font, "!=")[..]
        );
    }

    #[test]
    fn a_ligature_changes_both_of_its_cells() {
        let font = with_features(&[]);
        assert_eq!(differing(&font, "!="), [true, true]);
        let (shaped, plain) = (cells(&font, "!="), alone(&font, "!="));
        assert_ne!(shaped[0], plain[0]);
        assert_ne!(shaped[1], plain[1]);
        assert_eq!(shaped[0].len(), plain[0].len());
    }

    #[test]
    fn disabling_liga_leaves_a_run_unshaped_and_enabling_a_set_shapes_it() {
        let font = with_features(&["-liga"]);
        assert_eq!(differing(&font, "!="), [false, false]);
        assert_eq!(cells(&font, "!="), alone(&font, "!="));
        // Monaspace builds `!=` from `liga`, so `calt` alone doesn't change it.
        assert_eq!(differing(&with_features(&["-calt"]), "!="), [true, true]);
        // `ss01` redraws `==`, which is plain by default.
        assert_eq!(differing(&with_features(&[]), "=="), [false, false]);
        assert_eq!(differing(&with_features(&["ss01"]), "=="), [true, true]);
    }

    #[test]
    fn runs_hold_single_codepoints_the_main_face_draws_itself() {
        let font = with_features(&[]);
        for joins in ['a', '!', 'é', '→'] {
            assert!(font.joins_run(joins), "{joins}");
        }
        assert!(font.joins_run(' '));
        // Other whitespace, procedural box drawing, icons and glyphs only a fallback face has.
        for apart in ['\u{A0}', '│', '\u{E0B0}', '世', '\u{1F600}'] {
            assert!(!font.joins_run(apart), "{apart:?}");
        }
    }

    #[test]
    fn a_run_with_non_ascii_cells_is_indexed_by_character() {
        let font = with_features(&[]);
        let shaped = differing(&font, "→é!=");
        assert_eq!(shaped.len(), 4);
        assert_eq!(&shaped[2..], [true, true]);
        assert_eq!(cells(&font, "→é!=").len(), 4);
    }

    #[test]
    fn every_cell_of_a_multi_cell_ligature_keeps_its_ink() {
        let font = with_features(&[]);
        let dots = |text: &str| cells(&font, text);
        // The font draws all three dots with the last glyph, so cells need glyphs beyond neighbours.
        for (cell, pixels) in dots("...").iter().enumerate() {
            assert!(ink_height(&font, pixels, 1) > 0, "cell {cell}");
        }
        let long = dots("abcdefghijklmnopqrstuvwxyz...");
        assert_eq!(&long[26..], &dots("...")[..]);
    }

    #[test]
    fn unrelated_neighbours_leave_a_run_as_plain_cells() {
        let font = with_features(&[]);
        assert!(!differing(&font, "{}").contains(&true));
    }
}
