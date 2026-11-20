// Projection painting. The mesh is drawn in UV space, one fragment per texel of the paint
// layer. Each fragment projects its surface point through the camera and asks whether any dab
// (a screen-space ellipse) covers it, and whether the camera can actually see that point.
// Strokes therefore cross UV seams and stretched UVs without breaking up, the way they look on
// screen.
//
// A stroke builds up in `stroke` (one float per texel): each dab pulls the texel towards its
// strength at the brush's flow rate, never past it, so overlapping dabs can't exceed the
// stroke's opacity. The layer is then written as the copy taken when the stroke started
// (`base_*`) with the stroke laid over it, so repainting a texel never compounds.

struct Stroke {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // Viewport size in pixels, dab count, mode.
    viewport: vec4<f32>,
    // Paint values: sRGB colour, then roughness, metalness, height.
    colour: vec4<f32>,
    material: vec4<f32>,
    // Channels the brush paints: colour, rough, metal, height (0 or 1).
    channels: vec4<f32>,
    // Hardness, fill opacity, unused, unused.
    shape: vec4<f32>,
    // Brush texture: projection (0 none, 1 surface, 2 UV, 3 view), repeats per unit, then the
    // cosine and sine of its rotation.
    tex: vec4<f32>,
};

struct Dab {
    // Centre in viewport pixels, then the ellipse's long axis (unit vector).
    center_axis: vec4<f32>,
    // Radius in pixels, squash across the axis (1 = round), strength, flow.
    params: vec4<f32>,
};

const MODE_PAINT: u32 = 0u;
const MODE_ERASE: u32 = 1u;
const MODE_FILL: u32 = 2u;
const TILE: u32 = 256u;

@group(0) @binding(0) var<uniform> s: Stroke;
@group(0) @binding(1) var<storage, read> dabs: array<Dab>;
@group(0) @binding(2) var dist: texture_2d<f32>;
@group(0) @binding(3) var stroke: texture_storage_2d<r32float, read_write>;
@group(0) @binding(4) var base_color: texture_2d<f32>;
@group(0) @binding(5) var base_mat: texture_2d<f32>;
@group(0) @binding(6) var base_cov: texture_2d<f32>;
@group(0) @binding(7) var<storage, read_write> tiles: array<atomic<u32>>;
@group(0) @binding(8) var brush_colour: texture_2d<f32>;
@group(0) @binding(9) var brush_height: texture_2d<f32>;
@group(0) @binding(10) var brush_sampler: sampler;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct UvOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

@vertex
fn vs_uv(v: VsIn) -> UvOut {
    var o: UvOut;
    o.clip = vec4<f32>(v.uv.x * 2.0 - 1.0, 1.0 - v.uv.y * 2.0, 0.0, 1.0);
    o.world = v.pos;
    o.normal = v.normal;
    o.uv = v.uv;
    return o;
}

// ---------------------------------------------------------------- brush texture

/// What the brush lays down at a texel: colour with alpha (how much of it lands), and height.
struct Ink {
    colour: vec4<f32>,
    height: f32,
};

fn turn(c: vec2<f32>) -> vec2<f32> {
    let cs = s.tex.zw;
    return vec2<f32>(c.x * cs.x - c.y * cs.y, c.x * cs.y + c.y * cs.x);
}

/// One look-up, with the footprint given so the right mip level is used.
fn ink_at(c: vec2<f32>, dx: vec2<f32>, dy: vec2<f32>) -> Ink {
    var k: Ink;
    k.colour = textureSampleGrad(brush_colour, brush_sampler, turn(c), turn(dx), turn(dy));
    k.height = textureSampleGrad(brush_height, brush_sampler, turn(c), turn(dx), turn(dy)).r;
    return k;
}

