use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use anyhow::{Context, Result};
use fontique::{
    Blob, Collection, CollectionOptions, FallbackKey, FamilyId, FamilyInfo, FontStyle as Slant,
    FontWeight, FontWidth, GenericFamily, Script, SourceCache, Synthesis,
};
use harfrust::{Feature, ShapeOptions, ShaperData, ShaperInstance, Tag, UnicodeBuffer, Variation};
use icu_properties::props::{EmojiPresentation, Script as UnicodeScript};
use icu_properties::{CodePointMapData, CodePointSetData, PropertyNamesShort};
use swash::CacheKey;
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith, image::Content};
use swash::zeno::{Angle, Format, Transform};
use unicode_segmentation::UnicodeSegmentation;

use crate::constraint::Bounds;
use crate::{
    Backend, FaceMetrics, Fnv, FnvMap, Font, FontFeature, FontStyle, Glyph, InkExtent, Placement,
};

/// Fonts found through fontique, shaped by harfrust and drawn by swash.
pub(crate) struct Swash {
    shared: RefCell<Shared>,
    /// Whether this backend has rescanned the system fonts for a missing family.
    rescanned: Cell<bool>,
}

/// One `Font`'s font services, so one pixel size.
struct Shared {
    collection: Collection,
    sources: SourceCache,
    scale: ScaleContext,
    /// Each family's font file per style index, so fallback scans after the first only read
    /// charmaps; `None` when the family has no usable font.
    files: FnvMap<(FamilyId, usize), Option<Matched>>,
    /// Fallback fonts by family and style index, without features.
    fonts: FnvMap<(FamilyId, usize), Option<Rc<FaceData>>>,
    /// The family each cluster falls back to, per style index; `None` when no family has it.
    clusters: [FnvMap<String, Option<FamilyId>>; 4],
}

/// The latest scan of the system fonts, which every backend's collection clones.
static SYSTEM_FONTS: LazyLock<Mutex<(Collection, SourceCache)>> =
    LazyLock::new(|| Mutex::new((scan_system_fonts(), SourceCache::new_shared())));

fn scan_system_fonts() -> Collection {
    Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    })
}

#[derive(Clone)]
pub(crate) struct Face {
    data: Rc<FaceData>,
    features: Rc<[Feature]>,
}

struct FaceData {
    blob: Blob<u8>,
    index: u32,
    /// Where the font's table directory starts in `blob`, for swash.
    offset: u32,
    key: CacheKey,
    pixels: f32,
    family: Option<FamilyId>,
    style: FontStyle,
    /// The outline growth of a synthesised bold, in pixels.
    embolden: f32,
    skew: bool,
    coords: Vec<i16>,
    instance: ShaperInstance,
    shaper: ShaperData,
}

/// A font file and what it needs synthesised to match a style.
#[derive(Clone)]
struct Matched {
    blob: Blob<u8>,
    index: u32,
    /// Where the font's table directory starts in `blob`, for swash.
    offset: u32,
    key: CacheKey,
    synthesis: Synthesis,
}

impl Matched {
    fn new(blob: Blob<u8>, index: u32, synthesis: Synthesis) -> Option<Self> {
        let font = swash::FontRef::from_index(blob.data(), usize::try_from(index).ok()?)?;
        let (offset, key) = (font.offset, font.key);
        Some(Self {
            blob,
            index,
            offset,
            key,
            synthesis,
        })
    }

    fn swash(&self) -> swash::FontRef<'_> {
        swash::FontRef {
            data: self.blob.data(),
            offset: self.offset,
            key: self.key,
        }
    }
}

/// Text in runs of the faces that draw it, with each glyph's origin from the start of the line.
pub(crate) struct Line {
    runs: Vec<(Face, Vec<Glyph>)>,
    width: f64,
}

pub(crate) struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
}

/// Ghostty's synthetic italic angle for FreeType faces.
const SYNTHETIC_SKEW: f32 = 12.0;

/// Bitmap strikes come last, for bitmap-only fonts such as Terminus's OTB files.
const SOURCES: [Source; 4] = [
    Source::ColorOutline(0),
    Source::ColorBitmap(StrikeWith::BestFit),
    Source::Outline,
    Source::Bitmap(StrikeWith::BestFit),
];
/// The sources that draw in colour, which swash supports as COLR v0 and bitmaps only.
const COLOR_SOURCES: &[Source] = SOURCES.split_at(2).0;

