---
name: rune-compositor
description: Rune's custom GPU drawing contract as used by Benches - Compositor, Frame, effect slots, EffectSite, sRGB handling, device changes. Use when changing render.rs or cell.wgsl, or adding GPU-drawn surfaces, instead of reading the Rune source.
---

# Rune compositor and effect slots

Verified against Rune rev `4918969` (wgpu `~30.0.1`; Benches locks wgpu 30.0.2 over Rune's patched wgpu-hal and naga 30.0.1). If the pin has moved, check anything you rely on.

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

## Rules

- **Colour encoding:** use `site.srgb_blend`, never `format.is_srgb()`, because an isolated layer's `Rgba16Float` is neither. True: output sRGB-encoded values. False: output linear (`rune::color::srgb_decode`). Output is premultiplied; Benches uses `BlendState::PREMULTIPLIED_ALPHA_BLENDING`.
- **Clipping:** Rune sets the scissor to the slot's clip rect before calling you, so overdraw stays inside the slot. A slot clipped away entirely gets no call; viewport and scissor are reset to the full target after it.
- **Partial opacity** on the element puts the slot in its own layer, hence `format` and `origin` varying. Rotation draws upright inside a turned layer.
- `.effect_bleed(pt)` lets a slot paint past its rect. `.effect_backdrop(true)` requests a backdrop copy.
- `effect()` returns a normal `El`, so handlers, keys, sizing and children chain as usual.
- **Device changes and loss:** everything made on `f.device`, and any CPU state that indexes it (atlas placements, job ids), belongs in the struct rebuilt when `f.device_key()` changes. A lost device reaches the compositor as `release_gpu()`, called right after Rune drops the device, then a new key on a later frame. Benches keeps the default no-op `release_gpu` and rebuilds on the key; dropping early only matters on Windows D3D12, where a removed device's resources block making a new one; see [references/metal-backend.md](references/metal-backend.md).
- **Scroll-fade stops:** `f.ui.prepare` leaves the renderer's stops as they were; `Frame::draw_ui` sets them. A compositor frame with a scroller using `ScrollFade::stops` calls `f.ui.set_fade_stops(f.rt.fade_stops())` before `prepare`. Benches has none.
- `draw` receives only `&State`. Cell metrics known only in the compositor must travel by message or interior mutability; Benches sends them to the session actor.

## References

| Task | Reference |
|---|---|
| `Compositor` trait methods, `Frame` fields, `EffectSite` fields | [references/api.md](references/api.md) |
| Native Metal backend limits, wgpu fallback, WGSL, buffers, Benches' pipeline descriptor | [references/metal-backend.md](references/metal-backend.md) |

## File map (under `crates/`)

| Topic | Path |
|---|---|
| Trait, `Frame`, worked example in docs | `rune/src/compose.rs` |
| `EffectSite`, `EffectHook`, `draw_with`, scissor | `rune/src/render.rs` (`draw_with_effects` at line 584, scissor set at 607) |
| `Renderer::prepare`, `prepare_ui`, `prepare_lists` | `rune/src/render/upload.rs` |
| `Instance`, `Kind`, `KIND_*` | `rune/src/render/instance.rs` |
| `El::effect*` | `rune/src/el/effect.rs` |
| Colour helpers | `rune::color`, re-exporting `rune-core/src/color.rs` |
| Native Metal backend, backend choice and OS floor, `describe_gpu` | `rune-gpu/src/metal/`, `rune-gpu/src/backend.rs` |
| Metal command-buffer failure, lost callback | `rune-gpu/src/metal/submit.rs` (`failed`, `pop_completed`, `dispatch`), `rune-gpu/src/metal/device.rs` |
| `Gpu::is_lost`, window device replacement | `rune/src/render/gpu_loss.rs`, `rune/src/window/host/create.rs` (`reattach`, `replace_lost_surface`) |
