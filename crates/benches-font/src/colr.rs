//! Ported from Rune's `rune-text` (MIT, © Anomaly): `crates/rune-text/src/raster/colr.rs`.
//!
//! COLR v1 colour glyphs (gradients, transforms, layers), such as the COLR v1 Noto Color Emoji
//! many Linux distributions ship. swash draws only COLR v0's flat layers, and nothing for a
//! v1-only font. This walks skrifa's paint graph into premultiplied RGBA, as Skia does for
//! Chromium: each clip a coverage mask from zeno, each fill a solid colour or gradient evaluated
//! per pixel, layers composited on pop.

use skrifa::color::{
    Brush, ColorGlyph, ColorGlyphCollection, ColorGlyphFormat, ColorPainter, ColorStop,
    CompositeMode, Extend, Transform,
};
use skrifa::instance::{LocationRef, NormalizedCoord, Size};
use skrifa::metrics::BoundingBox;
use skrifa::outline::{DrawSettings, OutlineGlyphCollection, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::raw::tables::cpal::ColorRecord;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use swash::zeno::{Command, Mask, Scratch, Vector};

/// A font's COLR v1 glyphs at one variation instance.
pub(crate) struct ColrV1<'a> {
    font: FontRef<'a>,
    glyphs: ColorGlyphCollection<'a>,
    coords: Vec<NormalizedCoord>,
}

/// Premultiplied RGBA whose top-left corner is `left`, `top` pixels from the origin, y up.
pub(crate) struct Image {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Wider or taller than this, a glyph is a broken font's, not one worth an allocation.
const MAX_SIZE: u32 = 1024;

impl<'a> ColrV1<'a> {
    /// The font's COLR v1 glyphs, or `None` when it has none.
    pub(crate) fn new(data: &'a [u8], index: u32, coords: &[i16]) -> Option<Self> {
        let font = FontRef::from_index(data, index).ok()?;
        font.colr().ok()?.base_glyph_list()?.ok()?;
        Some(Self {
            glyphs: font.color_glyphs(),
            font,
            coords: coords
                .iter()
                .map(|&coord| NormalizedCoord::from_bits(coord))
                .collect(),
        })
    }

    fn glyph(&self, glyph: u16) -> Option<ColorGlyph<'a>> {
        self.glyphs
            .get_with_format(GlyphId::new(u32::from(glyph)), ColorGlyphFormat::ColrV1)
    }

    pub(crate) fn has(&self, glyph: u16) -> bool {
        self.glyph(glyph).is_some()
    }

    /// The glyph's clip box at `pixels`, up from its origin; without one, the em square across
    /// its advance.
    pub(crate) fn bounds(&self, glyph: u16, pixels: f32) -> Option<BoundingBox> {
        self.bounds_of(&self.glyph(glyph)?, glyph, pixels)
    }

    fn bounds_of(&self, color: &ColorGlyph<'a>, glyph: u16, pixels: f32) -> Option<BoundingBox> {
        let location = LocationRef::new(&self.coords);
        let size = Size::new(pixels);
        color.bounding_box(location, size).or_else(|| {
            let advance = self
                .font
                .glyph_metrics(size, location)
                .advance_width(GlyphId::new(u32::from(glyph)))?;
            Some(BoundingBox {
                x_min: 0.0,
                y_min: -0.25 * pixels,
                x_max: advance,
                y_max: pixels,
            })
        })
    }