/// `family`'s closest match to `style`.
fn matched(shared: &mut Shared, family: &FamilyInfo, style: FontStyle) -> Option<Matched> {
    let slant = if style.italic {
        Slant::Italic
    } else {
        Slant::Normal
    };
    let weight = if style.bold {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    let font = family.match_font(FontWidth::NORMAL, slant, weight, true)?;
    Matched::new(
        font.load(Some(&mut shared.sources))?,
        font.index(),
        font.synthesis(FontWidth::NORMAL, slant, weight),
    )
}

/// Whether `font` has a glyph for every character of `cluster`, each drawn in colour if `colour`.
fn fits(
    font: swash::FontRef<'_>,
    scale: &mut ScaleContext,
    pixels: f32,
    cluster: &str,
    colour: bool,
) -> bool {
    let charmap = font.charmap();
    let mut glyphs = cluster
        .chars()
        .filter(|&character| !is_joiner(character))
        .map(|character| charmap.map(character));
    if !colour {
        return glyphs.all(|glyph| glyph != 0);
    }
    let mut scaler = scale.builder(font).size(pixels).build();
    glyphs.all(|glyph| {
        glyph != 0
            && Render::new(COLOR_SOURCES)
                .render(&mut scaler, glyph)
                .is_some()
    })
}

impl FaceData {
    fn new(
        Matched {
            blob,
            index,
            offset,
            key,
            synthesis,
        }: Matched,
        pixels: f64,
        family: Option<FamilyId>,
        style: FontStyle,
    ) -> Option<Self> {
        let font = harfrust::FontRef::from_index(blob.data(), index).ok()?;
        // fontique 0.12 asks for `slnt=14` where the axis leans right at negative angles.
        let variations = synthesis
            .variation_settings()
            .iter()
            .map(|&(tag, value)| Variation {
                tag,
                value: if tag == Tag::new(b"slnt") {
                    -value
                } else {
                    value
                },
            });
        let instance = ShaperInstance::from_variations(&font, variations);
        let coords: Vec<i16> = instance
            .coords()
            .iter()
            .map(|coord| coord.to_bits())
            .collect();
        let pixels = pixels as f32;
        let embolden = if synthesis.embolden() {
            // Ghostty's FreeType heuristic: 1/64 pixel per 2048 units of 26.6 face height.
            let swash = swash::FontRef {
                data: blob.data(),
                offset,
                key,
            };
            let metrics = swash.metrics(&coords).scale(pixels);
            ((metrics.ascent + metrics.descent + metrics.leading) * 2.0).ceil() / 64.0
        } else {
            0.0
        };
        Some(Self {
            offset,
            key,
            shaper: ShaperData::new(&font),
            blob,
            index,
            pixels,
            family,
            style,
            embolden,
            skew: synthesis.skew().is_some(),
            coords,
            instance,
        })
    }

    fn swash(&self) -> swash::FontRef<'_> {
        swash::FontRef {
            data: self.blob.data(),
            offset: self.offset,
            key: self.key,
        }
    }

    fn scaler<'a>(&'a self, context: &'a mut ScaleContext) -> Scaler<'a> {
        context
            .builder(self.swash())
            // CoreText doesn't hint either, and hinting would move glyphs off Ghostty's metrics.
            .hint(false)
            .size(self.pixels)
            .normalized_coords(&self.coords)
            .build()
    }

    fn skew(&self) -> Transform {
        if self.skew {
            Transform::skew(Angle::from_degrees(SYNTHETIC_SKEW), Angle::ZERO)
        } else {
            Transform::IDENTITY
        }
    }

    /// The glyph's ink in pixels, up from its origin.
    fn ink(&self, context: &mut ScaleContext, glyph: u16) -> Option<Bounds> {
        let mut scaler = self.scaler(context);
        let outline = scaler
            .scale_color_outline(glyph)
            .or_else(|| scaler.scale_outline(glyph))
            .filter(|outline| !outline.is_empty());
        if let Some(mut outline) = outline {
            if self.embolden != 0.0 {
                outline.embolden(self.embolden, self.embolden);
            }
            outline.transform(&self.skew());
            let bounds = outline.bounds();
            return Some(Bounds {
                x: f64::from(bounds.min.x),
                y: f64::from(bounds.min.y),
                width: f64::from(bounds.width()),
                height: f64::from(bounds.height()),
            });
        }
        let placement = Render::new(&SOURCES).render(&mut scaler, glyph)?.placement;
        Some(Bounds {
            x: f64::from(placement.left),
            y: f64::from(placement.top) - f64::from(placement.height),
            width: f64::from(placement.width),
            height: f64::from(placement.height),
        })
    }
}

impl Face {
    fn new(data: FaceData) -> Self {
        Self {
            data: Rc::new(data),
            features: Rc::from([]),
        }
    }

    /// Faces in one line share the features of the face it was built from.
    fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.data, &other.data)
    }
}

