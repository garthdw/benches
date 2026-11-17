# Backend (macOS) and pipeline requirements

Benches takes Rune's defaults, so `metal-backend` is on and nothing calls `App::gpu_backend` or sets `RUNE_GPU_BACKEND`: it runs on **Rune's native Metal backend**, not wgpu-hal. It plugs in through wgpu's `custom` backend, so `f.device`/`f.queue`/`RenderPass` are ordinary wgpu types and render.rs needs no change. The adapter reports `Backend::Metal`, driver `"rune metal"`. `rune::describe_gpu(f.device)` names the backend and why; `RUNE_GPU_LOG=1` prints it per window.

- **Headless `--snap`** uses `Gpu::headless()`, the same native backend, so snaps exercise the same path as the window.
- **Fallback:** if `features()` asks for anything beyond `TIMESTAMP_QUERY | IMMEDIATES` (the native set) and the backend is only the default, Rune logs once and draws with wgpu instead. An explicit `RUNE_GPU_BACKEND=metal` skips the fallback.
- **Switch to wgpu-hal** to compare or debug: `RUNE_GPU_BACKEND=wgpu cargo run` (env beats `App::gpu_backend`).
- **WGSL only:** naga translates each entry point to MSL per pipeline. Other shader sources panic. WGSL that naga rejects panics at first use with the naga message.
- **Unsupported, panics** naming the way back to wgpu: depth/stencil, multisampling and resolves, more than 4 colour attachments, more than 8 bind groups, binding arrays, render bundles, indirect draws, query sets (except `TIMESTAMP_QUERY` at pass boundaries), `clear_texture`. Benches uses none; the quad pass is plain instanced triangles.
- **Formats/sRGB:** no change to the sRGB rules in `SKILL.md`. The frame target and layer formats come from Rune; Benches builds a pipeline per `site.format` and its atlas is `Rgba8Unorm`.
- **Buffers** are shared-storage and CPU-written in place; `Frame::write_buffer` and `Queue::write_buffer` both work, the former avoids an extra command buffer. `IMMEDIATES` (per-draw bytes up to 4 KB) is available if `features()` requests it.
- **Lost device:** a device can die and be replaced; the `device_key` rebuild rule covers it.

## Pipeline requirements in Benches

Instanced `TriangleStrip`, 4 vertices, one per-instance vertex buffer. The bind group has a uniform, the atlas texture and a sampler. `layout` uses `bind_group_layouts: &[Some(&layout)]`, `immediate_size: 0`, and `RenderPipelineDescriptor` needs `multiview_mask: None`, `cache: None` on this wgpu version.
