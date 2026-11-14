@group(0) @binding(0) var<uniform> frame: vec4f;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var coverage_atlas: texture_2d<f32>;

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
    let coverage = textureSample(coverage_atlas, atlas_sampler, v.uv).r;
    if v.mode == MODE_COVERAGE {
        return vec4f(v.color * coverage, coverage);
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

@group(1) @binding(0) var image: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

const MODE_IMAGE_LINEAR: u32 = 5u;

// Straight-alpha sRGB texels, premultiplied here; a linear target needs them decoded first.
@fragment fn fs_image(v: Out) -> @location(0) vec4f {
    let texel = textureSample(image, image_sampler, v.uv);
    var rgb = texel.rgb;
    if v.mode == MODE_IMAGE_LINEAR {
        rgb = srgb_decode(rgb);
    }
    return vec4f(rgb * texel.a, texel.a);
}