/// Characters that join a cluster without needing a glyph of their own.
fn is_joiner(character: char) -> bool {
    matches!(character, '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Whether `cluster` is an emoji drawn in colour: emoji presentation by default, or asked for.
fn is_emoji(cluster: &str) -> bool {
    let Some(first) = cluster.chars().next() else {
        return false;
    };
    !cluster.contains('\u{FE0E}')
        && (cluster.contains('\u{FE0F}')
            || CodePointSetData::new::<EmojiPresentation>().contains(first))
}

/// The cluster's script, if it has one more specific than Common or Inherited.
fn script(cluster: &str) -> Option<Script> {
    cluster.chars().find_map(|character| {
        let script = CodePointMapData::<UnicodeScript>::new().get(character);
        if matches!(
            script,
            UnicodeScript::Common | UnicodeScript::Inherited | UnicodeScript::Unknown
        ) {
            return None;
        }
        Script::parse(PropertyNamesShort::<UnicodeScript>::new().get(script)?).ok()
    })
}

impl Swash {
    fn face(&self, family: &FamilyInfo, style: FontStyle, pixels: f64) -> Option<Face> {
        let font = matched(&mut self.shared.borrow_mut(), family, style)?;
        FaceData::new(font, pixels, Some(family.id()), style).map(Face::new)
    }

    /// The face that draws `cluster`: `face` itself when it can, else a fallback with its features.
    fn face_for(&self, face: &Face, cluster: &str) -> Option<Face> {
        let data = &face.data;
        let mut shared = self.shared.borrow_mut();
        let emoji = is_emoji(cluster);
        if fits(data.swash(), &mut shared.scale, data.pixels, cluster, emoji) {
            return Some(face.clone());
        }
        let style = Font::face_index(data.style);
        let family = match shared.clusters[style].get(cluster) {
            Some(family) => *family,
            None => {
                // An emoji no font colours, such as with only a COLR v1 font installed, takes a
                // monochrome glyph, the face's own first.
                let family = fallback_family(&mut shared, data, cluster, emoji).or_else(|| {
                    let monochrome = emoji
                        && !fits(data.swash(), &mut shared.scale, data.pixels, cluster, false);
                    monochrome
                        .then(|| fallback_family(&mut shared, data, cluster, false))
                        .flatten()
                });
                shared.clusters[style].insert(cluster.to_owned(), family);
                family
            }
        }?;
        let fallback = shared.fonts.get(&(family, style)).cloned().flatten()?;
        Some(Face {
            data: fallback,
            features: face.features.clone(),
        })
    }
}

/// The first family to draw `cluster` in `data`'s style, in colour if `colour`: an emoji font for
/// emoji, then the system's fallback for the cluster's script, then the generic families, then
/// any family. Its font is left in `shared.fonts`.
fn fallback_family(
    shared: &mut Shared,
    data: &FaceData,
    cluster: &str,
    colour: bool,
) -> Option<FamilyId> {
    let mut candidates = Vec::new();
    if colour {
        candidates.extend(
            shared
                .collection
                .fallback_families(FallbackKey::from(Script::from_bytes(*b"Zsye"))),
        );
        candidates.extend(shared.collection.generic_families(GenericFamily::Emoji));
    }
    if let Some(script) = script(cluster) {
        candidates.extend(
            shared
                .collection
                .fallback_families(FallbackKey::from(script)),
        );
    }
    for generic in [
        GenericFamily::Monospace,
        GenericFamily::SansSerif,
        GenericFamily::Emoji,
    ] {
        candidates.extend(shared.collection.generic_families(generic));
    }
    candidates.extend(shared.collection.family_ids());
    let mut tried = HashSet::<FamilyId, BuildHasherDefault<Fnv>>::default();
    candidates
        .into_iter()
        .filter(|&id| tried.insert(id))
        .find(|&id| draws(shared, data, id, cluster, colour))
}

/// Whether family `id` draws `cluster` in `data`'s style, leaving its font in `shared.fonts`.
fn draws(shared: &mut Shared, data: &FaceData, id: FamilyId, cluster: &str, colour: bool) -> bool {
    let style = Font::face_index(data.style);
    let file = match shared.files.get(&(id, style)) {
        Some(file) => file.clone(),
        None => {
            let file = shared
                .collection
                .family(id)
                .and_then(|family| matched(shared, &family, data.style));
            shared.files.insert((id, style), file.clone());
            file
        }
    };
    let Some(file) = file else {
        return false;
    };
    if !fits(
        file.swash(),
        &mut shared.scale,
        data.pixels,
        cluster,
        colour,
    ) {
        return false;
    }
    shared
        .fonts
        .entry((id, style))
        .or_insert_with(|| {
            FaceData::new(file, f64::from(data.pixels), Some(id), data.style).map(Rc::new)
        })
        .is_some()
}

/// The glyphs of `text` in `face` alone, and their total advance.
fn shape(face: &Face, text: &str) -> Option<(Vec<Glyph>, f64)> {
    let data = &face.data;
    let font = harfrust::FontRef::from_index(data.blob.data(), data.index).ok()?;
    let shaper = data
        .shaper
        .shaper(&font)
        .instance(Some(&data.instance))
        .build();
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let shaped = shaper.shape(buffer, ShapeOptions::new().features(&face.features));
    let scale = f64::from(data.pixels) / f64::from(shaper.units_per_em());
    let mut pen = 0.0;
    let mut glyphs = Vec::with_capacity(shaped.len());
    for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        glyphs.push(Glyph {
            id: u16::try_from(info.glyph_id).ok()?,
            x: pen + f64::from(position.x_offset) * scale,
            y: f64::from(position.y_offset) * scale,
        });
        pen += f64::from(position.x_advance) * scale;
    }
    Some((glyphs, pen))
}

/// Source-over of one premultiplied pixel.
fn blend(pixel: &mut [u8], source: [u8; 4]) {
    let keep = 255 - u16::from(source[3]);
    for (out, source) in pixel.iter_mut().zip(source) {
        *out = (u16::from(source) + (u16::from(*out) * keep + 127) / 255).min(255) as u8;
    }
}

impl Canvas<'_> {
    fn pixel(&mut self, column: i64, row: i64) -> Option<&mut [u8]> {
        if column < 0 || row < 0 || column >= i64::from(self.width) || row >= i64::from(self.height)
        {
            return None;
        }
        let start = (row as usize * self.width as usize + column as usize) * 4;
        self.pixels.get_mut(start..start + 4)
    }

    /// Composites an image `width` by `height` whose top-left corner is at `left`, `top` (pixels
    /// from the canvas's top-left), reading premultiplied pixels by index from `texel`.
    fn blit(
        &mut self,
        (width, height): (u32, u32),
        (left, top): (i32, i32),
        texel: impl Fn(usize) -> [u8; 4],
    ) {
        for y in 0..height {
            for x in 0..width {
                let index = y as usize * width as usize + x as usize;
                let at = (
                    i64::from(left) + i64::from(x),
                    i64::from(top) + i64::from(y),
                );
                if let Some(pixel) = self.pixel(at.0, at.1) {
                    blend(pixel, texel(index));
                }
            }
        }
    }

    /// Composites premultiplied RGBA `width` by `height` stretched by `scale`, with its top-left
    /// corner at `left`, `top` (pixels from the canvas's top-left), sampling bilinearly.
    fn stretch(
        &mut self,
        rgba: &[u8],
        (width, height): (u32, u32),
        (left, top): (f64, f64),
        scale: (f64, f64),
    ) {
        let texel = |column: i64, row: i64| -> [f64; 4] {
            if column < 0 || row < 0 || column >= i64::from(width) || row >= i64::from(height) {
                return [0.0; 4];
            }
            let start = (row as usize * width as usize + column as usize) * 4;
            std::array::from_fn(|channel| f64::from(rgba[start + channel]))
        };
        // A canvas pixel's centre, mapped into the image: the texel at or before it, and the
        // weight of the next texel.
        let source = |position: i64, start: f64, scale: f64| {
            let mapped = (position as f64 + 0.5 - start) / scale - 0.5;
            (mapped.floor() as i64, mapped - mapped.floor())
        };
        let right = (left + f64::from(width) * scale.0).ceil() as i64;
        let bottom = (top + f64::from(height) * scale.1).ceil() as i64;
        for row in (top.floor() as i64)..bottom {
            let (source_row, below) = source(row, top, scale.1);
            for column in (left.floor() as i64)..right {
                let (source_column, beside) = source(column, left, scale.0);
                let (top_left, top_right, bottom_left, bottom_right) = (
                    texel(source_column, source_row),
                    texel(source_column + 1, source_row),
                    texel(source_column, source_row + 1),
                    texel(source_column + 1, source_row + 1),
                );
                let sample = std::array::from_fn(|channel| {
                    let upper = top_left[channel] * (1.0 - beside) + top_right[channel] * beside;
                    let lower =
                        bottom_left[channel] * (1.0 - beside) + bottom_right[channel] * beside;
                    (upper * (1.0 - below) + lower * below).round() as u8
                });
                if let Some(pixel) = self.pixel(column, row) {
                    blend(pixel, sample);
                }
            }
        }
    }

    /// Draws `glyph` with its origin at `origin`, in pixels up from the canvas's bottom-left, with
    /// its outline stretched by `scale`.
    fn draw(
        &mut self,
        context: &mut ScaleContext,
        face: &Face,
        glyph: u16,
        origin: (f64, f64),
        scale: (f64, f64),
    ) {
        let data = &face.data;
        let image = {
            let mut scaler = data.scaler(context);
            let transform = data
                .skew()
                .then(&Transform::scale(scale.0 as f32, scale.1 as f32))
                .then_translate(origin.0 as f32, origin.1 as f32);
            Render::new(&SOURCES)
                .format(Format::Alpha)
                .embolden(data.embolden)
                .transform(Some(transform))
                .render(&mut scaler, glyph)
        };
        let Some(image) = image else {
            return;
        };
        let placement = image.placement;
        let size = (placement.width, placement.height);
        let pixels = &image.data;
        match (image.source, image.content) {
            // swash places bitmaps at the font's size and ignores the transform.
            (Source::ColorBitmap(_) | Source::Bitmap(_), content) => {
                let rgba = if content == Content::Mask {
                    pixels.iter().flat_map(|&alpha| [alpha; 4]).collect()
                } else {
                    premultiplied(pixels)
                };
                let left = origin.0 + f64::from(placement.left) * scale.0;
                let top = f64::from(self.height) - (origin.1 + f64::from(placement.top) * scale.1);
                self.stretch(&rgba, size, (left, top), scale);
            }
            (_, content) => {
                let at = (placement.left, self.height as i32 - placement.top);
                if content == Content::Mask {
                    self.blit(size, at, |index| {
                        [pixels.get(index).copied().unwrap_or(0); 4]
                    });
                } else {
                    // swash composites colour outline layers premultiplied.
                    self.blit(size, at, |index| {
                        std::array::from_fn(|channel| {
                            pixels.get(index * 4 + channel).copied().unwrap_or(0)
                        })
                    });
                }
            }
        }
    }
}

