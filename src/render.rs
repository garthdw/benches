use std::collections::HashMap;
use std::mem::size_of;

use bytemuck::{Pod, Zeroable, cast_slice};
use libghostty_vt::render::CursorVisualStyle;
use libghostty_vt::style::Underline;
use rune::color::srgb_decode;
use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::EffectSite;

use crate::State;
use crate::font::{Font, FontStyle, GlyphKind};
use crate::session::{Cell, Grid, Metrics, Session, Surface};

const FONT_POINTS: f64 = 18.0;
const PADDING_POINTS: f32 = 6.0;
const ATLAS_SIZE: u32 = 2048;
const MODE_SOLID: u32 = 0;
const MODE_COVERAGE: u32 = 1;
const MODE_COLOR: u32 = 2;
/// A colour glyph whose sRGB texels must be decoded for a linear target.
const MODE_COLOR_LINEAR: u32 = 3;

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

#[derive(Clone, PartialEq, Eq, Hash)]
enum GlyphText {
    Character(char),
    Cluster(String),
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

struct Atlas {
    slots: HashMap<GlyphKey, Option<AtlasGlyph>>,
    next: [u32; 2],
    epoch: u64,
    scratch: Vec<u8>,
}

impl Atlas {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            next: [1, 1],
            epoch: 0,
            scratch: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.slots.clear();
        self.next = [1, 1];
        self.epoch += 1;
    }