    /// The glyph at `pixels`, stretched by `scale`, with its origin at `origin` (pixels, y up):
    /// `None` when the font has no paint graph for it.
    pub(crate) fn rasterize(
        &self,
        glyph: u16,
        pixels: f32,
        scale: (f32, f32),
        origin: (f32, f32),
    ) -> Option<Image> {
        let color = self.glyph(glyph)?;
        let bounds = self.bounds_of(&color, glyph, pixels)?;
        let left = (bounds.x_min * scale.0 + origin.0).floor() as i32;
        let top = (bounds.y_max * scale.1 + origin.1).ceil() as i32;
        let right = (bounds.x_max * scale.0 + origin.0).ceil() as i32;
        let bottom = (bounds.y_min * scale.1 + origin.1).floor() as i32;
        let width = u32::try_from(right - left).ok()?;
        let height = u32::try_from(top - bottom).ok()?;
        if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
            return None;
        }
        let units = pixels / f32::from(self.font.head().ok()?.units_per_em());
        // Font units, y up, to image pixels, y down.
        let base = Transform {
            xx: units * scale.0,
            yx: 0.0,
            xy: 0.0,
            yy: -units * scale.1,
            dx: origin.0 - left as f32,
            dy: top as f32 - origin.1,
        };
        let cpal = self.font.cpal().ok();
        let area = width as usize * height as usize;
        let location = LocationRef::new(&self.coords);
        let mut painter = Painter {
            palette: Palette {
                records: cpal
                    .as_ref()
                    .and_then(|cpal| cpal.color_records_array())
                    .and_then(Result::ok)
                    .unwrap_or(&[]),
                first: cpal
                    .as_ref()
                    .and_then(|cpal| cpal.color_record_indices().first())
                    .map_or(0, |first| usize::from(first.get())),
                entries: cpal.as_ref().map_or(0, |cpal| cpal.num_palette_entries()),
            },
            width: width as usize,
            paths: Paths {
                location,
                outlines: self.font.outline_glyphs(),
                transforms: vec![base],
            },
            clips: vec![Clip {
                cover: vec![1.0; area],
                bounds: [0, 0, width as usize, height as usize],
            }],
            layers: vec![vec![[0.0; 4]; area]],
            scratch: Scratch::new(),
        };
        color.paint(location, &mut painter).ok()?;
        let rgba = painter
            .layers
            .pop()?
            .iter()
            .flat_map(|pixel| pixel.map(|channel| (channel.clamp(0.0, 1.0) * 255.0 + 0.5) as u8))
            .collect();
        Some(Image {
            left,
            top,
            width,
            height,
            rgba,
        })
    }
}

/// Palette 0, read where a paint asks for a colour: an emoji font's palette has thousands of
/// entries and a glyph uses a handful.
struct Palette<'a> {
    records: &'a [ColorRecord],
    first: usize,
    entries: u16,
}

impl Palette<'_> {
    /// Straight RGBA in 0..1. 0xFFFF is the text's colour; an emoji's image takes none, so it's
    /// black.
    fn color(&self, index: u16, alpha: f32) -> [f32; 4] {
        let record = (index < self.entries)
            .then(|| self.records.get(self.first + usize::from(index)))
            .flatten();
        let [red, green, blue, opacity] = record.map_or([0.0, 0.0, 0.0, 1.0], |record| {
            [record.red, record.green, record.blue, record.alpha]
                .map(|channel| f32::from(channel) / 255.0)
        });
        [red, green, blue, opacity * alpha]
    }

    /// The colour line at `t`, premultiplied, interpolated premultiplied as Skia does so a stop
    /// going transparent doesn't darken.
    fn stop(&self, stops: &[ColorStop], t: f32) -> [f32; 4] {
        let at = |stop: &ColorStop| premultiply(self.color(stop.palette_index, stop.alpha));
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return [0.0; 4];
        };
        if t <= first.offset {
            return at(first);
        }
        for pair in stops.windows(2) {
            let [before, after] = pair else { continue };
            if t <= after.offset {
                let fraction = (t - before.offset) / (after.offset - before.offset).max(1e-6);
                let (from, to) = (at(before), at(after));
                return std::array::from_fn(|channel| {
                    from[channel] + (to[channel] - from[channel]) * fraction
                });
            }
        }
        at(last)
    }
}

