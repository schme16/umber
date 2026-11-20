//! Textures the brush can paint with instead of one flat colour: a few built-in patterns, the
//! textures the model came with, and image files.
//!
//! A brush texture is two GPU textures with full mip chains: colour with alpha (where alpha is
//! clear the brush lays nothing down, so a PNG with holes paints like a stencil) and height.
//! Built-in patterns draw their own height; for images it is the brightness.

use std::path::{Path, PathBuf};

use eframe::egui_wgpu::wgpu;

use crate::mesh::Image;

/// Built-in patterns are drawn at this size, and tile.
const PATTERN: u32 = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Pattern {
    Bricks,
    Planks,
    Tiles,
    Grunge,
}

impl Pattern {
    pub const ALL: [Pattern; 4] = [Pattern::Bricks, Pattern::Planks, Pattern::Tiles, Pattern::Grunge];

    pub fn name(self) -> &'static str {
        match self {
            Pattern::Bricks => "Bricks",
            Pattern::Planks => "Planks",
            Pattern::Tiles => "Tiles",
            Pattern::Grunge => "Grunge",
        }
    }
}

/// Where a brush texture comes from. Saved with the brush settings.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Source {
    Pattern(Pattern),
    /// The base colour texture of one of the model's materials.
    Model(usize),
    File(PathBuf),
}

impl Source {
    pub fn name(&self, materials: &[crate::mesh::Material]) -> String {
        match self {
            Source::Pattern(p) => p.name().to_owned(),
            Source::Model(i) => materials.get(*i).map_or_else(|| "Model texture".to_owned(), |m| format!("{} (from the model)", m.name)),
            Source::File(p) => p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()),
        }
    }
}

/// How the texture is laid onto the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Projection {
    /// Projected from the three axes and blended by the surface's facing, so it keeps one size
    /// across seams and stretched UVs.
    #[default]
    Surface,
    /// Tiled across the UV square.
    Uv,
    /// Fixed to the screen, like painting through a stencil.
    View,
}

impl Projection {
    pub const ALL: [Projection; 3] = [Projection::Surface, Projection::Uv, Projection::View];

    pub fn name(self) -> &'static str {
        match self {
            Projection::Surface => "Surface",
            Projection::Uv => "UV",
            Projection::View => "View",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Projection::Surface => "Wraps the model from all sides, so it keeps one size across seams and stretched UVs",
            Projection::Uv => "Follows the model's UV layout, the way a texture tiles in a game engine",
            Projection::View => "Stays fixed to the screen, like painting through a stencil",
        }
    }

    /// What the shader calls this projection (0 is no texture).
    pub fn code(self) -> u32 {
        match self {
            Projection::Surface => 1,
            Projection::Uv => 2,
            Projection::View => 3,
        }
    }

    /// The size of one repeat at scale 1: a quarter of the model, a quarter of the UV square,
    /// or 512 screen pixels.
    pub fn tile(self, model_size: f32, ppp: f32) -> f32 {
        match self {
            Projection::Surface => model_size.max(1e-4) * 0.25,
            Projection::Uv => 0.25,
            Projection::View => 512.0 * ppp,
        }
    }
}

/// A colour image and its height, both straight RGBA8 (height in every channel).
#[derive(Clone)]
pub struct Pixels {
    pub colour: Image,
    pub height: Image,
}

impl Pixels {
    /// Height from brightness, for images that don't come with one.
    pub fn from_image(colour: Image) -> Self {
        let rgba = colour
            .rgba
            .chunks_exact(4)
            .flat_map(|p| {
                let l = crate::texset::linear_to_srgb8(
                    0.2126 * crate::texset::srgb8_to_linear(p[0]) + 0.7152 * crate::texset::srgb8_to_linear(p[1]) + 0.0722 * crate::texset::srgb8_to_linear(p[2]),
                );
                [l, l, l, 255]
            })
            .collect();
        let height = Image { width: colour.width, height: colour.height, rgba };
        Self { colour, height }
    }
}

