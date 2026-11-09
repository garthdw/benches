use std::collections::HashMap;
use std::mem::size_of;

use bytemuck::{Pod, Zeroable, cast_slice};
use rune::color::srgb_decode;
use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::EffectSite;

use crate::State;
use crate::font::Font;
use crate::session::Grid;

const FONT_POINTS: f64 = 13.0;
const PADDING_POINTS: f32 = 6.0;
const ATLAS_SIZE: u32 = 2048;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Quad {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 3],
}

fn color(rgb: [u8; 3], srgb_blend: bool) -> [f32; 3] {
    rgb.map(|c| {
        let encoded = f32::from(c) / 255.0;
        if srgb_blend { encoded } else { srgb_decode(encoded) }
    })
}

struct Atlas {
    slots: HashMap<(char, bool), Option<[u32; 2]>>,
    next: [u32; 2],
    epoch: u64,
    scratch: Vec<u8>,
}

impl Atlas {
    fn new() -> Self {
        Self { slots: HashMap::new(), next: [1, 1], epoch: 0, scratch: Vec::new() }
    }

    fn reset(&mut self) {
        self.slots.clear();
        self.next = [1, 1];
        self.epoch += 1;
    }

    fn glyph(&mut self, queue: &wgpu::Queue, texture: &wgpu::Texture, font: &Font, ch: char, bold: bool) -> Option<[u32; 2]> {
        if let Some(slot) = self.slots.get(&(ch, bold)) {
            return *slot;
        }
        let (w, h) = (font.cell_w, font.cell_h);
        if self.next[0] + w + 1 > ATLAS_SIZE {
            self.next = [1, self.next[1] + h + 1];
        }
        if self.next[1] + h + 1 > ATLAS_SIZE {
            self.reset();
        }
        let slot = font.rasterize(ch, bold, &mut self.scratch).then_some(self.next);
        if let Some([x, y]) = slot {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &self.scratch,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            self.next[0] += w + 1;
        }
        self.slots.insert((ch, bold), slot);
        slot
    }
}

fn build_quads(
    grid: &Grid,
    rect: [f32; 4],
    padding: f32,
    font: &Font,
    srgb_blend: bool,
    quads: &mut Vec<Quad>,
    mut glyph: impl FnMut(char, bool) -> Option<[u32; 2]>,
) {
    let solid = |rect, c| Quad { rect, uv: [0.0; 4], color: color(c, srgb_blend) };
    let (w, h) = (font.cell_w as f32, font.cell_h as f32);
    let (origin_x, origin_y) = ((rect[0] + padding).round(), (rect[1] + padding).round());
    let cols = usize::from(grid.cols);
    let cell_rect = |i: usize| [origin_x + (i % cols) as f32 * w, origin_y + (i / cols) as f32 * h, w, h];

    quads.clear();
    quads.push(solid(rect, grid.bg));
    for (i, cell) in grid.cells.iter().enumerate() {
        if let Some(cell_bg) = cell.bg {
            quads.push(solid(cell_rect(i), cell_bg));
        }
    }
    let cursor = grid.cursor.map(|(x, y)| usize::from(y) * cols + usize::from(x));
    if let Some(i) = cursor {
        quads.push(solid(cell_rect(i), grid.cursor_color));
    }
    let (atlas_w, atlas_h) = (w / ATLAS_SIZE as f32, h / ATLAS_SIZE as f32);
    for (i, cell) in grid.cells.iter().enumerate() {
        if cell.ch == ' ' {
            continue;
        }
        if let Some([x, y]) = glyph(cell.ch, cell.bold) {
            let uv = [x as f32 / ATLAS_SIZE as f32, y as f32 / ATLAS_SIZE as f32, atlas_w, atlas_h];
            let fg = if cursor == Some(i) { grid.bg } else { cell.fg };
            quads.push(Quad { rect: cell_rect(i), uv, color: color(fg, srgb_blend) });
        }
    }
}