struct Painter<'a> {
    palette: Palette<'a>,
    width: usize,
    paths: Paths<'a>,
    /// Each the intersection of the clips under it.
    clips: Vec<Clip>,
    /// Premultiplied RGBA layers.
    layers: Vec<Vec<[f32; 4]>>,
    /// zeno's buffers, kept across a glyph's masks.
    scratch: Scratch,
}

/// Coverage 0..1 over the box (x0, y0, x1, y1) where it may be non-zero, row by row: a fill
/// visits that box alone, and a small clip costs its own size, not the glyph's.
struct Clip {
    cover: Vec<f32>,
    bounds: [usize; 4],
}

fn invert(transform: &Transform) -> Option<Transform> {
    let determinant = transform.xx * transform.yy - transform.xy * transform.yx;
    if determinant.abs() < 1e-12 {
        return None;
    }
    let (xx, xy, yx, yy) = (
        transform.yy / determinant,
        -transform.xy / determinant,
        -transform.yx / determinant,
        transform.xx / determinant,
    );
    Some(Transform {
        xx,
        xy,
        yx,
        yy,
        dx: -(xx * transform.dx + xy * transform.dy),
        dy: -(yx * transform.dx + yy * transform.dy),
    })
}

/// Glyph outlines and boxes through a stack of transforms from font units.
struct Paths<'a> {
    location: LocationRef<'a>,
    outlines: OutlineGlyphCollection<'a>,
    transforms: Vec<Transform>,
}

impl Paths<'_> {
    fn transform(&self) -> Transform {
        self.transforms.last().copied().unwrap_or_default()
    }

    fn push(&mut self, transform: Transform) {
        let transform = self.transform() * transform;
        self.transforms.push(transform);
    }

    fn pop(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn pen(&self) -> Pen {
        Pen {
            transform: self.transform(),
            path: Vec::new(),
        }
    }

    /// Glyph `glyph`'s outline through the current transform.
    fn glyph(&self, glyph: GlyphId) -> Vec<Command> {
        let mut pen = self.pen();
        if let Some(outline) = self.outlines.get(glyph) {
            let settings = DrawSettings::unhinted(Size::unscaled(), self.location);
            // A broken outline draws what it reached.
            if let Err(error) = outline.draw(settings, &mut pen) {
                eprintln!("benches: COLR v1 glyph {glyph}'s outline is broken: {error}");
            }
        }
        pen.path
    }

    /// `rect` through the current transform.
    fn rect(&self, rect: BoundingBox) -> Vec<Command> {
        let mut pen = self.pen();
        pen.move_to(rect.x_min, rect.y_min);
        pen.line_to(rect.x_max, rect.y_min);
        pen.line_to(rect.x_max, rect.y_max);
        pen.line_to(rect.x_min, rect.y_max);
        pen.close();
        pen.path
    }
}

/// An outline as path commands, through a transform.
struct Pen {
    transform: Transform,
    path: Vec<Command>,
}

impl Pen {
    fn point(&self, x: f32, y: f32) -> Vector {
        let (x, y) = self.transform.transform(x, y);
        Vector::new(x, y)
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.push(Command::MoveTo(point));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.push(Command::LineTo(point));
    }

    fn quad_to(&mut self, control_x: f32, control_y: f32, x: f32, y: f32) {
        let (control, point) = (self.point(control_x, control_y), self.point(x, y));
        self.path.push(Command::QuadTo(control, point));
    }

    fn curve_to(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, x: f32, y: f32) {
        let (first, second, point) = (self.point(x0, y0), self.point(x1, y1), self.point(x, y));
        self.path.push(Command::CurveTo(first, second, point));
    }

    fn close(&mut self) {
        self.path.push(Command::Close);
    }
}

