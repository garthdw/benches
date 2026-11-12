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

## Rules

- **Colour encoding:** use `site.srgb_blend`, never `format.is_srgb()`, because an isolated layer's `Rgba16Float` is neither. True: output sRGB-encoded values. False: output linear (`rune::color::srgb_decode`). Output is premultiplied; agentd uses `BlendState::PREMULTIPLIED_ALPHA_BLENDING`.
- **Clipping:** Rune sets the scissor to the slot's clip rect before calling you, so overdraw stays inside the slot.
- **Partial opacity** on the element puts the slot in its own layer, hence `format` and `origin` varying. Rotation draws upright inside a turned layer.
- `.effect_bleed(pt)` lets a slot paint past its rect. `.effect_backdrop(true)` requests a backdrop copy.
- `effect()` returns a normal `El`, so handlers, keys, sizing and children chain as usual.
- `draw` receives only `&State`. Cell metrics known only in the compositor must travel by message or interior mutability; agentd sends them to the session actor.

## References

| Task | Reference |
|---|---|
| `Compositor` trait methods, `Frame` fields, `EffectSite` fields | [references/api.md](references/api.md) |
| Native Metal backend limits, wgpu fallback, WGSL, buffers, agentd's pipeline descriptor | [references/metal-backend.md](references/metal-backend.md) |

## File map (under `crates/rune/`)

| Topic | Path |
|---|---|
| Trait, `Frame`, worked example in docs | `src/compose.rs` |
| `EffectSite`, `draw_with`, scissor | `src/render.rs` (scissor near line 1080) |
| `El::effect*` | `src/el/effect.rs` |
| Colour helpers | `rune::color` |