fn premultiplied(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[red, green, blue, alpha]| {
            let scale = |channel: u8| ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8;
            [scale(red), scale(green), scale(blue), alpha]
        })
        .collect()
}

impl Backend for Swash {
    type Face = Face;
    type Line = Line;
    type Canvas<'a> = Canvas<'a>;

    fn new() -> Self {
        let (collection, sources) = SYSTEM_FONTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Self {
            shared: RefCell::new(Shared {
                collection,
                sources,
                scale: ScaleContext::new(),
                files: FnvMap::default(),
                fonts: FnvMap::default(),
                clusters: Default::default(),
            }),
            rescanned: Cell::new(false),
        }
    }

    /// A family installed since the last scan is found after one rescan per backend, as each
    /// config reload builds new backends.
    fn installed_face(&self, family: &str, pixels: f64) -> Option<Face> {
        let lookup = || self.shared.borrow_mut().collection.family_by_name(family);
        let found = lookup().or_else(|| {
            if self.rescanned.replace(true) {
                return None;
            }
            let collection = scan_system_fonts();
            SYSTEM_FONTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .0 = collection.clone();
            self.shared.borrow_mut().collection = collection;
            lookup()
        })?;
        self.face(&found, FontStyle::default(), pixels)
    }

    fn monospace_family(&self) -> Option<String> {
        let mut shared = self.shared.borrow_mut();
        let id = shared
            .collection
            .generic_families(GenericFamily::Monospace)
            .next()?;
        shared.collection.family_name(id).map(str::to_owned)
    }