impl Painter<'_> {
    /// `path`'s coverage inside the clip on top: only the box the path and that clip share is
    /// rasterised and kept.
    fn mask(&mut self, path: &[Command]) -> Clip {
        let empty = Clip {
            cover: Vec::new(),
            bounds: [0; 4],
        };
        let Some(under) = self.clips.last() else {
            return empty;
        };
        let (mut low, mut high) = ([f32::MAX; 2], [f32::MIN; 2]);
        let points = path
            .iter()
            .flat_map(|command| match *command {
                Command::MoveTo(a) | Command::LineTo(a) => [Some(a), None, None],
                Command::QuadTo(a, b) => [Some(a), Some(b), None],
                Command::CurveTo(a, b, c) => [Some(a), Some(b), Some(c)],
                Command::Close => [None, None, None],
            })
            .flatten();
        for point in points {
            low = [low[0].min(point.x), low[1].min(point.y)];
            high = [high[0].max(point.x), high[1].max(point.y)];
        }
        let [under_x0, under_y0, under_x1, under_y1] = under.bounds;
        let x0 = (low[0].floor().max(0.0) as usize).max(under_x0);
        let y0 = (low[1].floor().max(0.0) as usize).max(under_y0);
        let x1 = (high[0].ceil().max(0.0) as usize).min(under_x1);
        let y1 = (high[1].ceil().max(0.0) as usize).min(under_y1);
        if x1 <= x0 || y1 <= y0 {
            return empty;
        }
        let (width, height) = (x1 - x0, y1 - y0);
        let mut mask = vec![0u8; width * height];
        Mask::with_scratch(path, &mut self.scratch)
            .size(width as u32, height as u32)
            .offset(Vector::new(-(x0 as f32), -(y0 as f32)))
            .render_into(&mut mask, None);
        let coverage = |alpha: u8| f32::from(alpha) / 255.0;
        let under_width = under_x1 - under_x0;
        let cover = mask
            .chunks_exact(width)
            .zip((y0 - under_y0)..)
            .flat_map(|(row, under_row)| {
                let start = under_row * under_width + (x0 - under_x0);
                row.iter()
                    .zip(&under.cover[start..start + width])
                    .map(|(&alpha, &under)| coverage(alpha) * under)
            })
            .collect();
        Clip {
            cover,
            bounds: [x0, y0, x1, y1],
        }
    }
}

/// The rows of `layer` (`width` wide) that `clip` covers, each with its y, its x0, the layer's
/// pixels and the clip's coverage.
fn clipped_rows<'l>(
    layer: &'l mut [[f32; 4]],
    width: usize,
    clip: &'l Clip,
) -> impl Iterator<Item = (usize, &'l mut [[f32; 4]], &'l [f32])> {
    let [x0, y0, x1, y1] = clip.bounds;
    let clip_width = x1 - x0;
    layer
        .chunks_exact_mut(width)
        .skip(y0)
        .take(y1.saturating_sub(y0))
        .zip(clip.cover.chunks_exact(clip_width.max(1)))
        .zip(y0..)
        .map(move |((row, cover), y)| (y, &mut row[x0..x1], cover))
}

/// Paints `colour` at each pixel centre (x, y) under `clip` over `layer`, source over: one loop
/// per brush kind, the colour inlined.
fn shade(
    layer: &mut [[f32; 4]],
    width: usize,
    clip: &Clip,
    mut colour: impl FnMut(f32, f32) -> [f32; 4],
) {
    let x0 = clip.bounds[0];
    for (y, row, cover) in clipped_rows(layer, width, clip) {
        for ((pixel, &coverage), x) in row.iter_mut().zip(cover).zip(x0..) {
            if coverage > 0.0 {
                blend(pixel, colour(x as f32 + 0.5, y as f32 + 0.5), coverage);
            }
        }
    }
}

/// A gradient's colour line, sampled once per fill: finer than one step a pixel at emoji sizes
/// (20-80 px across), and a quarter of 256's setup.
const LINE_STEPS: usize = 64;