/// Read an image file for the brush.
pub fn load_file(path: &Path) -> anyhow::Result<Pixels> {
    let img = image::open(path)?.to_rgba8();
    let (width, height) = img.dimensions();
    anyhow::ensure!(width > 0 && height > 0, "the image is empty");
    Ok(Pixels::from_image(Image { width, height, rgba: img.into_raw() }))
}

/// A brush texture on the GPU.
pub struct GpuTexture {
    pub colour: wgpu::TextureView,
    pub height: wgpu::TextureView,
}

impl GpuTexture {
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, px: &Pixels) -> Self {
        Self { colour: upload(device, queue, "brush texture", &px.colour), height: upload(device, queue, "brush texture height", &px.height) }
    }
}

/// Upload with a full mip chain, so a big texture painted small doesn't sparkle.
fn upload(device: &wgpu::Device, queue: &wgpu::Queue, label: &str, img: &Image) -> wgpu::TextureView {
    let levels = 32 - img.width.max(img.height).leading_zeros();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut level = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone()).expect("sized to match");
    for mip in 0..levels {
        let (w, h) = level.dimensions();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: mip, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            level.as_raw(),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        if mip + 1 < levels {
            level = image::imageops::resize(&level, (w / 2).max(1), (h / 2).max(1), image::imageops::FilterType::Triangle);
        }
    }
    texture.create_view(&Default::default())
}

/// A small picture of the texture for the picker, `size` texels square.
pub fn thumbnail(img: &Image, size: u32) -> egui::ColorImage {
    let src = image::RgbaImage::from_raw(img.width, img.height, img.rgba.clone()).expect("sized to match");
    // Crop to a square from the middle, so a wide photo isn't squashed.
    let side = img.width.min(img.height);
    let crop = image::imageops::crop_imm(&src, (img.width - side) / 2, (img.height - side) / 2, side, side).to_image();
    let small = image::imageops::resize(&crop, size, size, image::imageops::FilterType::Triangle);
    // Over the panel colour, so see-through patterns read as what they'd lay down.
    let bg = crate::theme::RAISED;
    let mut rgb = Vec::with_capacity((size * size * 3) as usize);
    for p in small.pixels() {
        let a = p[3] as f32 / 255.0;
        for (c, b) in [(p[0], bg.r()), (p[1], bg.g()), (p[2], bg.b())] {
            rgb.push((c as f32 * a + b as f32 * (1.0 - a)).round() as u8);
        }
    }
    egui::ColorImage::from_rgb([size as usize, size as usize], &rgb)
}

// ---------------------------------------------------------------- built-in patterns

/// A built-in pattern. Each is drawn once (about 50 ms) and kept, so the picker and the brush
/// share it. Every pattern tiles.
pub fn pattern(p: Pattern) -> Pixels {
    static DRAWN: std::sync::Mutex<Vec<(Pattern, Pixels)>> = std::sync::Mutex::new(Vec::new());
    let mut drawn = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, px)) = drawn.iter().find(|(q, _)| *q == p) {
        return px.clone();
    }
    let px = draw_pattern(p);
    drawn.push((p, px.clone()));
    px
}

fn draw_pattern(p: Pattern) -> Pixels {
    let n = PATTERN;
    let mut colour = Vec::with_capacity((n * n * 4) as usize);
    let mut height = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (c, a, h) = match p {
                Pattern::Bricks => bricks(x, y),
                Pattern::Planks => planks(x, y),
                Pattern::Tiles => tiles(x, y),
                Pattern::Grunge => grunge(x, y),
            };
            colour.extend_from_slice(&[to8(c[0]), to8(c[1]), to8(c[2]), to8(a)]);
            let h = to8(h);
            height.extend_from_slice(&[h, h, h, 255]);
        }
    }
    Pixels { colour: Image { width: n, height: n, rgba: colour }, height: Image { width: n, height: n, rgba: height } }
}