    /// A style the family lacks is synthesised, as Ghostty does with FreeType.
    fn styled(&self, face: &Face, style: FontStyle) -> Option<Face> {
        let family = self
            .shared
            .borrow_mut()
            .collection
            .family(face.data.family?)?;
        let styled = self.face(&family, style, f64::from(face.data.pixels))?;
        let unchanged = styled.data.blob.id() == face.data.blob.id()
            && styled.data.index == face.data.index
            && styled.data.coords == face.data.coords
            && styled.data.embolden == 0.0
            && !styled.data.skew;
        (!unchanged).then_some(styled)
    }

    fn embedded_face(&self, bytes: &'static [u8], pixels: f64) -> Result<Face> {
        let font = Matched::new(Blob::new(Arc::new(bytes)), 0, Synthesis::default())
            .context("swash couldn't read the font data")?;
        FaceData::new(font, pixels, None, FontStyle::default())
            .map(Face::new)
            .context("swash couldn't read the font data")
    }

    fn with_features(face: Face, features: &[FontFeature]) -> Face {
        let features = features
            .iter()
            .map(|feature| {
                Feature::new(
                    Tag::new(&feature.tag),
                    u32::try_from(feature.value).unwrap_or(0),
                    ..,
                )
            })
            .collect();
        Face { features, ..face }
    }

    fn glyph(face: &Face, character: char) -> Option<u16> {
        Some(face.data.swash().charmap().map(character)).filter(|&glyph| glyph != 0)
    }

    fn advance(face: &Face, glyph: u16) -> f64 {
        let data = &face.data;
        f64::from(
            data.swash()
                .glyph_metrics(&data.coords)
                .scale(data.pixels)
                .advance_width(glyph),
        )
    }

    fn metrics(&self, face: &Face) -> FaceMetrics {
        let data = &face.data;
        let metrics = data.swash().metrics(&data.coords).scale(data.pixels);
        // Fonts with an old OS/2 table lack these; Ghostty measures the glyphs instead.
        let measured = |height: f32, character: char| {
            if height > 0.0 {
                return f64::from(height);
            }
            Self::glyph(face, character)
                .and_then(|glyph| data.ink(&mut self.shared.borrow_mut().scale, glyph))
                .map_or(0.0, |ink| ink.height)
        };
        FaceMetrics {
            ascent: f64::from(metrics.ascent),
            descent: f64::from(metrics.descent),
            leading: f64::from(metrics.leading),
            underline_position: f64::from(metrics.underline_offset),
            underline_thickness: f64::from(metrics.stroke_size),
            x_height: measured(metrics.x_height, 'x'),
            cap_height: measured(metrics.cap_height, 'H'),
        }
    }

