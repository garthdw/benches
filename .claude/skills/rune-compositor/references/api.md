# Compositor, Frame and EffectSite

## Compositor trait

- `render(&mut self, &mut Frame<S>)`, required. Runs after `Runtime::frame` built the instances.
- `release_gpu(&mut self)`, default no-op. Called right after Rune drops its device and before it makes a replacement: drop everything made on the old device.
- `features() -> wgpu::Features`, optional device features. Default empty; keep it so on macOS (see [metal-backend.md](metal-backend.md)).
- `animating() -> bool`, default false. True keeps the display link running. Leave it false for a terminal: frames come from `poll` and wakes.
- `draws_own() -> bool`, default true. False tells the window the last frame drew nothing beyond `Frame::draw_ui`, so a due timer whose frame would build the same instances draws nothing. Leave it true while the terminal slot is drawn.
- `summary(&mut self, &Device) -> Option<String>`, printed after a headless run.

## Frame

`device, queue, encoder, target, format, size (device px), dt, update_ms, rt: &mut Runtime<S>, ui: &mut Renderer`.

- `size` never exceeds the device's `max_texture_dimension_2d`: a window surface is clamped to it, and a resize past it keeps the previous drawable and viewport.
- `f.device_key()`: the device can change between frames. Rebuild GPU resources when the key changes. Never compare `wgpu::Device`s, whose ids repeat across instances.
- `f.srgb_blend()`.
- `f.instance()`, `f.adapter()`: the wgpu instance and adapter behind the device, `None` for a standalone `Renderer`. `f.restore_device_callbacks()` reinstalls Rune's device-lost and error callbacks after a compositor replaced them.
- `f.fade_stops()`: the frame's packed `ScrollFade::stops`, `[0.0, 0.0]` when no scroller uses them. Only `draw_ui` hands them to the renderer; see the scroll-fade rule in `SKILL.md`.
- `f.write_buffer(buffer, offset, data)`: `queue.write_buffer` via reused staging. Offset and length must be multiples of 4. It needs the `Frame`, so it isn't usable from inside the `draw_with` closure, where Benches uses `queue.write_buffer`.
- `f.draw_ui()`: the default frame with no slots handled. It sets the fade stops and uploads with the crate-private `prepare_ui`, which on an 8-bit sRGB-blended target rounds flat translucent fills and greyscale glyph ink to bytes before blending, as the CPU boot painter does. `f.ui.prepare` (Benches' path) leaves that off, so Rune's own chrome in a compositor frame can differ by 1 LSB at the CPU-to-GPU hand-over; nothing a compositor can switch.

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
| `fade_stops` | The renderer's packed scroll-fade stops, matching `clip_alpha_stops` in `CLIP_WGSL` |
