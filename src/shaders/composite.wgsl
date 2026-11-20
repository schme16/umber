// Layer stack compositor. One `blend` dispatch per visible layer ping-pongs between two
// accumulators; `resolve` then writes the maps the viewport samples and derives the normal
// map from height.
//
// Colour is kept sRGB-encoded throughout, so blend modes behave the way painters expect
// (multiply, screen and overlay all work on the encoded values, as in Photoshop or Krita).
// The material accumulator holds roughness, metalness, height (0.5 = flat) and AO.

struct LayerParams {
    fill_color: vec4<f32>, // sRGB rgb, unused
    fill_mat: vec4<f32>,   // roughness, metalness, height, unused
    channels: vec4<f32>,   // fill layers: colour, rough, metal, height switched on (0 or 1)
    mode: u32,             // BlendMode
    kind: u32,             // 0 clear, 1 paint, 2 fill
    opacity: f32,
    mask_invert: u32,
};

@group(0) @binding(0) var<uniform> p: LayerParams;
@group(0) @binding(1) var src_color: texture_2d<f32>;
@group(0) @binding(2) var src_mat: texture_2d<f32>;
@group(0) @binding(3) var dst_color: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var dst_mat: texture_storage_2d<rgba16float, write>;
@group(0) @binding(5) var layer_color: texture_2d<f32>;
@group(0) @binding(6) var layer_mat: texture_2d<f32>;
@group(0) @binding(7) var layer_cov: texture_2d<f32>;
@group(0) @binding(8) var mask: texture_2d<f32>;

fn blend(mode: u32, b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    switch mode {
        case 1u: { return b * s; }
        case 2u: { return b + s - b * s; }
        case 3u: {
            return select(vec3<f32>(1.0) - 2.0 * (vec3<f32>(1.0) - b) * (vec3<f32>(1.0) - s), 2.0 * b * s, b < vec3<f32>(0.5));
        }
        case 4u: { return min(b + s, vec3<f32>(1.0)); }
        case 5u: { return max(b - s, vec3<f32>(0.0)); }
        case 6u: { return min(b, s); }
        case 7u: { return max(b, s); }
        case 8u: {
            // W3C soft light.
            let d = select(sqrt(b), ((16.0 * b - 12.0) * b + 4.0) * b, b <= vec3<f32>(0.25));
            return select(b + (2.0 * s - 1.0) * (d - b), b - (1.0 - 2.0 * s) * b * (1.0 - b), s <= vec3<f32>(0.5));
        }
        default: { return s; }
    }
}

@compute @workgroup_size(8, 8)
fn blend_layer(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(dst_color);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let c = vec2<i32>(gid.xy);
    if (p.kind == 0u) {
        textureStore(dst_color, c, vec4<f32>(0.5, 0.5, 0.5, 1.0));
        textureStore(dst_mat, c, vec4<f32>(0.5, 0.0, 0.5, 1.0));
        return;
    }
    let bc = textureLoad(src_color, c, 0);
    let bm = textureLoad(src_mat, c, 0);

    var sc: vec3<f32>;
    var sm: vec3<f32>;
    var cov: vec4<f32>;
    if (p.kind == 1u) {
        let lc = textureLoad(layer_color, c, 0);
        sc = lc.rgb;
        sm = textureLoad(layer_mat, c, 0).xyz;
        cov = vec4<f32>(lc.a, textureLoad(layer_cov, c, 0).xyz);
    } else {
        sc = p.fill_color.rgb;
        sm = p.fill_mat.xyz;
        cov = p.channels;
    }

    // Masks may be smaller than the texture set (a 1x1 white texture stands in for "no mask").
    let md = textureDimensions(mask);
    var m = textureLoad(mask, vec2<i32>(gid.xy * md / dims), 0).r;
    if (p.mask_invert != 0u) {
        m = 1.0 - m;
    }
    let a = clamp(cov * p.opacity * m, vec4<f32>(0.0), vec4<f32>(1.0));

    let rgb = mix(bc.rgb, blend(p.mode, bc.rgb, sc), a.x);
    let mat = mix(bm.xyz, blend(p.mode, bm.xyz, sm), a.yzw);
    textureStore(dst_color, c, vec4<f32>(rgb, 1.0));
    textureStore(dst_mat, c, vec4<f32>(mat, bm.w));
}

struct ResolveParams {
    // How steep a height change reads in the normal map, per texel.
    normal_strength: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(9) var<uniform> r: ResolveParams;
@group(0) @binding(10) var out_color: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(11) var out_mat: texture_storage_2d<rgba16float, write>;
@group(0) @binding(12) var out_normal: texture_storage_2d<rgba8unorm, write>;

fn height_at(c: vec2<i32>, dims: vec2<i32>) -> f32 {
    return textureLoad(src_mat, clamp(c, vec2<i32>(0), dims - vec2<i32>(1)), 0).z;
}

@compute @workgroup_size(8, 8)
fn resolve(@builtin(global_invocation_id) gid: vec3<u32>) {
    let udims = textureDimensions(out_color);
    if (gid.x >= udims.x || gid.y >= udims.y) {
        return;
    }
    let dims = vec2<i32>(udims);
    let c = vec2<i32>(gid.xy);
    let col = textureLoad(src_color, c, 0);
    let mat = textureLoad(src_mat, c, 0);
    textureStore(out_color, c, vec4<f32>(clamp(col.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
    textureStore(out_mat, c, clamp(mat, vec4<f32>(0.0), vec4<f32>(1.0)));

    // Tangent-space normal, OpenGL convention (+Y points to the top of the image, as Godot
    // and Blender expect). Image rows run down, so the Y gradient flips sign.
    let dx = (height_at(c + vec2<i32>(1, 0), dims) - height_at(c - vec2<i32>(1, 0), dims)) * 0.5;
    let dy = (height_at(c + vec2<i32>(0, 1), dims) - height_at(c - vec2<i32>(0, 1), dims)) * 0.5;
    let n = normalize(vec3<f32>(-dx * r.normal_strength, dy * r.normal_strength, 1.0));
    textureStore(out_normal, c, vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0));
}