fn to8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn scale3(c: [f32; 3], k: f32) -> [f32; 3] {
    [c[0] * k, c[1] * k, c[2] * k]
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d) ^ (y as u32).wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9e37_79b9);
    h = (h ^ (h >> 15)).wrapping_mul(0x85eb_ca6b);
    h = (h ^ (h >> 13)).wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    (h & 0xff_ffff) as f32 / 16_777_215.0
}

/// Smooth noise in 0..1 over a lattice of `period` cells that wraps, so it tiles.
fn noise(x: f32, y: f32, period: i32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - xi as f32, y - yi as f32);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let at = |i: i32, j: i32| hash(i.rem_euclid(period), j.rem_euclid(period), seed);
    let a = at(xi, yi) + (at(xi + 1, yi) - at(xi, yi)) * sx;
    let b = at(xi, yi + 1) + (at(xi + 1, yi + 1) - at(xi, yi + 1)) * sx;
    a + (b - a) * sy
}

/// Layered noise in roughly 0..1. `u`, `v` run 0..1 across the pattern.
fn fbm(u: f32, v: f32, cells: i32, octaves: u32, seed: u32) -> f32 {
    let (mut sum, mut amp, mut total, mut period) = (0.0, 0.5, 0.0, cells);
    for o in 0..octaves {
        sum += noise(u * period as f32, v * period as f32, period, seed + o) * amp;
        total += amp;
        amp *= 0.5;
        period *= 2;
    }
    sum / total
}

fn uv(x: u32, y: u32) -> (f32, f32) {
    ((x as f32 + 0.5) / PATTERN as f32, (y as f32 + 0.5) / PATTERN as f32)
}

/// Red-brown bricks in running bond with pale mortar.
fn bricks(x: u32, y: u32) -> ([f32; 3], f32, f32) {
    let (rows, per_row, mortar) = (8, 4, 3.5);
    let bh = PATTERN / rows;
    let bw = PATTERN / per_row;
    let row = y / bh;
    let shifted = (x + (row % 2) * bw / 2) % PATTERN;
    let col = shifted / bw;
    let (lx, ly) = ((shifted % bw) as f32 + 0.5, (y % bh) as f32 + 0.5);
    let edge = lx.min(bw as f32 - lx).min(ly).min(bh as f32 - ly);
    let (u, v) = uv(x, y);
    let grit = fbm(u, v, 32, 3, 7);
    let tone = hash(col as i32, row as i32, 3);
    let in_brick = smoothstep(mortar - 0.5, mortar + 0.5, edge);
    let brick = scale3(mix3([0.55, 0.25, 0.17], [0.42, 0.19, 0.13], tone), 0.8 + 0.4 * grit);
    let brick = mix3(brick, [0.30, 0.16, 0.11], (1.0 - smoothstep(mortar, mortar + 5.0, edge)) * 0.35);
    let mortar_c = scale3([0.62, 0.59, 0.54], 0.85 + 0.3 * fbm(u, v, 64, 2, 11));
    let colour = mix3(mortar_c, brick, in_brick);
    let bevel = smoothstep(mortar, mortar + 6.0, edge);
    let h = 0.25 + 0.5 * bevel + 0.12 * (grit - 0.5) * in_brick;
    (colour, 1.0, h)
}