    fn shape(&self, face: &Face, text: &str) -> Option<Vec<Glyph>> {
        shape(face, text).map(|(glyphs, _)| glyphs)
    }

    fn glyph_ink(&self, face: &Face, glyph: u16) -> InkExtent {
        let ink = face.data.ink(&mut self.shared.borrow_mut().scale, glyph)?;
        (ink.width > 0.0).then_some((ink.x, ink.x + ink.width))
    }

    fn line(&self, face: &Face, text: &str) -> Option<Line> {
        let mut runs: Vec<(Face, String)> = Vec::new();
        for cluster in text.graphemes(true) {
            // A cluster no font has draws as the face's missing glyph.
            let chosen = self.face_for(face, cluster).unwrap_or_else(|| face.clone());
            match runs.last_mut() {
                Some((last, run)) if last.same(&chosen) => run.push_str(cluster),
                _ => runs.push((chosen, cluster.to_owned())),
            }
        }
        let mut line = Line {
            runs: Vec::with_capacity(runs.len()),
            width: 0.0,
        };
        for (face, text) in runs {
            let (mut glyphs, advance) = shape(&face, &text)?;
            for glyph in &mut glyphs {
                glyph.x += line.width;
            }
            line.width += advance;
            line.runs.push((face, glyphs));
        }
        Some(line)
    }

    fn line_width(line: &Line) -> f64 {
        line.width
    }

    fn canvas<'a>(&self, out: &'a mut [u8], width: u32, height: u32) -> Option<Canvas<'a>> {
        (out.len() >= width as usize * height as usize * 4).then_some(Canvas {
            pixels: out,
            width,
            height,
        })
    }

    fn line_ink(&self, _canvas: &Canvas<'_>, line: &Line) -> Bounds {
        let corners = line.runs.iter().flat_map(|(face, glyphs)| {
            glyphs.iter().filter_map(|glyph| {
                let ink = face
                    .data
                    .ink(&mut self.shared.borrow_mut().scale, glyph.id)
                    .filter(|ink| ink.width > 0.0)?;
                let low = (glyph.x + ink.x, glyph.y + ink.y);
                Some((low, (low.0 + ink.width, low.1 + ink.height)))
            })
        });
        let union = corners.reduce(|(low, high), (glyph_low, glyph_high)| {
            (
                (low.0.min(glyph_low.0), low.1.min(glyph_low.1)),
                (high.0.max(glyph_high.0), high.1.max(glyph_high.1)),
            )
        });
        let ((x, y), (right, top)) = union.unwrap_or_default();
        Bounds {
            x,
            y,
            width: right - x,
            height: top - y,
        }
    }

    fn draw_line(&self, canvas: &mut Canvas<'_>, line: &Line, placement: Placement) {
        let mut shared = self.shared.borrow_mut();
        let Placement {
            offset,
            scale,
            origin,
        } = placement;
        for (face, glyphs) in &line.runs {
            for glyph in glyphs {
                let at = (
                    offset.0 + scale.0 * (glyph.x - origin.0),
                    offset.1 + scale.1 * (glyph.y - origin.1),
                );
                canvas.draw(&mut shared.scale, face, glyph.id, at, scale);
            }
        }
    }

    fn draw_glyphs(
        &self,
        canvas: &mut Canvas<'_>,
        face: &Face,
        glyphs: impl IntoIterator<Item = Glyph>,
    ) {
        let mut shared = self.shared.borrow_mut();
        for glyph in glyphs {
            canvas.draw(
                &mut shared.scale,
                face,
                glyph.id,
                (glyph.x, glyph.y),
                (1.0, 1.0),
            );
        }
    }

    #[cfg(test)]
    fn postscript_name(face: &Face) -> String {
        face.data
            .swash()
            .localized_strings()
            .find_by_id(swash::StringId::PostScript, None)
            .map(|name| name.to_string())
            .unwrap_or_default()
    }
}

/// The constraint scale groups and Ghostty's metrics were measured on CoreText's numbers.
#[cfg(all(test, target_os = "macos"))]
mod coretext_parity {
    use super::*;
    use crate::coretext::CoreText;
    use crate::{DEFAULT_FONT_FAMILY, SYMBOLS_FONT};

    const PIXELS: f64 = 26.0;

    fn assert_close(what: &str, coretext: f64, swash: f64) {
        assert!(
            (coretext - swash).abs() <= 0.5,
            "{what}: CoreText {coretext}, swash {swash}"
        );
    }

