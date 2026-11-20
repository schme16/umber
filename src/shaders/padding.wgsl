// Padding past UV island edges. `vs_islands`/`fs_islands` mark every texel the mesh's UVs
// cover with its own coordinates (packed x | y << 16); everything else starts empty. A jump
// flood then gives every texel the coordinates of the nearest covered one, which the
// compositor's resolve reads from instead, so filtering and mips never pull in the colour
// from outside an island.

const NO_SEED: u32 = 0xffffffffu;

struct IslandOut {
    @builtin(position) clip: vec4<f32>,
};

@vertex
fn vs_islands(@location(0) uv: vec2<f32>) -> IslandOut {
    var o: IslandOut;
    o.clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@fragment
fn fs_islands(i: IslandOut) -> @location(0) u32 {
    let t = vec2<u32>(floor(i.clip.xy));
    return t.x | (t.y << 16u);
}

struct Step {
    step: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> j: Step;
@group(0) @binding(1) var src: texture_2d<u32>;
@group(0) @binding(2) var dst: texture_storage_2d<r32uint, write>;

fn unpack(v: u32) -> vec2<i32> {
    return vec2<i32>(i32(v & 0xffffu), i32(v >> 16u));
}

@compute @workgroup_size(8, 8)
fn jump(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(src));
    let c = vec2<i32>(gid.xy);
    if (c.x >= dims.x || c.y >= dims.y) {
        return;
    }
    var best = textureLoad(src, c, 0).r;
    var best_d = 1e30;
    if (best != NO_SEED) {
        let d = vec2<f32>(unpack(best) - c);
        best_d = dot(d, d);
    }
    let k = i32(j.step);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            let p = c + vec2<i32>(dx, dy) * k;
            if (p.x < 0 || p.y < 0 || p.x >= dims.x || p.y >= dims.y) {
                continue;
            }
            let s = textureLoad(src, p, 0).r;
            if (s == NO_SEED) {
                continue;
            }
            let d = vec2<f32>(unpack(s) - c);
            let dd = dot(d, d);
            if (dd < best_d) {
                best_d = dd;
                best = s;
            }
        }
    }
    textureStore(dst, c, vec4<u32>(best, 0u, 0u, 0u));
}