struct Slot {
    quads: wgpu::Buffer,
    scratch: Vec<Quad>,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    built: Option<(u64, [f32; 4], u64, bool)>,
    sent_size: Option<(u16, u16, u32, u32)>,
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
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
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
                entry(0, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }),
                entry(1, wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }),
                entry(2, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
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
        if self.pipelines.iter().any(|(f, _)| *f == format) {
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
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x3],
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

fn new_slot(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, view: &wgpu::TextureView, sampler: &wgpu::Sampler) -> Slot {
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
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    });
    Slot {
        quads: quad_buffer(device, 4096 * size_of::<Quad>() as u64),
        scratch: Vec::new(),
        uniforms,
        bind_group,
        built: None,
        sent_size: None,
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
    fn draw(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, state: &State, pass: &mut wgpu::RenderPass<'_>, site: &EffectSite<'_>) {
        let gpu = self.gpu.as_mut().expect("gpu is built before drawing");
        if self.font.as_ref().is_some_and(|(scale, _)| *scale != site.scale) {
            self.font = None;
        }
        let (_, font) = self.font.get_or_insert_with(|| {
            gpu.atlas.reset();
            (site.scale, Font::new(FONT_POINTS, site.scale))
        });
        gpu.ensure_pipeline(device, site.format);
        let slot = gpu.slots.entry(site.mark).or_insert_with(|| new_slot(device, &gpu.layout, &gpu.view, &gpu.sampler));

        let rect = site.instance.rect;
        let padding = PADDING_POINTS * site.scale;
        let cols = (((rect[2] - 2.0 * padding) / font.cell_w as f32) as u16).max(1);
        let rows = (((rect[3] - 2.0 * padding) / font.cell_h as f32) as u16).max(1);
        let size = (cols, rows, font.cell_w, font.cell_h);
        if slot.sent_size.replace(size) != Some(size)
            && let Some(session) = &state.session
        {
            session.resize(cols, rows, font.cell_w as u16, font.cell_h as u16);
        }

        let grid = state.grid();
        let key = (grid.generation, rect, gpu.atlas.epoch, site.srgb_blend);
        if slot.built.replace(key) != Some(key) {
            let (atlas, texture) = (&mut gpu.atlas, &gpu.texture);
            build_quads(&grid, rect, padding, font, site.srgb_blend, &mut slot.scratch, |ch, bold| atlas.glyph(queue, texture, font, ch, bold));
            let bytes: &[u8] = cast_slice(&slot.scratch);
            if bytes.len() as u64 > slot.quads.size() {
                slot.quads = quad_buffer(device, (bytes.len() as u64).next_power_of_two());
            }
            queue.write_buffer(&slot.quads, 0, bytes);
        }

        let frame = [site.origin[0], site.origin[1], site.size[0] as f32, site.size[1] as f32];
        queue.write_buffer(&slot.uniforms, 0, cast_slice(&frame));
        let pipeline = &gpu.pipelines.iter().find(|(f, _)| *f == site.format).expect("pipeline was just built").1;
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &slot.bind_group, &[]);
        pass.set_vertex_buffer(0, slot.quads.slice(..));
        pass.draw(0..4, 0..slot.scratch.len() as u32);
    }
}

impl Compositor<State> for TerminalCompositor {
    fn render(&mut self, f: &mut Frame<'_, State>) {
        let key = f.device_key();
        if self.gpu.as_ref().is_none_or(|gpu| gpu.key != key) {
            self.gpu = Some(Gpu::new(f.device, key));
        }
        let (device, queue, size, clear) = (f.device, f.queue, f.size, f.rt.clear_color());
        f.ui.prepare(device, queue, &mut f.rt.text.atlas, &f.rt.instances, size);
        let state = &f.rt.state;
        f.ui.draw_with(f.encoder, f.target, clear, &mut |pass, site| self.draw(device, queue, state, pass, site));
    }
}
