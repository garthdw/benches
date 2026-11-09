@group(0) @binding(0) var<uniform> frame: vec4f;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

const MODE_COVERAGE: u32 = 1u;
const MODE_COLOR: u32 = 2u;

struct In { @location(0) rect: vec4f, @location(1) uv: vec4f, @location(2) color: vec3f, @location(3) mode: u32 }
struct Out { @builtin(position) pos: vec4f, @location(0) uv: vec2f, @location(1) color: vec3f, @location(2) @interpolate(flat) mode: u32 }

@vertex fn vs(@builtin(vertex_index) vi: u32, i: In) -> Out {
    let corner = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let p = (i.rect.xy + corner * i.rect.zw - frame.xy) / frame.zw;
    var out: Out;
    out.pos = vec4f(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    out.uv = i.uv.xy + corner * i.uv.zw;
    out.color = i.color;
    out.mode = i.mode;
    return out;
}

fn srgb_decode(c: vec3f) -> vec3f {
    return mix(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, vec3f(c <= vec3f(0.04045)));
}

@fragment fn fs(v: Out) -> @location(0) vec4f {
    let texel = textureSample(atlas, atlas_sampler, v.uv);
    if v.mode == MODE_COVERAGE {
        return vec4f(v.color * texel.a, texel.a);
    }
    if v.mode == MODE_COLOR {
        return texel;
    }
    if v.mode > MODE_COLOR {
        let straight = texel.rgb / max(texel.a, 0.0001);
        return vec4f(srgb_decode(straight) * texel.a, texel.a);
    }
    return vec4f(v.color, 1.0);
}