/// A gradient's fill: `line` writes each row's colour-line parameters (NaN where the gradient
/// draws nothing) for pixel centres x = x0 + 0.5.. at y, a whole row in one pass the compiler can
/// vectorise; then each is looked up in `lookup` and painted, source over.
fn shade_line(
    layer: &mut [[f32; 4]],
    width: usize,
    clip: &Clip,
    lookup: &[[f32; 4]; LINE_STEPS],
    extend: Extend,
    mut line: impl FnMut(f32, f32, &mut [f32]),
) {
    let x0 = clip.bounds[0];
    let mut parameters = vec![0.0f32; clip.bounds[2] - x0];
    for (y, row, cover) in clipped_rows(layer, width, clip) {
        line(x0 as f32 + 0.5, y as f32 + 0.5, &mut parameters);
        for ((pixel, &coverage), &t) in row.iter_mut().zip(cover).zip(&parameters) {
            if coverage <= 0.0 || t.is_nan() {
                continue;
            }
            let step = (extend_t(extend, t) * (LINE_STEPS - 1) as f32 + 0.5) as usize;
            blend(pixel, lookup[step.min(LINE_STEPS - 1)], coverage);
        }
    }
}

fn blend(pixel: &mut [f32; 4], colour: [f32; 4], coverage: f32) {
    let keep = 1.0 - colour[3] * coverage;
    *pixel = std::array::from_fn(|channel| colour[channel] * coverage + pixel[channel] * keep);
}

