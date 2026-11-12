use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::ptr::NonNull;
use std::rc::Rc;

use anyhow::{Context, Result, ensure};
use libghostty_vt::style::Underline;
use objc2_core_foundation::{
    CFArray, CFAttributedString, CFData, CFDictionary, CFNumber, CFRange, CFRetained, CFString,
    CFType, CGPoint, CGRect, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontManagerCreateFontDescriptorFromData, CTFontOrientation,
    CTFontSymbolicTraits, CTLine, CTRun, kCTFontAttributeName, kCTFontFeatureSettingsAttribute,
    kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue,
    kCTForegroundColorFromContextAttributeName,
};

use crate::config::DEFAULT_FONT_FAMILY;
use crate::constraint::{Bounds, Constraint, Metrics, Modifier, Size, is_private_use};
use crate::sprite;

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
                eprintln!("agentd: ignoring invalid font feature {spec:?}");
            }
            feature
        })
        .collect()
}

fn feature_dictionary(feature: FontFeature) -> CFRetained<CFDictionary<CFString, CFType>> {
    let tag = CFString::from_str(&String::from_utf8_lossy(&feature.tag));
    let value = CFNumber::new_i32(feature.value);
    let values: [&CFType; 2] = [tag.as_ref(), value.as_ref()];
    // SAFETY: the extern statics are immutable CoreFoundation constants.
    let keys = unsafe { [kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue] };
    CFDictionary::from_slices(&keys, &values)
}

/// `font` with `features` on top of CoreText's defaults, which already enable `calt` and `liga`.
fn with_features(font: CFRetained<CTFont>, features: &[FontFeature]) -> CFRetained<CTFont> {
    if features.is_empty() {
        return font;
    }
    let settings = CFArray::from_retained_objects(
        &features
            .iter()
            .map(|feature| feature_dictionary(*feature))
            .collect::<Vec<_>>(),
    );
    let values: [&CFType; 1] = [settings.as_ref()];
    // SAFETY: the extern static is an immutable CoreFoundation constant.
    let key = unsafe { kCTFontFeatureSettingsAttribute };
    let attributes = CFDictionary::<CFString, CFType>::from_slices(&[key], &values);
    // SAFETY: the dictionary holds the settings array CoreText documents for this key, a zero
    // size keeps the font's size and a null matrix means identity.
    unsafe {
        let descriptor = CTFontDescriptor::with_attributes(attributes.as_opaque());
        font.copy_with_attributes(0.0, std::ptr::null(), Some(&descriptor))
    }
}

/// A monospaced face rasterised into whole-cell RGBA bitmaps, in device pixels.
pub struct Font {
    /// Indexed by `bold + 2 * italic`.
    faces: [CFRetained<CTFont>; 4],
    symbols: CFRetained<CTFont>,
    color_space: Option<CFRetained<CGColorSpace>>,
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
    alone: RefCell<HashMap<(usize, char), Option<u16>>>,
    joins: RefCell<HashMap<char, bool>>,
    /// The horizontal extent of a glyph's ink from its origin, per style index and glyph.
    ink: RefCell<HashMap<(usize, u16), InkExtent>>,
}

/// Where a glyph's ink starts and ends horizontally, if it has any.
type InkExtent = Option<(f64, f64)>;

type RunCache = HashMap<Rc<str>, Rc<ShapedRun>, BuildHasherDefault<Fnv>>;

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

/// FNV-1a, because the run cache and glyph atlas hash every word and cell of every rebuild and
/// their keys are short.
#[derive(Default)]
pub(crate) struct Fnv(u64);

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

fn symbols_face(pixels: f64) -> Result<CFRetained<CTFont>> {
    // SAFETY: the data is live for the call, and a null matrix means identity.
    unsafe {
        let descriptor =
            CTFontManagerCreateFontDescriptorFromData(&CFData::from_static_bytes(SYMBOLS_FONT))
                .context("the bundled symbols font couldn't be loaded")?;
        Ok(CTFont::with_font_descriptor(
            &descriptor,
            pixels,
            std::ptr::null(),
        ))
    }
}