/// The brush's colour and height at this texel: flat paint, or a look-up in its texture.
/// Must run before anything branches per texel, since it takes screen derivatives.
fn ink(i: UvOut) -> Ink {
    let mode = u32(s.tex.x);
    let rep = s.tex.y;
    // The same three projections are worked out whichever is used: derivatives need every
    // texel to take the same path.
    let p = i.world * rep;
    let dpx = dpdx(p);
    let dpy = dpdy(p);
    let u = i.uv * rep;
    let dux = dpdx(u);
    let duy = dpdy(u);
    let cp = s.view_proj * vec4<f32>(i.world, 1.0);
    let ndc = cp.xy / select(cp.w, 1.0, abs(cp.w) < 1e-6);
    let v = vec2<f32>((ndc.x * 0.5 + 0.5) * s.viewport.x, (0.5 - ndc.y * 0.5) * s.viewport.y) * rep;
    let dvx = dpdx(v);
    let dvy = dpdy(v);

    var k: Ink;
    k.colour = vec4<f32>(s.colour.rgb, 1.0);
    k.height = 1.0;
    if (mode == 1u) {
        // Three projections blended by which way the surface faces. Up stays up on walls.
        var w = pow(abs(normalize(i.normal)), vec3<f32>(4.0));
        w = w / max(w.x + w.y + w.z, 1e-5);
        let a = ink_at(vec2<f32>(p.z, -p.y), vec2<f32>(dpx.z, -dpx.y), vec2<f32>(dpy.z, -dpy.y));
        let b = ink_at(p.xz, dpx.xz, dpy.xz);
        let c = ink_at(vec2<f32>(p.x, -p.y), vec2<f32>(dpx.x, -dpx.y), vec2<f32>(dpy.x, -dpy.y));
        k.colour = a.colour * w.x + b.colour * w.y + c.colour * w.z;
        k.height = a.height * w.x + b.height * w.y + c.height * w.z;
    } else if (mode == 2u) {
        k = ink_at(u, dux, duy);
    } else if (mode == 3u) {
        k = ink_at(v, dvx, dvy);
    }
    return k;
}

/// The height the brush paints: the slider alone, or with a texture, the texture's bumps
/// scaled by how far the slider is from flat (0.5).
fn ink_height(k: Ink) -> f32 {
    if (u32(s.tex.x) == 0u) {
        return s.material.z;
    }
    return clamp(0.5 + (k.height - 0.5) * (s.material.z - 0.5) * 2.0, 0.0, 1.0);
}

/// Can the camera see `world`? Compares its distance from the eye with the nearest surface
/// in the distance buffer at the four pixels around where it lands.
fn visible(world: vec3<f32>, px: vec2<f32>) -> bool {
    let size = vec2<i32>(textureDimensions(dist));
    let d = distance(world, s.eye.xyz);
    let tol = d * 0.004 + 1e-5;
    let p0 = vec2<i32>(floor(px - vec2<f32>(0.5)));
    for (var k = 0; k < 4; k++) {
        let p = clamp(p0 + vec2<i32>(k & 1, k >> 1), vec2<i32>(0), size - vec2<i32>(1));
        if (d <= textureLoad(dist, p, 0).r + tol) {
            return true;
        }
    }
    return false;
}

/// How much of the brush reaches this texel this frame, as an updated stroke value.
/// Returns -1 when no dab touches it.
fn stroke_value(i: UvOut, texel: vec2<i32>) -> f32 {
    let mode = u32(s.viewport.w);
    var m = textureLoad(stroke, texel).r;
    if (mode == MODE_FILL) {
        let t = s.shape.y;
        return select(-1.0, t, t > m);
    }

    let cp = s.view_proj * vec4<f32>(i.world, 1.0);
    if (cp.w <= 0.0) {
        return -1.0;
    }
    let ndc = cp.xy / cp.w;
    let px = vec2<f32>((ndc.x * 0.5 + 0.5) * s.viewport.x, (0.5 - ndc.y * 0.5) * s.viewport.y);
    if (px.x < -1.0 || px.y < -1.0 || px.x > s.viewport.x + 1.0 || px.y > s.viewport.y + 1.0) {
        return -1.0;
    }

    // Fade out where the surface turns away, so paint doesn't smear along grazing faces.
    let n = normalize(i.normal);
    let facing = smoothstep(0.03, 0.18, abs(dot(n, normalize(s.eye.xyz - i.world))));
    if (facing <= 0.0) {
        return -1.0;
    }

    let hard = clamp(s.shape.x, 0.0, 0.98);
    var touched = false;
    var checked = false;
    let count = u32(s.viewport.z);
    for (var k = 0u; k < count; k++) {
        let dab = dabs[k];
        let rel = px - dab.center_axis.xy;
        let axis = dab.center_axis.zw;
        let along = dot(rel, axis) / max(dab.params.y, 0.05);
        let across = dot(rel, vec2<f32>(-axis.y, axis.x));
        let r = length(vec2<f32>(along, across)) / max(dab.params.x, 0.5);
        if (r >= 1.0) {
            continue;
        }
        if (!checked) {
            // Only the first dab that reaches the texel pays for the visibility test.
            if (!visible(i.world, px)) {
                return -1.0;
            }
            checked = true;
        }
        let falloff = 1.0 - smoothstep(hard, 1.0, r);
        let t = dab.params.z * falloff * facing;
        if (t > m) {
            m = m + (t - m) * dab.params.w;
            touched = true;
        }
    }
    return select(-1.0, m, touched);
}

