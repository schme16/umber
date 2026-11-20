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

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct UvOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

@vertex
fn vs_uv(v: VsIn) -> UvOut {
    var o: UvOut;
    o.clip = vec4<f32>(v.uv.x * 2.0 - 1.0, 1.0 - v.uv.y * 2.0, 0.0, 1.0);
    o.world = v.pos;
    o.normal = v.normal;
    return o;
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
    let a = clamp(m * s.channels, vec4<f32>(0.0), vec4<f32>(1.0));
    var o: LayerOut;
    if (u32(s.viewport.w) == MODE_ERASE) {
        o.color = vec4<f32>(bc.rgb, bc.a * (1.0 - a.x));
        o.mat = bm;
        o.cov = vec4<f32>(bv.xyz * (vec3<f32>(1.0) - a.yzw), 0.0);
        return o;
    }
    // Lay the paint over what was there: coverage adds, values mix by coverage.
    let ca = a.x + bc.a * (1.0 - a.x);
    let rgb = (s.colour.rgb * a.x + bc.rgb * bc.a * (1.0 - a.x)) / max(ca, 1e-5);
    o.color = vec4<f32>(select(bc.rgb, rgb, ca > 0.0), ca);
    let cv = a.yzw + bv.xyz * (vec3<f32>(1.0) - a.yzw);
    let mv = (s.material.xyz * a.yzw + bm.xyz * bv.xyz * (vec3<f32>(1.0) - a.yzw)) / max(cv, vec3<f32>(1e-5));
    o.mat = vec4<f32>(select(bm.xyz, mv, cv > vec3<f32>(0.0)), bm.w);
    o.cov = vec4<f32>(cv, 0.0);
    return o;
}

/// Layer masks: greyscale in red, painted towards the brush's grey value.
@fragment
fn fs_mask(i: UvOut) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(floor(i.clip.xy));
    let m = stroke_value(i, texel);
    if (m < 0.0) {
        discard;
    }
    textureStore(stroke, texel, vec4<f32>(m, 0.0, 0.0, 0.0));
    mark_tile(texel);
    let b = textureLoad(base_color, texel, 0).r;
    var v = mix(b, s.colour.r, m);
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