fn face(name: &str, pixels: f64) -> CFRetained<CTFont> {
    // SAFETY: a null matrix means identity.
    unsafe { CTFont::with_name(&CFString::from_str(name), pixels, std::ptr::null()) }
}

fn installed_face(family: &str, pixels: f64) -> Option<CFRetained<CTFont>> {
    let regular = face(family, pixels);
    // CoreText silently substitutes another family when a name isn't installed.
    // SAFETY: a plain getter on a live font.
    (unsafe { regular.family_name() }.to_string() == family).then_some(regular)
}

fn has_icons(font: &CTFont) -> bool {
    // A Powerline separator and a Font Awesome icon.
    let mut characters = [0xE0B0u16, 0xF432];
    let mut glyphs = [0u16; 2];
    // SAFETY: both arrays hold the two elements the count names.
    unsafe {
        font.glyphs_for_characters(
            NonNull::from(&mut characters).cast(),
            NonNull::from(&mut glyphs).cast(),
            2,
        )
    }
}

/// The face for private-use icons: the configured family, else the first Ghostty fallback, else the
/// main font, each only when it has the icons, else the bundled Symbols Nerd Font.
fn icon_face(
    main: &CFRetained<CTFont>,
    pixels: f64,
    configured: Option<&str>,
    fallbacks: &[String],
) -> Result<CFRetained<CTFont>> {
    let with_icons = |family: &str| installed_face(family, pixels).filter(|font| has_icons(font));
    if let Some(family) = configured {
        if let Some(font) = with_icons(family) {
            return Ok(font);
        }
        eprintln!(
            "agentd: symbols family {family} isn't installed or lacks icons; using the next choice"
        );
    }
    if let Some(font) = fallbacks.iter().find_map(|family| with_icons(family)) {
        return Ok(font);
    }
    if has_icons(main) {
        return Ok(main.clone());
    }
    symbols_face(pixels)
}

/// Regular, bold, italic and bold italic faces of `family`, or `None` if it isn't installed.
/// A style the family lacks reuses the regular face.
fn family_faces(family: &str, pixels: f64) -> Option<[CFRetained<CTFont>; 4]> {
    let regular = installed_face(family, pixels)?;
    let styled = |traits: CTFontSymbolicTraits| {
        // SAFETY: a zero size keeps the font's size and a null matrix means identity.
        unsafe { regular.copy_with_symbolic_traits(0.0, std::ptr::null(), traits, traits) }
            .unwrap_or_else(|| regular.clone())
    };
    let bold = styled(CTFontSymbolicTraits::TraitBold);
    let italic = styled(CTFontSymbolicTraits::TraitItalic);
    let bold_italic = styled(CTFontSymbolicTraits::TraitBold | CTFontSymbolicTraits::TraitItalic);
    Some([regular, bold, italic, bold_italic])
}

fn shaped_line(font: &CTFont, text: &str) -> Option<CFRetained<CTLine>> {
    // SAFETY: the extern statics are immutable CoreFoundation constants.
    let (font_key, color_key, enabled) = unsafe {
        (
            kCTFontAttributeName,
            kCTForegroundColorFromContextAttributeName,
            kCFBooleanTrue?,
        )
    };
    let values: [&CFType; 2] = [font.as_ref(), enabled.as_ref()];
    let attributes = CFDictionary::<CFString, CFType>::from_slices(&[font_key, color_key], &values);
    // SAFETY: the dictionary maps CFString attribute names to values of the types CoreText expects.
    let string = unsafe {
        CFAttributedString::new(
            None,
            Some(&CFString::from_str(text)),
            Some(attributes.as_opaque()),
        )
    }?;
    // SAFETY: `string` is a valid attributed string.
    Some(unsafe { CTLine::with_attributed_string(&string) })
}

struct Placed {
    glyph: u16,
    x: f64,
    y: f64,
    /// Where the glyph's ink starts and ends horizontally, if it has any.
    ink: Option<(f64, f64)>,
}

struct Shaped {
    glyphs: Vec<u16>,
    positions: Vec<CGPoint>,
}