fn mark_tile(texel: vec2<i32>) {
    let n = textureDimensions(stroke).x / TILE;
    let t = vec2<u32>(texel) / TILE;
    atomicStore(&tiles[t.y * n + t.x], 1u);
}

struct LayerOut {
    @location(0) color: vec4<f32>,
    @location(1) mat: vec4<f32>,
    @location(2) cov: vec4<f32>,
};

/// Paint layers: colour with its coverage in alpha, material values, material coverage.
@fragment
fn fs_layer(i: UvOut) -> LayerOut {
    let k = ink(i);
    let texel = vec2<i32>(floor(i.clip.xy));
    let m = stroke_value(i, texel);
    if (m < 0.0) {
        discard;
    }
    textureStore(stroke, texel, vec4<f32>(m, 0.0, 0.0, 0.0));
    mark_tile(texel);

    let bc = textureLoad(base_color, texel, 0);
    let bm = textureLoad(base_mat, texel, 0);
    let bv = textureLoad(base_cov, texel, 0);
    var o: LayerOut;
    if (u32(s.viewport.w) == MODE_ERASE) {
        let a = clamp(m * s.channels, vec4<f32>(0.0), vec4<f32>(1.0));
        o.color = vec4<f32>(bc.rgb, bc.a * (1.0 - a.x));
        o.mat = bm;
        o.cov = vec4<f32>(bv.xyz * (vec3<f32>(1.0) - a.yzw), 0.0);
        return o;
    }
    // Where the texture is see-through, the brush lays nothing down.
    let a = clamp(m * k.colour.a * s.channels, vec4<f32>(0.0), vec4<f32>(1.0));
    let material = vec3<f32>(s.material.xy, ink_height(k));
    // Lay the paint over what was there: coverage adds, values mix by coverage.
    let ca = a.x + bc.a * (1.0 - a.x);
    let rgb = (k.colour.rgb * a.x + bc.rgb * bc.a * (1.0 - a.x)) / max(ca, 1e-5);
    o.color = vec4<f32>(select(bc.rgb, rgb, ca > 0.0), ca);
    let cv = a.yzw + bv.xyz * (vec3<f32>(1.0) - a.yzw);
    let mv = (material * a.yzw + bm.xyz * bv.xyz * (vec3<f32>(1.0) - a.yzw)) / max(cv, vec3<f32>(1e-5));
    o.mat = vec4<f32>(select(bm.xyz, mv, cv > vec3<f32>(0.0)), bm.w);
    o.cov = vec4<f32>(cv, 0.0);
    return o;
}

/// Layer masks: greyscale in red, painted towards the brush's grey value.
@fragment
fn fs_mask(i: UvOut) -> @location(0) vec4<f32> {
    let k = ink(i);
    let texel = vec2<i32>(floor(i.clip.xy));
    let m = stroke_value(i, texel);
    if (m < 0.0) {
        discard;
    }
    textureStore(stroke, texel, vec4<f32>(m, 0.0, 0.0, 0.0));
    mark_tile(texel);
    let b = textureLoad(base_color, texel, 0).r;
    // With a texture, its brightness is the grey, and see-through parts leave the mask alone.
    let grey = select(s.colour.r, dot(k.colour.rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), u32(s.tex.x) != 0u);
    var v = mix(b, grey, m * k.colour.a);
    if (u32(s.viewport.w) == MODE_ERASE) {
        // Erasing a mask brings the layer back.
        v = mix(b, 1.0, m);
    }
    return vec4<f32>(v, v, v, 1.0);
}

// ---------------------------------------------------------------- camera distance buffer

struct DistOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
};

@vertex
fn vs_dist(v: VsIn) -> DistOut {
    var o: DistOut;
    o.clip = s.view_proj * vec4<f32>(v.pos, 1.0);
    o.world = v.pos;
    return o;
}

@fragment
fn fs_dist(i: DistOut) -> @location(0) vec4<f32> {
    return vec4<f32>(distance(i.world, s.eye.xyz), 0.0, 0.0, 0.0);
}
