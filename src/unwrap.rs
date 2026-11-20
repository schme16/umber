//! Automatic UV unwrapping and repacking (xatlas), and the UV health readout.
//!
//! Each material has its own texture set, so each gets its own 0 to 1 UV square: the mesh is
//! unwrapped one material at a time. Triangles keep their order, so submesh ranges stay valid.
//! Every new vertex remembers the UV it had before, so paint can follow it to the new layout.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use glam::{Vec2, Vec3};
use xatlas_rs_v2 as xatlas;

use crate::mesh::{self, Mesh};
use crate::texset;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Cut the mesh into charts from scratch and pack them.
    Unwrap,
    /// Keep every island's shape and lay them out again, tighter.
    Repack,
}

/// Shared with the worker thread: how far along it is (0 to 1000) and whether to stop.
#[derive(Clone, Default)]
pub struct Progress {
    permille: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        self.permille.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled()
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The mesh with its new UVs, and for every vertex the UV it had before.
pub struct Unwrapped {
    pub mesh: Mesh,
    pub old_uv: Vec<[f32; 2]>,
}

/// Charts are packed at this resolution, then scaled to fill the texture. Packing at the
/// full texture size took half as long again for the same layout.
const PACK_RESOLUTION: u32 = 1024;
/// Texels between charts at the packing resolution (about twice that in the texture).
const PADDING: u32 = 3;

pub fn run(mesh: &Mesh, mode: Mode, progress: &Progress) -> Result<Unwrapped> {
    // Triangles of each material, in mesh order.
    let mut by_material: Vec<Vec<usize>> = vec![Vec::new(); mesh.materials.len().max(1)];
    let last = by_material.len() - 1;
    for s in &mesh.submeshes {
        let first = s.index_start as usize / 3;
        by_material[s.material.min(last)].extend(first..first + s.index_count as usize / 3);
    }
    let groups: Vec<&Vec<usize>> = by_material.iter().filter(|t| !t.is_empty()).collect();

    let mut vertices = Vec::new();
    let mut old_uv = Vec::new();
    let mut indices = vec![0u32; mesh.indices.len()];
    for (g, tris) in groups.iter().enumerate() {
        let span = (g as f32 / groups.len() as f32, (g + 1) as f32 / groups.len() as f32);
        let charts = unwrap_group(mesh, tris, mode, progress, span)?;
        let base = vertices.len() as u32;
        for (src, uv) in charts.vertices {
            let mut v = mesh.vertices[src as usize];
            old_uv.push(v.uv);
            v.uv = uv;
            vertices.push(v);
        }
        for (k, &t) in tris.iter().enumerate() {
            for c in 0..3 {
                indices[t * 3 + c] = base + charts.indices[k * 3 + c];
            }
        }
    }
    mesh::compute_tangents(&mut vertices, &indices);
    let mut out = mesh.clone();
    out.vertices = vertices;
    out.indices = indices;
    out.has_uvs = true;
    progress.permille.store(1000, Ordering::Relaxed);
    Ok(Unwrapped { mesh: out, old_uv })
}

/// One material's triangles laid out in their own UV square.
struct Charts {
    /// Source vertex (index into the whole mesh) and its new UV.
    vertices: Vec<(u32, [f32; 2])>,
    /// Three per triangle, in the order the triangles were given, into `vertices`.
    indices: Vec<u32>,
}

fn unwrap_group(mesh: &Mesh, tris: &[usize], mode: Mode, progress: &Progress, span: (f32, f32)) -> Result<Charts> {
    // A compact vertex list for just these triangles.
    let mut local: HashMap<u32, u32> = HashMap::new();
    let mut source = Vec::new();
    let mut idx = Vec::with_capacity(tris.len() * 3);
    for &t in tris {
        for &v in &mesh.indices[t * 3..t * 3 + 3] {
            let next = source.len() as u32;
            let l = *local.entry(v).or_insert_with(|| {
                source.push(v);
                next
            });
            idx.push(l);
        }
    }
    let pos: Vec<f32> = source.iter().flat_map(|&v| mesh.vertices[v as usize].pos).collect();
    let nrm: Vec<f32> = source.iter().flat_map(|&v| mesh.vertices[v as usize].normal).collect();
    let uvs: Vec<f32> = source.iter().flat_map(|&v| mesh.vertices[v as usize].uv).collect();

    // Declarations must outlive the atlas, which reads them in place.
    let decl = xatlas::MeshDecl {
        vertex_position_data: xatlas::MeshData::Contiguous(&pos),
        vertex_normal_data: Some(xatlas::MeshData::Contiguous(&nrm)),
        index_data: Some(xatlas::IndexData::U32(&idx)),
        face_count: tris.len() as u32,
        ..Default::default()
    };
    let uv_decl = xatlas::UvMeshDecl {
        // Contiguous data is counted in threes, so two-float UVs need an explicit stride.
        vertex_uv_data: Some(xatlas::MeshData::WithStride { data: bytemuck::cast_slice(&uvs), stride: 8 }),
        face_material_data: None,
        index_data: Some(xatlas::IndexData::U32(&idx)),
    };
    let mut atlas = xatlas::Xatlas::new();
    let added = match mode {
        Mode::Unwrap => atlas.add_mesh(&decl),
        Mode::Repack => atlas.add_uv_mesh(&uv_decl),
    };
    if let Err(e) = added {
        bail!("xatlas couldn't read the mesh ({e:?})");
    }

    {
        let progress = progress.clone();
        atlas.set_progress_callback(move |category, pct| {
            // Charting is the long part of an unwrap; packing is all a repack does.
            let (from, to) = match category {
                xatlas::ProgressCategory::AddMesh => (0.0, 0.05),
                xatlas::ProgressCategory::ComputeCharts => (0.05, 0.6),
                xatlas::ProgressCategory::PackCharts => (0.6, 0.97),
                xatlas::ProgressCategory::BuildOutputMeshes => (0.97, 1.0),
            };
            let local = from + (to - from) * pct as f32 / 100.0;
            let overall = span.0 + (span.1 - span.0) * local;
            progress.permille.fetch_max((overall * 1000.0) as u32, Ordering::Relaxed);
            !progress.cancelled()
        });
    }

    let charts = xatlas::ChartOptions { fix_winding: true, ..Default::default() };
    atlas.compute_charts(&charts);
    if progress.cancelled() {
        bail!("Cancelled");
    }
    // xatlas picks the scale for the resolution; when its estimate spills onto a second
    // atlas, shrink until everything fits on one. (Searching upwards from the estimate never
    // fitted more in on test models, and brute-force packing gains a few percent for ten
    // times the wait.)
    let mut pack = xatlas::PackOptions { resolution: PACK_RESOLUTION, padding: PADDING, ..Default::default() };
    atlas.pack_charts(&pack);
    for _ in 0..12 {
        if atlas.atlas_count() <= 1 || progress.cancelled() {
            break;
        }
        pack.texels_per_unit = atlas.texels_per_unit() * 0.9;
        atlas.pack_charts(&pack);
    }
    if progress.cancelled() {
        bail!("Cancelled");
    }
    if atlas.atlas_count() > 1 {
        bail!("Couldn't fit the charts into one texture");
    }

    // The atlas comes back cropped to what it used, not always square. One scale for both
    // axes keeps every chart's shape.
    let scale = atlas.width().max(atlas.height()).max(1) as f32;
    let meshes = atlas.meshes();
    let Some(out) = meshes.first() else { bail!("xatlas returned nothing") };
    if out.index_array.len() != idx.len() {
        bail!("xatlas dropped some triangles");
    }
    Ok(Charts {
        vertices: out.vertex_array.iter().map(|v| (source[v.xref as usize], [v.uv[0] / scale, v.uv[1] / scale])).collect(),
        indices: out.index_array.to_vec(),
    })
}

// ---------------------------------------------------------------- health

/// Side of the coverage map the health check rasterises into.
pub const MAP: usize = 512;

/// How one material's UVs hold up for painting.
#[derive(Clone)]
pub struct Health {
    /// Triangles with surface area (the ones that can take paint).
    pub surfaced: usize,
    pub islands: usize,
    /// Share of the UV square covered.
    pub used: f32,
    /// Share of the covered texels that more than one triangle claims.
    pub overlap: f32,
    /// Share of the UV area that lies outside the 0 to 1 square.
    pub outside: f32,
    /// Area-weighted mean of each triangle's stretch (long axis over short axis, 1 is none).
    pub stretch: f32,
    /// Share of the surface stretched more than 1.5 to 1.
    pub badly_stretched: f32,
    /// Texels per model unit (metres for glTF) at the texture size.
    pub density: f32,
    /// How much the density varies: the 90th percentile over the 10th, by area.
    pub density_spread: f32,
    /// Triangles with surface area but no UV area: paint can't land on them.
    pub collapsed: usize,
    /// Per texel of the coverage map: 0 empty, 1 one triangle, 2 overlapping.
    pub map: Vec<u8>,
}

impl Health {
    /// Fewer than one triangle in a hundred has UV area: the model has no usable UVs.
    pub fn unusable(&self) -> bool {
        self.surfaced > 0 && self.collapsed * 100 > self.surfaced * 99
    }
}

/// Check the UVs of one material's triangles.
pub fn health(mesh: &Mesh, material: usize) -> Health {
    let tris: Vec<usize> = mesh
        .submeshes
        .iter()
        .filter(|s| s.material == material)
        .flat_map(|s| {
            let first = s.index_start as usize / 3;
            first..first + s.index_count as usize / 3
        })
        .collect();

    let corner = |t: usize, c: usize| &mesh.vertices[mesh.indices[t * 3 + c] as usize];
    // Per triangle: surface area, UV area, stretch.
    let mut rows: Vec<(f32, f32, f32)> = Vec::with_capacity(tris.len());
    let mut outside_area = 0.0f64;
    let mut collapsed = 0;
    let mut surfaced = 0;
    for &t in &tris {
        let [a, b, c] = [0, 1, 2].map(|k| corner(t, k));
        let (p0, p1, p2) = (Vec3::from(a.pos), Vec3::from(b.pos), Vec3::from(c.pos));
        let (q0, q1, q2) = (Vec2::from(a.uv), Vec2::from(b.uv), Vec2::from(c.uv));
        let area = (p1 - p0).cross(p2 - p0).length() * 0.5;
        let uv_area = (q1 - q0).perp_dot(q2 - q0).abs() * 0.5;
        if area <= f32::EPSILON {
            continue;
        }
        surfaced += 1;
        if uv_area <= 1e-12 {
            collapsed += 1;
            continue;
        }
        rows.push((area, uv_area, stretch_of([p0, p1, p2], [q0, q1, q2])));
        outside_area += uv_area as f64 * (1.0 - clipped_fraction([q0, q1, q2]) as f64);
    }

    let area_sum: f64 = rows.iter().map(|r| r.0 as f64).sum();
    let uv_sum: f64 = rows.iter().map(|r| r.1 as f64).sum();
    let weighted = |f: &dyn Fn(&(f32, f32, f32)) -> f64| {
        if area_sum > 0.0 { rows.iter().map(|r| r.0 as f64 * f(r)).sum::<f64>() / area_sum } else { 0.0 }
    };
    let stretch = weighted(&|r| r.2 as f64) as f32;
    let badly_stretched = weighted(&|r| if r.2 > 1.5 { 1.0 } else { 0.0 }) as f32;
    let size = texset::SIZE as f64;
    let density = if area_sum > 0.0 { ((uv_sum / area_sum).sqrt() * size) as f32 } else { 0.0 };

    // Spread of per-triangle density, by area.
    let mut dens: Vec<(f32, f32)> = rows.iter().map(|r| ((r.1 / r.0).sqrt(), r.0)).collect();
    dens.sort_by(|a, b| a.0.total_cmp(&b.0));
    let percentile = |p: f64| {
        let mut acc = 0.0;
        for &(d, a) in &dens {
            acc += a as f64;
            if acc >= p * area_sum {
                return d;
            }
        }
        dens.last().map_or(0.0, |d| d.0)
    };
    let density_spread = if dens.is_empty() { 1.0 } else { percentile(0.9) / percentile(0.1).max(f32::MIN_POSITIVE) };

    // Coverage map.
    let mut count = vec![0u8; MAP * MAP];
    for &t in &tris {
        let q = [0, 1, 2].map(|k| Vec2::from(corner(t, k).uv) * MAP as f32);
        raster(q, |x, y| {
            let c = &mut count[y * MAP + x];
            *c = c.saturating_add(1);
        });
    }
    let covered = count.iter().filter(|&&c| c > 0).count();
    let overlapping = count.iter().filter(|&&c| c > 1).count();
    let map = count.iter().map(|&c| c.min(2)).collect();

    let islands = {
        let all = mesh.uv_islands();
        let mut seen: Vec<u32> = tris.iter().map(|&t| all[t]).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };

    Health {
        surfaced,
        islands,
        used: covered as f32 / (MAP * MAP) as f32,
        overlap: if covered > 0 { overlapping as f32 / covered as f32 } else { 0.0 },
        outside: if uv_sum > 0.0 { (outside_area / uv_sum) as f32 } else { 0.0 },
        stretch,
        badly_stretched,
        density,
        density_spread,
        collapsed,
        map,
    }
}

/// Long axis over short axis of the ellipse a circle on the surface becomes in UV space.
fn stretch_of(p: [Vec3; 3], q: [Vec2; 3]) -> f32 {
    // The triangle in its own plane.
    let e1 = p[1] - p[0];
    let e2 = p[2] - p[0];
    let x = e1.normalize_or_zero();
    let y = e1.cross(e2).cross(e1).normalize_or_zero();
    let (a, b) = (Vec2::new(e1.length(), 0.0), Vec2::new(e2.dot(x), e2.dot(y)));
    let (d1, d2) = (q[1] - q[0], q[2] - q[0]);
    // M maps surface offsets to UV offsets: M [a b] = [d1 d2].
    let det = a.x * b.y - b.x * a.y;
    if det.abs() < f32::MIN_POSITIVE {
        return 1.0;
    }
    let inv = [[b.y / det, -b.x / det], [-a.y / det, a.x / det]]; // rows of [a b]^-1
    let m = [
        [d1.x * inv[0][0] + d2.x * inv[1][0], d1.x * inv[0][1] + d2.x * inv[1][1]],
        [d1.y * inv[0][0] + d2.y * inv[1][0], d1.y * inv[0][1] + d2.y * inv[1][1]],
    ];
    // Singular values of a 2x2 matrix.
    let (p_, q_) = ((m[0][0] + m[1][1]) * 0.5, (m[0][0] - m[1][1]) * 0.5);
    let (r_, s_) = ((m[1][0] + m[0][1]) * 0.5, (m[1][0] - m[0][1]) * 0.5);
    let (h1, h2) = ((p_ * p_ + s_ * s_).sqrt(), (q_ * q_ + r_ * r_).sqrt());
    let (big, small) = (h1 + h2, (h1 - h2).abs());
    if small <= big * 1e-6 { 1e6 } else { big / small }
}

/// Share of a UV triangle's area inside the 0 to 1 square, by sampling.
fn clipped_fraction(q: [Vec2; 3]) -> f32 {
    let inside = |p: Vec2| (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y);
    if q.iter().all(|&p| inside(p)) {
        return 1.0;
    }
    const N: u32 = 6;
    let (mut hit, mut total) = (0, 0);
    for i in 0..N {
        for j in 0..N - i {
            let (u, v) = ((i as f32 + 1.0 / 3.0) / N as f32, (j as f32 + 1.0 / 3.0) / N as f32);
            total += 1;
            if inside(q[0] + (q[1] - q[0]) * u + (q[2] - q[0]) * v) {
                hit += 1;
            }
        }
    }
    hit as f32 / total as f32
}

/// Every texel centre of the map inside the triangle (in texels), with a top-left rule so
/// triangles sharing an edge never both claim a texel on it.
fn raster(q: [Vec2; 3], mut plot: impl FnMut(usize, usize)) {
    let [mut a, mut b, c] = q;
    if (b - a).perp_dot(c - a) < 0.0 {
        std::mem::swap(&mut a, &mut b);
    }
    let area = (b - a).perp_dot(c - a);
    if area <= 0.0 {
        return;
    }
    let lo = a.min(b).min(c).floor().max(Vec2::ZERO);
    let hi = a.max(b).max(c).ceil().min(Vec2::splat(MAP as f32));
    if lo.x >= hi.x || lo.y >= hi.y {
        return;
    }
    // Edge from u to v owns its points when it's a top edge or a left edge.
    let owns = |u: Vec2, v: Vec2| {
        let d = v - u;
        (d.y == 0.0 && d.x < 0.0) || d.y > 0.0
    };
    let edges = [(b, c), (c, a), (a, b)];
    let bias: [bool; 3] = edges.map(|(u, v)| owns(u, v));
    for y in lo.y as usize..hi.y as usize {
        for x in lo.x as usize..hi.x as usize {
            let p = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
            let inside = edges.iter().zip(bias).all(|(&(u, v), own)| {
                let w = (v - u).perp_dot(p - u);
                w > 0.0 || (w == 0.0 && own)
            });
            if inside {
                plot(x, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_uvs(mut m: Mesh) -> Mesh {
        for v in &mut m.vertices {
            v.uv = [0.0; 2];
        }
        m.has_uvs = false;
        m
    }

    #[test]
    fn unwrap_gives_a_clean_layout() {
        let mesh = no_uvs(mesh::sphere(48, 24));
        assert!(health(&mesh, 0).unusable());
        let out = run(&mesh, Mode::Unwrap, &Progress::default()).expect("unwrap");
        assert_eq!(out.mesh.indices.len(), mesh.indices.len());
        assert_eq!(out.old_uv.len(), out.mesh.vertices.len());
        let h = health(&out.mesh, 0);
        assert_eq!(h.collapsed, 0);
        assert!(h.overlap < 0.002, "overlap {}", h.overlap);
        assert!(h.outside == 0.0, "outside {}", h.outside);
        assert!(h.used > 0.4, "used {}", h.used);
        assert!(h.stretch < 1.3, "stretch {}", h.stretch);
        // Same triangles, same places.
        for (a, b) in mesh.indices.iter().zip(&out.mesh.indices) {
            assert_eq!(mesh.vertices[*a as usize].pos, out.mesh.vertices[*b as usize].pos);
        }
    }

    #[test]
    fn repack_keeps_island_shapes() {
        // The cube's six faces squeezed into a quarter of the square.
        let mut mesh = mesh::cube();
        for v in &mut mesh.vertices {
            v.uv = [v.uv[0] * 0.5, v.uv[1] * 0.5];
        }
        let before = health(&mesh, 0);
        let out = run(&mesh, Mode::Repack, &Progress::default()).expect("repack");
        let after = health(&out.mesh, 0);
        assert_eq!(after.islands, before.islands);
        assert!((after.stretch - before.stretch).abs() < 0.01, "{} vs {}", before.stretch, after.stretch);
        assert!(after.used > before.used, "{} vs {}", before.used, after.used);
        assert!(after.overlap == 0.0);
    }

    #[test]
    fn health_finds_overlaps_and_spills() {
        // Two quads on the same UVs, half of each outside the square.
        let mut m = mesh::cube();
        for v in &mut m.vertices {
            v.uv = [v.uv[0] * 3.0 % 1.0, v.uv[1] * 2.0 % 1.0];
        }
        let h = health(&m, 0);
        assert!(h.overlap > 0.9, "overlap {}", h.overlap);
        let mut m = mesh::cube();
        for v in &mut m.vertices {
            v.uv = [v.uv[0] + 0.5, v.uv[1]];
        }
        let h = health(&m, 0);
        assert!(h.outside > 0.3 && h.outside < 0.7, "outside {}", h.outside);
    }

    #[test]
    fn stretch_measures_shape_not_scale() {
        let p = [Vec3::ZERO, Vec3::X, Vec3::Y];
        let even = stretch_of(p, [Vec2::ZERO, Vec2::X * 0.1, Vec2::Y * 0.1]);
        assert!((even - 1.0).abs() < 1e-4);
        let squashed = stretch_of(p, [Vec2::ZERO, Vec2::X * 0.2, Vec2::Y * 0.1]);
        assert!((squashed - 2.0).abs() < 1e-4);
    }
}