fn shape(font: &CTFont, text: &str) -> Option<Shaped> {
    let line = shaped_line(font, text)?;
    // SAFETY: a plain getter on a live line.
    let runs = unsafe { line.glyph_runs() };
    // SAFETY: a line's glyph runs are `CTRun`s.
    let runs = unsafe { runs.cast_unchecked::<CTRun>() };
    let mut shaped = Shaped {
        glyphs: Vec::new(),
        positions: Vec::new(),
    };
    for run in runs.iter() {
        // SAFETY: plain getter.
        let count = unsafe { run.glyph_count() };
        let length = usize::try_from(count).ok()?;
        let range = CFRange {
            location: 0,
            length: count,
        };
        let (mut glyphs, mut positions) =
            (vec![0u16; length], vec![CGPoint { x: 0.0, y: 0.0 }; length]);
        // SAFETY: each buffer holds the `count` elements the range names.
        unsafe {
            run.glyphs(range, NonNull::new(glyphs.as_mut_ptr())?);
            run.positions(range, NonNull::new(positions.as_mut_ptr())?);
        }
        shaped.glyphs.append(&mut glyphs);
        shaped.positions.append(&mut positions);
    }
    Some(shaped)
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
    #[cfg(test)]
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
        let faces = family_faces(family, pixels)
            .or_else(|| {
                eprintln!(
                    "agentd: font family {family} isn't installed; using {DEFAULT_FONT_FAMILY}"
                );
                family_faces(DEFAULT_FONT_FAMILY, pixels)
            })
            .with_context(|| format!("neither {family} nor {DEFAULT_FONT_FAMILY} is installed"))?;
        let features = parse_features(features);
        let faces = faces.map(|face| with_features(face, &features));
        let regular = &faces[0];
        let mut character = u16::from(b'M');
        let mut glyph = 0u16;
        // SAFETY: both pointers are to one element, matching the count.
        let found = unsafe {
            regular.glyphs_for_characters(
                NonNull::from(&mut character),
                NonNull::from(&mut glyph),
                1,
            )
        };
        ensure!(found, "the terminal font has no glyph for M");
        // SAFETY: one glyph, and a null advances buffer is allowed when only the total is needed.
        let advance = unsafe {
            regular.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&mut glyph),
                std::ptr::null_mut(),
                1,
            )
        };
        // SAFETY: plain metric getters.
        let (
            ascent,
            descent,
            leading,
            underline_position,
            underline_thickness,
            x_height,
            cap_height,
        ) = unsafe {
            (
                regular.ascent(),
                regular.descent(),
                regular.leading(),
                regular.underline_position(),
                regular.underline_thickness(),
                regular.x_height(),
                regular.cap_height(),
            )
        };
        let face_height = ascent + descent + leading;
        let cell_height = face_height.ceil() as u32;
        let cell_width = advance.ceil() as u32;
        let baseline = (descent + leading / 2.0).round();
        let baseline_from_top = f64::from(cell_height) - baseline;
        let line_thickness = (underline_thickness.round() as u32).max(1);
        let half_line = f64::from(line_thickness) / 2.0;
        // Room for the second line of a double underline.
        let lowest_underline = cell_height.saturating_sub(3 * line_thickness);
        let symbols = icon_face(regular, pixels, symbols_family, fallback_families)?;
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
            color_space: CGColorSpace::new_device_rgb(),
        })
    }

    fn face_index(style: FontStyle) -> usize {
        usize::from(style.bold) + 2 * usize::from(style.italic)
    }

    /// A white-on-clear context over `out`, which must hold `4 * width * cell_height` bytes.
    fn bitmap_context(&self, out: &mut [u8], width: u32) -> Option<CFRetained<CGContext>> {
        // SAFETY: `out` holds that many bytes and outlives the context.
        let context = unsafe {
            CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                width as usize,
                self.cell_height as usize,
                8,
                width as usize * 4,
                self.color_space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }?;
        // LCD smoothing would give coverage glyphs unequal channels and read as colour.
        CGContext::set_should_smooth_fonts(Some(&context), false);
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
        Some(context)
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
        // CoreText's cascade never picks an installed font for private-use codepoints.
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
        let line = shaped_line(font, text)?;
        let context = self.bitmap_context(out, width)?;
        if let Some(constraint) = single.and_then(Constraint::of) {
            // SAFETY: `line` and `context` are live.
            let ink = unsafe { line.image_bounds(Some(&context)) };
            if ink.size.width > 0.0 && ink.size.height > 0.0 {
                let glyph = Bounds {
                    x: ink.origin.x,
                    y: ink.origin.y + self.baseline,
                    width: ink.size.width,
                    height: ink.size.height,
                };
                let mut target = constraint.constrain(glyph, self.metrics, cells);
                if constraint.size != Size::Stretch {
                    // Centre the face in a pixel cell wider than its advance.
                    target.x += (self.metrics.cell_width - self.metrics.face_width) / 2.0;
                }
                let (scale_x, scale_y) = (target.width / glyph.width, target.height / glyph.height);
                CGContext::translate_ctm(Some(&context), target.x, target.y);
                CGContext::scale_ctm(Some(&context), scale_x, scale_y);
                CGContext::translate_ctm(Some(&context), -glyph.x, -ink.origin.y);
                CGContext::set_text_position(Some(&context), 0.0, 0.0);
                // SAFETY: `line` and `context` are live, and `out` outlives the context.
                unsafe { line.draw(&context) };
                return Some(glyph_kind(out));
            }
        }
        // SAFETY: null out-parameters are allowed when only the width is needed.
        let line_width = unsafe {
            line.typographic_bounds(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let available = f64::from(width);
        // Fallback faces such as CJK are narrower than two terminal cells.
        let centred = ((available - line_width) / 2.0).max(0.0);
        CGContext::translate_ctm(Some(&context), centred, self.baseline);
        if line_width > available {
            let shrink = available / line_width;
            CGContext::scale_ctm(Some(&context), shrink, shrink);
        }
        CGContext::set_text_position(Some(&context), 0.0, 0.0);
        // SAFETY: `line` and `context` are live, and `out` outlives the context.
        unsafe { line.draw(&context) };
        Some(glyph_kind(out))
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
            let mut unit = [u16::try_from(u32::from(character)).unwrap_or(0)];
            let mut glyph = 0u16;
            let mut scratch = vec![0; (self.cell_width * self.cell_height * 4) as usize];
            !character.is_whitespace()
                && !character.is_control()
                && !is_private_use(character)
                && character.len_utf16() == 1
                // SAFETY: both pointers are to one element, matching the count.
                && unsafe {
                    self.faces[0]
                        .glyphs_for_characters(NonNull::from(&mut unit[0]), NonNull::from(&mut glyph), 1)
                }
                && !sprite::draw(
                    character,
                    self.cell_width,
                    self.cell_height,
                    self.line_thickness,
                    &mut scratch,
                )
        })
    }

    fn glyph_alone(&self, face: usize, character: char) -> Option<u16> {
        *self
            .alone
            .borrow_mut()
            .entry((face, character))
            .or_insert_with(|| {
                let shaped = shape(&self.faces[face], &character.to_string())?;
                (shaped.glyphs.len() == 1).then(|| shaped.glyphs[0])
            })
    }

    fn glyph_ink(&self, face: usize, glyph: u16) -> InkExtent {
        *self
            .ink
            .borrow_mut()
            .entry((face, glyph))
            .or_insert_with(|| {
                let mut glyph = glyph;
                let mut rect = CGRect::ZERO;
                // SAFETY: one glyph and one rect, matching the count of 1.
                unsafe {
                    self.faces[face].bounding_rects_for_glyphs(
                        CTFontOrientation::Horizontal,
                        NonNull::from(&mut glyph),
                        &mut rect,
                        1,
                    );
                }
                (rect.size.width > 0.0).then_some((rect.origin.x, rect.origin.x + rect.size.width))
            })
    }

    /// The glyphs of `shaped` placed in the cell grid, each with its ink's horizontal extent.
    /// A cell's glyph is centred in it and CoreText advances by the unrounded width.
    fn placed(&self, face: usize, shaped: &Shaped) -> Vec<Placed> {
        let ratio = f64::from(self.cell_width) / self.advance;
        let centred = (f64::from(self.cell_width) - self.advance) / 2.0;
        shaped
            .glyphs
            .iter()
            .zip(&shaped.positions)
            .map(|(glyph, position)| {
                let x = position.x * ratio + centred;
                let ink = self
                    .glyph_ink(face, *glyph)
                    .map(|(low, high)| (x + low * ratio, x + high * ratio));
                Placed {
                    glyph: *glyph,
                    x,
                    y: position.y + self.baseline,
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
        placed.iter().filter(move |glyph| {
            glyph
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
        let Some(shaped) = shape(&self.faces[face], run) else {
            return ShapedRun {
                text,
                signatures: vec![0; length].into(),
            };
        };
        let placed = self.placed(face, &shaped);
        // A monospaced font gives a ligature one glyph per character, though CoreText clusters
        // them under the first. Fewer glyphs than characters is a true ligature, so no cell
        // stands alone.
        let in_order = shaped.glyphs.len() == length;
        let differs: Vec<bool> = if in_order {
            run.chars()
                .zip(&shaped.glyphs)
                .zip(&shaped.positions)
                .zip(0..)
                .map(|(((character, glyph), position), cell): (_, u32)| {
                    self.glyph_alone(face, character) != Some(*glyph)
                        || (position.x - f64::from(cell) * self.advance).abs() > 0.5
                })
                .collect()
        } else {
            vec![true; length]
        };
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
                for glyph in self.painting(&placed, index) {
                    (glyph.glyph, ((glyph.x - origin) * 8.0).round() as i32).hash(&mut hasher);
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
        let placed = self.placed(face, &shape(font, run)?);
        out.clear();
        out.resize((self.cell_width * self.cell_height * 4) as usize, 0);
        let context = self.bitmap_context(out, self.cell_width)?;
        let origin = index as f64 * f64::from(self.cell_width);
        let (mut glyphs, mut positions): (Vec<u16>, Vec<CGPoint>) = self
            .painting(&placed, index)
            .map(|glyph| {
                (
                    glyph.glyph,
                    CGPoint {
                        x: glyph.x - origin,
                        y: glyph.y,
                    },
                )
            })
            .unzip();
        // SAFETY: both buffers hold `glyphs.len()` elements and `context` is live.
        unsafe {
            font.draw_glyphs(
                NonNull::new(glyphs.as_mut_ptr())?,
                NonNull::new(positions.as_mut_ptr())?,
                glyphs.len(),
                &context,
            );
        }
        Some(GlyphKind::Coverage)
    }

    /// One cell of a tiling underline, in the same format as `rasterize`.
    pub fn rasterize_underline(&self, kind: Underline, out: &mut Vec<u8>) {
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
        // SAFETY: plain metric getters on a live font.
        let (cap, ascent, descent, leading) = unsafe {
            let face = &font.faces[0];
            (
                face.cap_height(),
                face.ascent(),
                face.descent(),
                face.leading(),
            )
        };
        (2.0 * cap + ascent + descent + leading) / 3.0
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
        font.faces
            .iter()
            // SAFETY: a plain getter on a live font.
            .map(|face| unsafe { face.post_script_name() }.to_string())
            .collect()
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
        // SAFETY: a plain getter on a live font.
        let name = unsafe { font.symbols.post_script_name() }.to_string();
        assert_eq!(name, "SymbolsNF");
    }

    fn symbols_name(font: &Font) -> String {
        // SAFETY: a plain getter on a live font.
        unsafe { font.symbols.post_script_name() }.to_string()
    }

    /// CI doesn't have Caskaydia; tests that need it skip.
    fn nerd_family_installed() -> bool {
        family_faces(NERD_FAMILY, 26.0).is_some()
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

    #[test]
    fn a_feature_becomes_a_coretext_tag_and_value_dictionary() {
        let dictionary = feature_dictionary(feature(b"liga", 0));
        // SAFETY: the extern statics are immutable CoreFoundation constants.
        let (tag, value) = unsafe { (kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue) };
        let tag = dictionary.get(tag).unwrap();
        let value = dictionary.get(value).unwrap();
        assert_eq!(tag.downcast_ref::<CFString>().unwrap().to_string(), "liga");
        assert_eq!(value.downcast_ref::<CFNumber>().unwrap().as_i32(), Some(0));
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
