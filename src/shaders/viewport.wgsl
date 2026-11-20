// Viewport preview shading: GGX key + fill + rim lights, hemisphere ambient, ACES tonemap.
// Output target is sRGB, so everything here stays linear.

struct Globals {
    view_proj: mat4x4<f32>,
    cam_pos: vec4<f32>,
    key_dir: vec4<f32>,
    fill_dir: vec4<f32>,
    params: vec4<f32>, // unused, unused, view mode, unused
    misc: vec4<f32>,   // exposure
};

@group(0) @binding(0) var<uniform> g: Globals;
// The texture set's maps: colour (sRGB view, so samples are linear), material
// (roughness, metalness, height, AO) and a tangent-space normal map (OpenGL convention).
@group(1) @binding(0) var color_tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;
@group(1) @binding(2) var mat_tex: texture_2d<f32>;
@group(1) @binding(3) var normal_tex: texture_2d<f32>;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) tangent: vec4<f32>,
};

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var o: VsOut;
    o.clip = g.view_proj * vec4<f32>(v.pos, 1.0);
    o.world = v.pos;
    o.normal = v.normal;
    o.uv = v.uv;
    o.tangent = v.tangent;
    return o;
}

const PI: f32 = 3.14159265;

fn d_ggx(nh: f32, a: f32) -> f32 {
    let a2 = a * a;
    let d = nh * nh * (a2 - 1.0) + 1.0;
    return a2 / (PI * d * d);
}

fn v_smith(nv: f32, nl: f32, a: f32) -> f32 {
    let k = a * 0.5;
    return 0.25 / ((nv * (1.0 - k) + k) * (nl * (1.0 - k) + k));
}

fn f_schlick(f0: vec3<f32>, vh: f32) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(1.0 - vh, 5.0);
}

fn shade(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, radiance: vec3<f32>, albedo: vec3<f32>, f0: vec3<f32>, rough: f32, metal: f32) -> vec3<f32> {
    let h = normalize(v + l);
    let nl = max(dot(n, l), 0.0);
    let nv = max(dot(n, v), 1e-4);
    let nh = max(dot(n, h), 0.0);
    let vh = max(dot(v, h), 0.0);
    let a = max(rough * rough, 0.002);
    let spec = d_ggx(nh, a) * v_smith(nv, nl, a) * f_schlick(f0, vh);
    let diffuse = (1.0 - metal) * albedo / PI;
    return (diffuse + spec) * radiance * nl;
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn uv_checker(uv: vec2<f32>) -> vec3<f32> {
    let cell = floor(uv * 8.0);
    let parity = (cell.x + cell.y) - 2.0 * floor((cell.x + cell.y) * 0.5);
    let base = mix(0.16, 0.72, parity);
    let tint = mix(vec3<f32>(1.0, 0.62, 0.28), vec3<f32>(0.36, 0.6, 1.0), fract(uv.x));
    return vec3<f32>(base) * mix(vec3<f32>(1.0), tint, 0.45);
}

@fragment
fn fs_main(i: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    var ng = normalize(i.normal);
    if (!front) {
        ng = -ng;
    }
    // Normal map: +X along the tangent (+u), +Y towards the top of the image (-v).
    let t = normalize(i.tangent.xyz - ng * dot(ng, i.tangent.xyz));
    let b_up = -cross(ng, t) * i.tangent.w;
    let nt = textureSample(normal_tex, samp, i.uv).xyz * 2.0 - vec3<f32>(1.0);
    var n = normalize(t * nt.x + b_up * nt.y + ng * nt.z);

    let v = normalize(g.cam_pos.xyz - i.world);
    var albedo = textureSample(color_tex, samp, i.uv).rgb;
    let mat = textureSample(mat_tex, samp, i.uv);

    let mode = u32(g.params.z + 0.5);
    if (mode == 2u) {
        albedo = uv_checker(i.uv);
    }
    if (mode == 1u) {
        return vec4<f32>(albedo, 1.0);
    }
    if (mode == 3u) {
        return vec4<f32>(pow(n * 0.5 + vec3<f32>(0.5), vec3<f32>(2.2)), 1.0);
    }

    if (mode == 2u) {
        n = ng;
    }
    let rough = select(clamp(mat.r, 0.04, 1.0), 0.55, mode == 2u);
    let metal = select(mat.g, 0.0, mode == 2u);
    let ao = select(mat.a, 1.0, mode == 2u);
    let f0 = mix(vec3<f32>(0.04), albedo, metal);

    let key = g.key_dir.xyz;
    let fill = g.fill_dir.xyz;
    // Rim light sits behind the model relative to the camera.
    let rim = normalize(-v + vec3<f32>(0.0, 0.6, 0.0));

    var color = shade(n, v, key, vec3<f32>(1.0, 0.95, 0.88) * 3.2, albedo, f0, rough, metal);
    color += shade(n, v, fill, vec3<f32>(0.55, 0.65, 0.85) * 0.9, albedo, f0, rough, metal);
    color += shade(n, v, rim, vec3<f32>(0.9, 0.92, 1.0) * 0.8, albedo, f0, rough, metal);

    let sky = vec3<f32>(0.30, 0.33, 0.38);
    let ground = vec3<f32>(0.11, 0.10, 0.09);
    let hemi = mix(ground, sky, n.y * 0.5 + 0.5);
    let r = reflect(-v, n);
    let env_spec = mix(ground, sky * 1.4, r.y * 0.5 + 0.5) * f_schlick(f0, max(dot(n, v), 0.0)) * (1.0 - rough * 0.7);
    color += (hemi * albedo * (1.0 - metal) + env_spec) * ao;

    return vec4<f32>(aces(color * g.misc.x), 1.0);
}
