# Compositor, Frame and EffectSite

## Compositor trait

- `render(&mut self, &mut Frame<S>)`, required. Runs after `Runtime::frame` built the instances.
- `features() -> wgpu::Features`, optional device features. Default empty; keep it so on macOS (see [metal-backend.md](metal-backend.md)).
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