/// Fills `clip` on `layer` with `brush`, whose points are in the space `inverse` maps pixels to.
fn fill_brush(
    palette: &Palette<'_>,
    layer: &mut [[f32; 4]],
    width: usize,
    clip: &Clip,
    inverse: &Transform,
    brush: &Brush<'_>,
) {
    let line = |stops: &[ColorStop]| {
        std::array::from_fn::<[f32; 4], LINE_STEPS, _>(|step| {
            palette.stop(stops, step as f32 / (LINE_STEPS - 1) as f32)
        })
    };
    match *brush {
        Brush::Solid {
            palette_index,
            alpha,
        } => {
            let colour = premultiply(palette.color(palette_index, alpha));
            shade(layer, width, clip, |_, _| colour);
        }
        Brush::LinearGradient {
            p0,
            p1,
            color_stops,
            extend,
        } => {
            // t is affine in the pixel's position: t = a x + b y + c.
            let (dx, dy) = (p1.x - p0.x, p1.y - p0.y);
            let length = (dx * dx + dy * dy).max(1e-12);
            let (a, b) = (
                (inverse.xx * dx + inverse.yx * dy) / length,
                (inverse.xy * dx + inverse.yy * dy) / length,
            );
            let c = ((inverse.dx - p0.x) * dx + (inverse.dy - p0.y) * dy) / length;
            shade_line(
                layer,
                width,
                clip,
                &line(color_stops),
                extend,
                |x0, y, parameters| {
                    for (t, x) in parameters.iter_mut().zip(0..) {
                        *t = a * (x0 + x as f32) + b * y + c;
                    }
                },
            );
        }
        Brush::RadialGradient {
            c0,
            r0,
            c1,
            r1,
            color_stops,
            extend,
        } => {
            // The two-point conical gradient's parameter at each pixel: the largest t with
            // |p - c(t)| = r(t) and r(t) >= 0, its constants hoisted and its divisions made
            // products.
            let (centre_x_delta, centre_y_delta, radius_delta) =
                (c1.x - c0.x, c1.y - c0.y, r1 - r0);
            let a = centre_x_delta * centre_x_delta + centre_y_delta * centre_y_delta
                - radius_delta * radius_delta;
            // A pixel centre in the gradient's space, from its first circle's centre.
            let relative = |x: f32, y: f32| {
                let (x, y) = inverse.transform(x, y);
                (x - c0.x, y - c0.y)
            };
            let (start_radius_by_delta, start_radius_squared) = (r0 * radius_delta, r0 * r0);
            let lookup = line(color_stops);
            if a.abs() < 1e-6 {
                shade_line(layer, width, clip, &lookup, extend, |x0, y, parameters| {
                    for (t, x) in parameters.iter_mut().zip(0..) {
                        let (point_x, point_y) = relative(x0 + x as f32, y);
                        let b = point_x * centre_x_delta
                            + point_y * centre_y_delta
                            + start_radius_by_delta;
                        let v = (point_x * point_x + point_y * point_y - start_radius_squared)
                            / (2.0 * b);
                        *t = if b.abs() < 1e-6 || r0 + v * radius_delta < 0.0 {
                            f32::NAN
                        } else {
                            v
                        };
                    }
                });
                return;
            }
            let inverse_a = 1.0 / a;
            shade_line(layer, width, clip, &lookup, extend, |x0, y, parameters| {
                for (t, x) in parameters.iter_mut().zip(0..) {
                    let (point_x, point_y) = relative(x0 + x as f32, y);
                    let b =
                        point_x * centre_x_delta + point_y * centre_y_delta + start_radius_by_delta;
                    let discriminant =
                        b * b - a * (point_x * point_x + point_y * point_y - start_radius_squared);
                    let root = discriminant.max(0.0).sqrt();
                    let (first_root, second_root) =
                        ((b + root) * inverse_a, (b - root) * inverse_a);
                    let (high, low) = (first_root.max(second_root), first_root.min(second_root));
                    *t = if discriminant < 0.0 {
                        f32::NAN
                    } else if r0 + high * radius_delta >= 0.0 {
                        high
                    } else if r0 + low * radius_delta >= 0.0 {
                        low
                    } else {
                        f32::NAN
                    };
                }
            });
        }
        Brush::SweepGradient {
            c0,
            start_angle,
            end_angle,
            color_stops,
            extend,
        } => {
            let span = end_angle - start_angle;
            // A sweep with no span draws its first stop.
            let per_degree = if span.abs() > 1e-6 { 1.0 / span } else { 0.0 };
            shade_line(
                layer,
                width,
                clip,
                &line(color_stops),
                extend,
                |x0, y, parameters| {
                    for (t, x) in parameters.iter_mut().zip(0..) {
                        let (point_x, point_y) = inverse.transform(x0 + x as f32, y);
                        // Clockwise degrees in font space (y up), as skrifa hands them over.
                        let angle = (-(point_y - c0.y).atan2(point_x - c0.x).to_degrees())
                            .rem_euclid(360.0);
                        *t = (angle - start_angle) * per_degree;
                    }
                },
            );
        }
    }
}

/// `t` on the colour line's 0..1 as `extend` carries it past the ends.
fn extend_t(extend: Extend, t: f32) -> f32 {
    match extend {
        Extend::Repeat => t - t.floor(),
        Extend::Reflect => {
            let reflected = t.rem_euclid(2.0);
            if reflected > 1.0 {
                2.0 - reflected
            } else {
                reflected
            }
        }
        _ => t.clamp(0.0, 1.0),
    }
}

fn premultiply([red, green, blue, alpha]: [f32; 4]) -> [f32; 4] {
    [red * alpha, green * alpha, blue * alpha, alpha]
}

