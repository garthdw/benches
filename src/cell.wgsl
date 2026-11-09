@group(0) @binding(0) var<uniform> frame: vec4f;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct In { @location(0) rect: vec4f, @location(1) uv: vec4f, @location(2) color: vec3f }
struct Out { @builtin(position) pos: vec4f, @location(0) uv: vec2f, @location(1) color: vec3f, @location(2) solid: f32 }

@vertex fn vs(@builtin(vertex_index) vi: u32, i: In) -> Out {
    let corner = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let p = (i.rect.xy + corner * i.rect.zw - frame.xy) / frame.zw;
    var out: Out;
    out.pos = vec4f(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    out.uv = i.uv.xy + corner * i.uv.zw;
    out.color = i.color;
    out.solid = select(0.0, 1.0, i.uv.z == 0.0);
    return out;
}

@fragment fn fs(v: Out) -> @location(0) vec4f {
    let coverage = textureSample(atlas, atlas_sampler, v.uv).r;
    let a = select(coverage, 1.0, v.solid > 0.5);
    return vec4f(v.color * a, a);
}