    fn glyph(
        &mut self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        font: &Font,
        key: GlyphKey,
    ) -> Option<AtlasGlyph> {
        if let Some(slot) = self.slots.get(&key) {
            return *slot;
        }
        let (width, height) = (font.cell_width * key.cells, font.cell_height);
        if self.next[0] + width + 1 > ATLAS_SIZE {
            self.next = [1, self.next[1] + height + 1];
        }
        if self.next[1] + height + 1 > ATLAS_SIZE {
            self.reset();
        }
        let mut utf8 = [0; 4];
        let kind = match &key.text {
            GlyphText::Character(character) => font.rasterize(
                character.encode_utf8(&mut utf8),
                key.style,
                key.cells,
                &mut self.scratch,
            ),
            GlyphText::Cluster(cluster) => {
                font.rasterize(cluster, key.style, key.cells, &mut self.scratch)
            }
            GlyphText::Underline(underline) => {
                font.rasterize_underline(*underline, &mut self.scratch);
                Some(GlyphKind::Coverage)
            }
        };
        let slot = kind.map(|kind| AtlasGlyph {
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
                &self.scratch,
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
        self.slots.insert(key, slot);
        slot
    }
}

fn glyph_key(grid: &Grid, cell: &Cell) -> Option<GlyphKey> {
    let text = match grid.cluster(cell) {
        Some(cluster) => GlyphText::Cluster(cluster.to_owned()),
        None if cell.spacer || cell.character == ' ' => return None,
        None => GlyphText::Character(cell.character),
    };
    Some(GlyphKey {
        text,
        style: FontStyle {
            bold: cell.bold,
            italic: cell.italic,
        },
        cells: if cell.wide { 2 } else { 1 },
    })
}

fn build_quads(
    grid: &Grid,
    rect: [f32; 4],
    padding: f32,
    font: &Font,
    srgb_blend: bool,
    quads: &mut Vec<Quad>,
    mut glyph: impl FnMut(GlyphKey) -> (Option<AtlasGlyph>, u64),
) {
    let solid = |rect, fill| Quad {
        rect,
        uv: [0.0; 4],
        color: color(fill, srgb_blend),
        mode: MODE_SOLID,
    };
    let (width, height) = (font.cell_width as f32, font.cell_height as f32);
    let (origin_x, origin_y) = ((rect[0] + padding).round(), (rect[1] + padding).round());
    let cols = usize::from(grid.cols);
    let cell_rect = |index: usize| {
        [
            origin_x + (index % cols) as f32 * width,
            origin_y + (index / cols) as f32 * height,
            width,
            height,
        ]
    };

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
    let inverted = cursor.filter(|_| grid.cursor_style == CursorVisualStyle::Block);
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
            let Some(key) = glyph_key(grid, cell) else {
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
}

struct Slot {
    quads: wgpu::Buffer,
    scratch: Vec<Quad>,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Session id, generation, rect, atlas epoch and blend mode; generations restart per session.
    built: Option<(u64, u64, [f32; 4], u64, bool)>,
}

struct Gpu {
    key: DeviceKey,
    layout: wgpu::BindGroupLayout,
    module: wgpu::ShaderModule,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    atlas: Atlas,
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
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
            atlas: Atlas::new(),
            pipelines: Vec::new(),
            slots: HashMap::new(),
        }
    }

    fn ensure_pipeline(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) {
        if self.pipelines.iter().any(|(built, _)| *built == format) {
            return;
        }
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&self.layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.push((format, pipeline));
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

#[derive(Default)]
pub struct TerminalCompositor {
    gpu: Option<Gpu>,
    font: Option<(f32, Font)>,
}

impl TerminalCompositor {
    fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        state: &State,
        pass: &mut wgpu::RenderPass<'_>,
        site: &EffectSite<'_>,
    ) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        if self
            .font
            .as_ref()
            .is_some_and(|(scale, _)| *scale != site.scale)
        {
            self.font = None;
        }
        if self.font.is_none() {
            match Font::new(FONT_POINTS, site.scale) {
                Ok(font) => {
                    gpu.atlas.reset();
                    self.font = Some((site.scale, font));
                }
                Err(error) => {
                    eprintln!("agentd: load font: {error:#}");
                    return;
                }
            }
        }
        let Some((_, font)) = &self.font else {
            return;
        };
        gpu.ensure_pipeline(device, site.format);
        let slot = gpu
            .slots
            .entry(site.mark)
            .or_insert_with(|| new_slot(device, &gpu.layout, &gpu.view, &gpu.sampler));

        let rect = site.instance.rect;
        let padding = PADDING_POINTS * site.scale;
        let cols = (((rect[2] - 2.0 * padding) / font.cell_width as f32) as u16).max(1);
        let rows = (((rect[3] - 2.0 * padding) / font.cell_height as f32) as u16).max(1);
        let surface = Surface {
            cols,
            rows,
            metrics: Metrics {
                cell_width: font.cell_width as u16,
                cell_height: font.cell_height as u16,
                padding,
                scale: site.scale,
            },
        };
        // Hidden tabs share the visible area, so they are sized too.
        for session in state.workspace.sessions() {
            session.resize(surface);
        }

        let grid = state.grid();
        let built = (
            state.session().map_or(0, Session::id),
            grid.generation,
            rect,
            gpu.atlas.epoch,
            site.srgb_blend,
        );
        if slot.built != Some(built) {
            let (atlas, texture) = (&mut gpu.atlas, &gpu.texture);
            build_quads(
                &grid,
                rect,
                padding,
                font,
                site.srgb_blend,
                &mut slot.scratch,
                |key| {
                    let slot = atlas.glyph(queue, texture, font, key);
                    (slot, atlas.epoch)
                },
            );
            let bytes: &[u8] = cast_slice(&slot.scratch);
            if bytes.len() as u64 > slot.quads.size() {
                slot.quads = quad_buffer(device, (bytes.len() as u64).next_power_of_two());
            }
            queue.write_buffer(&slot.quads, 0, bytes);
            slot.built = Some(built);
        }

        let frame = [
            site.origin[0],
            site.origin[1],
            site.size[0] as f32,
            site.size[1] as f32,
        ];
        queue.write_buffer(&slot.uniforms, 0, cast_slice(&frame));
        let Some((_, pipeline)) = gpu
            .pipelines
            .iter()
            .find(|(built, _)| *built == site.format)
        else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &slot.bind_group, &[]);
        pass.set_vertex_buffer(0, slot.quads.slice(..));
        pass.draw(0..4, 0..slot.scratch.len() as u32);
    }
}

impl Compositor<State> for TerminalCompositor {
    fn render(&mut self, frame: &mut Frame<'_, State>) {
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
        let state = &frame.rt.state;
        frame
            .ui
            .draw_with(frame.encoder, frame.target, clear, &mut |pass, site| {
                self.draw(device, queue, state, pass, site)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn fake_glyph(position: [u32; 2]) -> AtlasGlyph {
        AtlasGlyph {
            position,
            kind: GlyphKind::Coverage,
        }
    }

    fn quads_for(grid: &Grid, scale: f32) -> (Vec<Quad>, Font) {
        let font = Font::new(FONT_POINTS, scale).unwrap();
        let mut quads = Vec::new();
        build_quads(
            grid,
            [0.0, 0.0, 800.0, 600.0],
            0.0,
            &font,
            true,
            &mut quads,
            |_| (Some(fake_glyph([0, 0])), 0),
        );
        (quads, font)
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
    fn a_reset_during_a_build_leaves_no_quad_on_a_wiped_slot() {
        let grid = Grid::message("abc");
        let font = Font::new(FONT_POINTS, 1.0).unwrap();
        let mut slots: HashMap<GlyphKey, [u32; 2]> = HashMap::new();
        let mut epoch = 0;
        let mut quads = Vec::new();
        build_quads(
            &grid,
            [0.0, 0.0, 800.0, 600.0],
            0.0,
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