/// Warm wooden floorboards with grain and staggered ends.
fn planks(x: u32, y: u32) -> ([f32; 3], f32, f32) {
    let rows = 8;
    let ph = PATTERN / rows;
    let row = y / ph;
    let joint = (hash(row as i32, 0, 21) * PATTERN as f32) as u32;
    let lx = (x + PATTERN - joint) % PATTERN;
    let ly = (y % ph) as f32 + 0.5;
    let gap = 1.6;
    let edge = ly.min(ph as f32 - ly).min(lx as f32 + 0.5).min(PATTERN as f32 - lx as f32 - 0.5);
    let (u, v) = uv(x, y);
    // Grain runs along the board: noise stretched across it, then bent into rings.
    let warp = fbm(u, v * 0.25, 8, 3, 31 + row);
    let rings = (((v * rows as f32 * 6.0) + warp * 6.0 + hash(row as i32, 1, 5) * 3.0) * std::f32::consts::TAU).sin() * 0.5 + 0.5;
    let fibre = fbm(u * 0.125, v, 64, 2, 41);
    let grain = smoothstep(0.2, 0.9, rings) * 0.7 + fibre * 0.3;
    let tone = hash(row as i32, 2, 9);
    let wood = mix3([0.69, 0.48, 0.30], [0.47, 0.30, 0.18], grain);
    let wood = scale3(wood, 0.85 + 0.25 * tone);
    let in_board = smoothstep(gap - 0.5, gap + 0.8, edge);
    let colour = mix3([0.16, 0.10, 0.06], wood, in_board);
    let h = 0.2 + 0.55 * in_board - 0.06 * grain * in_board;
    (colour, 1.0, h)
}

/// Off-white square tiles with grey grout.
fn tiles(x: u32, y: u32) -> ([f32; 3], f32, f32) {
    let per = 4;
    let s = PATTERN / per;
    let (tx, ty) = (x / s, y / s);
    let (lx, ly) = ((x % s) as f32 + 0.5, (y % s) as f32 + 0.5);
    let edge = lx.min(s as f32 - lx).min(ly).min(s as f32 - ly);
    let (u, v) = uv(x, y);
    let grout = 3.0;
    let in_tile = smoothstep(grout - 0.5, grout + 0.5, edge);
    let tone = hash(tx as i32, ty as i32, 13);
    let glaze = mix3([0.86, 0.84, 0.79], [0.78, 0.76, 0.70], tone) ;
    let glaze = scale3(glaze, 0.96 + 0.06 * fbm(u, v, 16, 3, 17));
    let grout_c = scale3([0.42, 0.40, 0.37], 0.85 + 0.3 * fbm(u, v, 64, 2, 19));
    let colour = mix3(grout_c, glaze, in_tile);
    let h = 0.25 + 0.5 * smoothstep(grout, grout + 4.0, edge);
    (colour, 1.0, h)
}

/// Dark blotches of dirt; see-through between them.
fn grunge(x: u32, y: u32) -> ([f32; 3], f32, f32) {
    let (u, v) = uv(x, y);
    let n = fbm(u, v, 4, 5, 51);
    let speck = fbm(u, v, 64, 2, 53);
    let a = (smoothstep(0.48, 0.72, n) * 0.85 + smoothstep(0.72, 0.9, speck) * 0.5).min(1.0);
    let colour = scale3([0.24, 0.18, 0.12], 0.7 + 0.5 * speck);
    (colour, a, 0.5 - 0.1 * a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_tile() {
        for p in Pattern::ALL {
            let px = pattern(p);
            let n = PATTERN as usize;
            let at = |x: usize, y: usize| &px.colour.rgba[(y * n + x) * 4..(y * n + x) * 4 + 4];
            // Across the wrap, neighbours differ no more than neighbours inside do.
            let jump = |a: &[u8], b: &[u8]| a.iter().zip(b).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            let mut worst_wrap = 0;
            let mut worst_inside = 0;
            for i in 0..n {
                worst_wrap = worst_wrap.max(jump(at(n - 1, i), at(0, i))).max(jump(at(i, n - 1), at(i, 0)));
                worst_inside = worst_inside.max(jump(at(n / 2 - 1, i), at(n / 2, i))).max(jump(at(i, n / 2 - 1), at(i, n / 2)));
            }
            assert!(worst_wrap <= worst_inside.max(48), "{p:?} has a seam: {worst_wrap} across the wrap, {worst_inside} inside");
        }
    }

    #[test]
    fn height_from_brightness() {
        let img = Image { width: 2, height: 1, rgba: vec![0, 0, 0, 255, 255, 255, 255, 128] };
        let px = Pixels::from_image(img);
        assert_eq!(px.height.rgba, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }
}
