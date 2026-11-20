// Moving paint to a new UV layout. Each triangle is drawn where its new UVs put it, and every
// texel it covers takes the colour its old UVs pointed at: a bilinear blend of the four
// nearest old texels, each looked up through the old layout's padding map. A texel on the
// edge of a new island can point just past the edge of the old one, where the brush never
// painted; the map sends each of those lookups to the nearest texel that was, so nothing
// from outside an island bleeds in.

struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) old_uv: vec2<f32>,
};

@vertex
fn vs(@location(0) uv: vec2<f32>, @location(1) old_uv: vec2<f32>) -> Out {
    var o: Out;
    o.clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.old_uv = old_uv;
    return o;
}

const NO_SEED: u32 = 0xffffffffu;

@group(0) @binding(0) var src: texture_2d<f32>;
// For every texel of the old layout, the nearest one inside an island (x | y << 16).
@group(0) @binding(1) var padding: texture_2d<u32>;

// The old texel at `p` (wrapped), or the nearest one inside an island.
fn fetch(p_in: vec2<i32>, size: vec2<i32>) -> vec4<f32> {
    var p = ((p_in % size) + size) % size;
    if all(vec2<i32>(textureDimensions(padding)) == size) {
        let seed = textureLoad(padding, p, 0).r;
        if seed != NO_SEED {
            p = vec2<i32>(i32(seed & 0xffffu), i32(seed >> 16u));
        }
    }
    return textureLoad(src, p, 0);
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(src));
    // Wrapped, as the viewport samples it.
    let t = fract(i.old_uv) * vec2<f32>(size) - 0.5;
    let base = vec2<i32>(floor(t));
    let f = t - floor(t);
    let top = mix(fetch(base, size), fetch(base + vec2<i32>(1, 0), size), f.x);
    let bottom = mix(fetch(base + vec2<i32>(0, 1), size), fetch(base + vec2<i32>(1, 1), size), f.x);
    return mix(top, bottom, f.y);
}
