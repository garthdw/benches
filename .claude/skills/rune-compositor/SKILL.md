---
name: rune-compositor
description: Rune's custom GPU drawing contract as used by agentd - Compositor, Frame, effect slots, EffectSite, sRGB handling, device changes. Use when changing render.rs or cell.wgsl, or adding GPU-drawn surfaces, instead of reading the Rune source.
---

# Rune compositor and effect slots

Verified against Rune rev `ba5e7c8` (wgpu `~30.0.1`). If the pin has moved, check anything you rely on.

## Pattern

An element reserves space with `effect(MARK)`; a `Compositor` paints there, inside Rune's own render pass.

```rust
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
```

Register with `App::new(spec).compositor(..)`. It runs in the window and in headless `--snap` alike.

## Compositor trait

- `render(&mut self, &mut Frame<S>)`, required. Runs after `Runtime::frame` built the instances.
- `features() -> wgpu::Features`, optional device features. Default empty; keep it so on macOS (see Backend).
- `animating() -> bool`, default false. True keeps the display link running. Leave it false for a terminal: frames come from `poll` and wakes.
- `summary(&mut self, &Device) -> Option<String>`, printed after a headless run.

## Frame

`device, queue, encoder, target, format, size (device px), dt, update_ms, rt: &mut Runtime<S>, ui: &mut Renderer`.

- `f.device_key()`: the device can change between frames. Rebuild GPU resources when the key changes. Never compare `wgpu::Device`s, whose ids repeat across instances.
- `f.srgb_blend()`.
- `f.write_buffer(buffer, offset, data)`: `queue.write_buffer` via reused staging. Offset and length must be multiples of 4. It needs the `Frame`, so it isn't usable from inside the `draw_with` closure, where agentd uses `queue.write_buffer`.
- `f.draw_ui()`: the default frame with no slots handled.

## EffectSite (argument to the draw closure)

| Field | Meaning |
|---|---|
| `mark` | The `effect(mark)` tag; key per-slot resources on it |
| `instance` | The record; `instance.rect` is `[x, y, w, h]` in device px, window space |
| `data` | `[f32; 4]` from `El::effect_data` |
| `scale` | Device px per point at this slot, including element scales |
| `format` | Format of this pass; build a pipeline per format |
| `origin` | Top-left of this pass in window px. Subtract from window coordinates. Non-zero in isolated layers |
| `size` | Pass target size in device px |
| `in_layer` | Drawing into an isolated layer |
| `srgb_blend` | How to encode colours |
| `index` | Position in the painter stream |
| `backdrop` | `Option<&TextureView>`, a copy of the target so far; only when the slot set `effect_backdrop(true)` |

## Rules

- **Colour encoding:** use `site.srgb_blend`, never `format.is_srgb()`, because an isolated layer's `Rgba16Float` is neither. True: output sRGB-encoded values. False: output linear (`rune::color::srgb_decode`). Output is premultiplied; agentd uses `BlendState::PREMULTIPLIED_ALPHA_BLENDING`.
- **Clipping:** Rune sets the scissor to the slot's clip rect before calling you, so overdraw stays inside the slot.
- **Partial opacity** on the element puts the slot in its own layer, hence `format` and `origin` varying. Rotation draws upright inside a turned layer.
- `.effect_bleed(pt)` lets a slot paint past its rect. `.effect_backdrop(true)` requests a backdrop copy.
- `effect()` returns a normal `El`, so handlers, keys, sizing and children chain as usual.
- `draw` receives only `&State`. Cell metrics known only in the compositor must travel by message or interior mutability; agentd sends them to the session actor.

## Backend (macOS)

agentd takes Rune's defaults, so `metal-backend` is on and nothing calls `App::gpu_backend` or sets `RUNE_GPU_BACKEND`: it runs on **Rune's native Metal backend**, not wgpu-hal. It plugs in through wgpu's `custom` backend, so `f.device`/`f.queue`/`RenderPass` are ordinary wgpu types and render.rs needs no change. The adapter reports `Backend::Metal`, driver `"rune metal"`. `rune::describe_gpu(f.device)` names the backend and why; `RUNE_GPU_LOG=1` prints it per window.

- **Headless `--snap`** uses `Gpu::headless()`, the same native backend, so snaps exercise the same path as the window.
- **Fallback:** if `features()` asks for anything beyond `TIMESTAMP_QUERY | IMMEDIATES` (the native set) and the backend is only the default, Rune logs once and draws with wgpu instead. An explicit `RUNE_GPU_BACKEND=metal` skips the fallback.
- **Switch to wgpu-hal** to compare or debug: `RUNE_GPU_BACKEND=wgpu cargo run` (env beats `App::gpu_backend`).
- **WGSL only:** naga translates each entry point to MSL per pipeline. Other shader sources panic. WGSL that naga rejects panics at first use with the naga message.
- **Unsupported, panics** naming the way back to wgpu: depth/stencil, multisampling and resolves, more than 4 colour attachments, more than 8 bind groups, binding arrays, render bundles, indirect draws, query sets (except `TIMESTAMP_QUERY` at pass boundaries), `clear_texture`. agentd uses none; the quad pass is plain instanced triangles.
- **Formats/sRGB:** no change to the sRGB rules below. The frame target and layer formats come from Rune; agentd builds a pipeline per `site.format` and its atlas is `Rgba8Unorm`.
- **Buffers** are shared-storage and CPU-written in place; `Frame::write_buffer` and `Queue::write_buffer` both work, the former avoids an extra command buffer. `IMMEDIATES` (per-draw bytes up to 4 KB) is available if `features()` requests it.
- **Lost device:** a device can die and be replaced; the `device_key` rebuild rule covers it.

## Pipeline requirements in agentd

Instanced `TriangleStrip`, 4 vertices, one per-instance vertex buffer. The bind group has a uniform, the atlas texture and a sampler. `layout` uses `bind_group_layouts: &[Some(&layout)]`, `immediate_size: 0`, and `RenderPipelineDescriptor` needs `multiview_mask: None`, `cache: None` on this wgpu version.

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Trait, `Frame`, worked example in docs | `src/compose.rs` |
| `EffectSite`, `draw_with`, scissor | `src/render.rs` (scissor near line 1080) |
| `El::effect*` | `src/el/effect.rs` |
| Colour helpers | `rune::color` |