    /// `Font` takes its metrics from the main face only.
    #[test]
    fn main_face_metrics_match_coretext() {
        let (coretext, swash) = (CoreText::new(), Swash::new());
        let expected = coretext.metrics(
            &coretext
                .installed_face(DEFAULT_FONT_FAMILY, PIXELS)
                .unwrap(),
        );
        let actual = swash.metrics(&swash.installed_face(DEFAULT_FONT_FAMILY, PIXELS).unwrap());
        for (what, expected, actual) in [
            ("ascent", expected.ascent, actual.ascent),
            ("descent", expected.descent, actual.descent),
            ("leading", expected.leading, actual.leading),
            (
                "underline position",
                expected.underline_position,
                actual.underline_position,
            ),
            (
                "underline thickness",
                expected.underline_thickness,
                actual.underline_thickness,
            ),
            ("x height", expected.x_height, actual.x_height),
            ("cap height", expected.cap_height, actual.cap_height),
        ] {
            assert_close(what, expected, actual);
        }
    }

    #[test]
    fn glyph_advance_and_ink_match_coretext() {
        let (coretext, swash) = (CoreText::new(), Swash::new());
        let (mono, mono_swash) = (
            coretext
                .installed_face(DEFAULT_FONT_FAMILY, PIXELS)
                .unwrap(),
            swash.installed_face(DEFAULT_FONT_FAMILY, PIXELS).unwrap(),
        );
        let (symbols, symbols_swash) = (
            coretext.embedded_face(SYMBOLS_FONT, PIXELS).unwrap(),
            swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap(),
        );
        let characters = [
            (&mono, &mono_swash, 'M'),
            (&mono, &mono_swash, 'g'),
            (&mono, &mono_swash, '→'),
            (&symbols, &symbols_swash, '\u{E0B0}'),
            (&symbols, &symbols_swash, '\u{E0C6}'),
            (&symbols, &symbols_swash, '\u{F0E7}'),
            (&symbols, &symbols_swash, '\u{F418}'),
            (&symbols, &symbols_swash, '\u{F432}'),
        ];
        let mut pixels = vec![0; 4 * 64 * 64];
        for (ours, theirs, character) in characters {
            let glyph = CoreText::glyph(ours, character).unwrap();
            assert_eq!(
                Swash::glyph(theirs, character),
                Some(glyph),
                "{character:?}"
            );
            let what = |name: &str| format!("{character:?} {name}");
            assert_close(
                &what("advance"),
                CoreText::advance(ours, glyph),
                Swash::advance(theirs, glyph),
            );
            let (expected, actual) = (
                coretext.glyph_ink(ours, glyph).unwrap(),
                swash.glyph_ink(theirs, glyph).unwrap(),
            );
            assert_close(&what("ink left"), expected.0, actual.0);
            assert_close(&what("ink right"), expected.1, actual.1);
            let text = character.to_string();
            let expected = {
                let canvas = coretext.canvas(&mut pixels, 64, 64).unwrap();
                coretext.line_ink(&canvas, &coretext.line(ours, &text).unwrap())
            };
            let actual = {
                let canvas = swash.canvas(&mut pixels, 64, 64).unwrap();
                swash.line_ink(&canvas, &swash.line(theirs, &text).unwrap())
            };
            for (name, expected, actual) in [
                ("line ink x", expected.x, actual.x),
                ("line ink y", expected.y, actual.y),
                ("line ink width", expected.width, actual.width),
                ("line ink height", expected.height, actual.height),
            ] {
                assert_close(&what(name), expected, actual);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SYMBOLS_FONT;

    const PIXELS: f64 = 26.0;
    const BOLT: char = '\u{F0E7}';

    /// The bundled Symbols Nerd Font as an installed family, which has only a regular face.
    fn symbols(swash: &Swash, style: FontStyle) -> Face {
        swash
            .shared
            .borrow_mut()
            .collection
            .register_fonts(Blob::new(Arc::new(SYMBOLS_FONT)), None);
        let regular = swash.installed_face("Symbols Nerd Font", PIXELS).unwrap();
        if style == FontStyle::default() {
            return regular;
        }
        swash.styled(&regular, style).unwrap()
    }

    fn ink(swash: &Swash, face: &Face) -> (f64, f64) {
        swash
            .glyph_ink(face, Swash::glyph(face, BOLT).unwrap())
            .unwrap()
    }

    fn coverage(swash: &Swash, face: &Face) -> u32 {
        let mut pixels = vec![0; 4 * 64 * 64];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: 64,
            height: 64,
        };
        let glyph = Glyph {
            id: Swash::glyph(face, BOLT).unwrap(),
            x: 16.0,
            y: 16.0,
        };
        swash.draw_glyphs(&mut canvas, face, [glyph]);
        pixels
            .iter()
            .step_by(4)
            .map(|&alpha| u32::from(alpha))
            .sum()
    }

    #[test]
    fn a_bold_the_family_lacks_is_synthesised_heavier() {
        let swash = Swash::new();
        let (regular, bold) = (
            symbols(&swash, FontStyle::default()),
            symbols(
                &swash,
                FontStyle {
                    bold: true,
                    italic: false,
                },
            ),
        );
        assert!(bold.data.embolden > 0.0);
        assert!(coverage(&swash, &bold) > coverage(&swash, &regular));
    }

    #[test]
    fn an_italic_the_family_lacks_is_synthesised_slanted() {
        let swash = Swash::new();
        let italic = symbols(
            &swash,
            FontStyle {
                bold: false,
                italic: true,
            },
        );
        assert!(italic.data.skew);
        assert!(
            ink(&swash, &italic).1 > ink(&swash, &symbols(&swash, FontStyle::default())).1 + 1.0
        );
    }

    #[test]
    fn a_family_with_the_style_is_not_synthesised() {
        let swash = Swash::new();
        let regular = swash
            .installed_face(crate::DEFAULT_FONT_FAMILY, PIXELS)
            .unwrap();
        let bold = swash
            .styled(
                &regular,
                FontStyle {
                    bold: true,
                    italic: false,
                },
            )
            .unwrap();
        assert_eq!(bold.data.embolden, 0.0);
        assert!(!bold.data.skew);
    }

    #[test]
    fn a_fallback_shapes_with_the_features_of_the_face_that_asked() {
        let swash = Swash::new();
        let plain = swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap();
        let featured = Swash::with_features(
            plain.clone(),
            &[FontFeature {
                tag: *b"ss01",
                value: 1,
            }],
        );
        swash.face_for(&plain, "\u{03A9}").unwrap();
        let fallback = swash.face_for(&featured, "\u{03A9}").unwrap();
        assert!(Rc::ptr_eq(&fallback.features, &featured.features));
    }

    #[test]
    fn neighbouring_clusters_with_the_same_fallback_share_one_face() {
        let swash = Swash::new();
        let face = swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap();
        let (first, second) = (
            swash.face_for(&face, "\u{03A9}").unwrap(),
            swash.face_for(&face, "\u{03C8}").unwrap(),
        );
        assert!(first.same(&second));
    }

    #[test]
    fn dropping_the_backend_frees_its_fallback_fonts() {
        let swash = Swash::new();
        let face = swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap();
        let fallback = Rc::downgrade(&swash.face_for(&face, "\u{03A9}").unwrap().data);
        drop((face, swash));
        assert!(fallback.upgrade().is_none());
    }

    #[test]
    fn drawing_stays_inside_a_canvas_smaller_than_its_buffer() {
        let swash = Swash::new();
        let face = swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap();
        let mut pixels = vec![0; 4 * 64 * 64];
        let mut canvas = swash.canvas(&mut pixels, 64, 8).unwrap();
        let glyph = Glyph {
            id: Swash::glyph(&face, BOLT).unwrap(),
            x: 16.0,
            y: 0.0,
        };
        swash.draw_glyphs(&mut canvas, &face, [glyph]);
        assert!(pixels[4 * 64 * 8..].iter().all(|&byte| byte == 0));
    }

    /// Terminus ships as OTB bitmaps on Linux; tests that need it skip without it.
    #[test]
    fn a_bitmap_only_font_draws_its_glyphs() {
        let swash = Swash::new();
        let Some(face) = swash.installed_face("Terminus", 16.0) else {
            return;
        };
        let glyph = Swash::glyph(&face, 'A').unwrap();
        assert!(swash.glyph_ink(&face, glyph).is_some());
        let mut pixels = vec![0; 4 * 32 * 32];
        let mut canvas = swash.canvas(&mut pixels, 32, 32).unwrap();
        swash.draw_glyphs(
            &mut canvas,
            &face,
            [Glyph {
                id: glyph,
                x: 8.0,
                y: 8.0,
            }],
        );
        assert!(pixels.iter().any(|&byte| byte != 0));
    }

    #[test]
    fn an_emoji_no_font_colours_falls_back_to_a_monochrome_glyph() {
        let swash = Swash::new();
        let face = swash.embedded_face(SYMBOLS_FONT, PIXELS).unwrap();
        let fallback = swash.face_for(&face, "\u{2318}\u{FE0F}").unwrap();
        assert!(Swash::glyph(&fallback, '\u{2318}').is_some());
    }

    #[test]
    fn emoji_presentation_follows_variation_selectors() {
        assert!(is_emoji("\u{1F600}"));
        assert!(is_emoji("\u{2764}\u{FE0F}"));
        assert!(!is_emoji("\u{2764}"));
        assert!(!is_emoji("\u{1F600}\u{FE0E}"));
        assert!(!is_emoji("a"));
    }
}