fn composite(mode: CompositeMode, source: [f32; 4], destination: [f32; 4]) -> [f32; 4] {
    let (source_alpha, destination_alpha) = (source[3], destination[3]);
    match mode {
        CompositeMode::Clear => [0.0; 4],
        CompositeMode::Src => source,
        CompositeMode::Dest => destination,
        CompositeMode::DestOver => std::array::from_fn(|channel| {
            destination[channel] + source[channel] * (1.0 - destination_alpha)
        }),
        CompositeMode::SrcIn => source.map(|channel| channel * destination_alpha),
        CompositeMode::DestIn => destination.map(|channel| channel * source_alpha),
        CompositeMode::SrcOut => source.map(|channel| channel * (1.0 - destination_alpha)),
        CompositeMode::DestOut => destination.map(|channel| channel * (1.0 - source_alpha)),
        CompositeMode::SrcAtop => std::array::from_fn(|channel| {
            source[channel] * destination_alpha + destination[channel] * (1.0 - source_alpha)
        }),
        CompositeMode::DestAtop => std::array::from_fn(|channel| {
            destination[channel] * source_alpha + source[channel] * (1.0 - destination_alpha)
        }),
        CompositeMode::Xor => std::array::from_fn(|channel| {
            source[channel] * (1.0 - destination_alpha)
                + destination[channel] * (1.0 - source_alpha)
        }),
        CompositeMode::Plus => {
            std::array::from_fn(|channel| (source[channel] + destination[channel]).min(1.0))
        }
        CompositeMode::Multiply => std::array::from_fn(|channel| {
            source[channel] * destination[channel]
                + source[channel] * (1.0 - destination_alpha)
                + destination[channel] * (1.0 - source_alpha)
        }),
        CompositeMode::Screen => std::array::from_fn(|channel| {
            source[channel] + destination[channel] - source[channel] * destination[channel]
        }),
        // The rest of the separable and non-separable blend modes are rare in emoji fonts: drawn
        // over, as a renderer that lacks one does.
        _ => std::array::from_fn(|channel| {
            source[channel] + destination[channel] * (1.0 - source_alpha)
        }),
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, transform: Transform) {
        self.paths.push(transform);
    }

    fn pop_transform(&mut self) {
        self.paths.pop();
    }

    fn push_clip_glyph(&mut self, glyph: GlyphId) {
        let path = self.paths.glyph(glyph);
        let clip = self.mask(&path);
        self.clips.push(clip);
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox) {
        let path = self.paths.rect(clip_box);
        let clip = self.mask(&path);
        self.clips.push(clip);
    }

    fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    fn fill(&mut self, brush: Brush<'_>) {
        let Some(inverse) = invert(&self.paths.transform()) else {
            return;
        };
        let (Some(clip), Some(layer)) = (self.clips.last(), self.layers.last_mut()) else {
            return;
        };
        fill_brush(&self.palette, layer, self.width, clip, &inverse, &brush);
    }

    /// Most of an emoji's paints: a glyph's shape filled, its mask used once and not kept as a
    /// clip.
    fn fill_glyph(&mut self, glyph: GlyphId, brush_transform: Option<Transform>, brush: Brush<'_>) {
        let path = self.paths.glyph(glyph);
        let clip = self.mask(&path);
        let transform = self.paths.transform();
        let transform =
            brush_transform.map_or(transform, |brush_transform| transform * brush_transform);
        let Some(inverse) = invert(&transform) else {
            return;
        };
        let Some(layer) = self.layers.last_mut() else {
            return;
        };
        fill_brush(&self.palette, layer, self.width, &clip, &inverse, &brush);
    }

    fn push_layer(&mut self, _mode: CompositeMode) {
        let area = self.layers.first().map_or(0, Vec::len);
        self.layers.push(vec![[0.0; 4]; area]);
    }

    fn pop_layer_with_mode(&mut self, mode: CompositeMode) {
        let [.., under, top] = self.layers.as_mut_slice() else {
            return;
        };
        for (destination, &source) in under.iter_mut().zip(top.iter()) {
            *destination = composite(mode, source, *destination);
        }
        self.layers.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_past_palette_0_takes_the_fallback_colour() {
        let record = |red| ColorRecord {
            blue: 0,
            green: 0,
            red,
            alpha: 255,
        };
        let records = [record(255), record(128)];
        let palette = Palette {
            records: &records,
            first: 0,
            entries: 1,
        };
        assert_eq!(palette.color(0, 1.0), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(palette.color(1, 1.0), [0.0, 0.0, 0.0, 1.0]);
    }
}
