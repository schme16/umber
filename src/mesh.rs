//! Mesh data and importers. Everything is flattened into one triangle list in world
//! space; submeshes remember which source material each index range came from.

use std::path::Path;

use anyhow::{bail, Context, Result};
use glam::{Mat3, Mat4, Vec2, Vec3};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    /// xyz along +u; w is the sign that turns cross(normal, tangent) into +v (down the image).
    pub tangent: [f32; 4],
}

/// One index range drawn with one material (and so one texture set).
#[derive(Clone, Debug)]
pub struct Submesh {
    pub material: usize,
    pub index_start: u32,
    pub index_count: u32,
}

/// What the source file said about a material. It seeds the material's texture set.
#[derive(Clone)]
pub struct Material {
    pub name: String,
    /// Linear RGBA factor.
    pub base_color: [f32; 4],
    pub base_color_texture: Option<Image>,
    pub roughness: f32,
    pub metallic: f32,
}

impl Material {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into(), base_color: [1.0; 4], base_color_texture: None, roughness: 0.55, metallic: 0.0 }
    }
}

/// Straight RGBA8 pixels in sRGB.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct Mesh {
    pub name: String,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub submeshes: Vec<Submesh>,
    /// False when the source had no texture coordinates; the UV workspace offers to unwrap.
    pub has_uvs: bool,
    pub materials: Vec<Material>,
}

impl Mesh {
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn bounds(&self) -> (Vec3, Vec3) {
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        for v in &self.vertices {
            let p = Vec3::from(v.pos);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        if self.vertices.is_empty() {
            (Vec3::ZERO, Vec3::ZERO)
        } else {
            (lo, hi)
        }
    }

    /// Centre and radius of the bounding sphere around the box.
    pub fn bounding_sphere(&self) -> (Vec3, f32) {
        let (lo, hi) = self.bounds();
        ((lo + hi) * 0.5, ((hi - lo).length() * 0.5).max(1e-4))
    }

    /// The nearest triangle the ray hits, both sides counted.
    pub fn raycast(&self, origin: Vec3, dir: Vec3) -> Option<Hit> {
        let mut best: Option<Hit> = None;
        for (tri, idx) in self.indices.chunks_exact(3).enumerate() {
            let [a, b, c] = [0, 1, 2].map(|k| Vec3::from(self.vertices[idx[k] as usize].pos));
            // Moller-Trumbore.
            let (e1, e2) = (b - a, c - a);
            let p = dir.cross(e2);
            let det = e1.dot(p);
            if det.abs() < 1e-12 {
                continue;
            }
            let inv = 1.0 / det;
            let s = origin - a;
            let u = s.dot(p) * inv;
            if !(0.0..=1.0).contains(&u) {
                continue;
            }
            let q = s.cross(e1);
            let v = dir.dot(q) * inv;
            if v < 0.0 || u + v > 1.0 {
                continue;
            }
            let t = e2.dot(q) * inv;
            if t > 0.0 && best.as_ref().is_none_or(|h| t < h.t) {
                best = Some(Hit { triangle: tri, t, bary: [1.0 - u - v, u, v] });
            }
        }
        best
    }

    /// Texture coordinates at a hit.
    pub fn uv_at(&self, hit: &Hit) -> Vec2 {
        let idx = &self.indices[hit.triangle * 3..hit.triangle * 3 + 3];
        (0..3).map(|k| Vec2::from(self.vertices[idx[k] as usize].uv) * hit.bary[k]).sum()
    }

    /// Which material (texture set) draws a triangle.
    pub fn material_of(&self, triangle: usize) -> usize {
        let index = triangle as u32 * 3;
        self.submeshes
            .iter()
            .find(|s| (s.index_start..s.index_start + s.index_count).contains(&index))
            .map(|s| s.material)
            .unwrap_or(0)
    }

    /// An island number for every triangle: triangles joined by corners with the same position
    /// and UV belong to one UV island.
    pub fn uv_islands(&self) -> Vec<u32> {
        use std::collections::HashMap;
        let mut welded: HashMap<[u32; 5], u32> = HashMap::new();
        let corner: Vec<u32> = self
            .vertices
            .iter()
            .map(|v| {
                let key = [v.pos[0].to_bits(), v.pos[1].to_bits(), v.pos[2].to_bits(), v.uv[0].to_bits(), v.uv[1].to_bits()];
                let next = welded.len() as u32;
                *welded.entry(key).or_insert(next)
            })
            .collect();
        let mut parent: Vec<u32> = (0..welded.len() as u32).collect();
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                parent[x as usize] = parent[parent[x as usize] as usize];
                x = parent[x as usize];
            }
            x
        }
        for idx in self.indices.chunks_exact(3) {
            let a = find(&mut parent, corner[idx[0] as usize]);
            for &k in &idx[1..] {
                let b = find(&mut parent, corner[k as usize]);
                let a = find(&mut parent, a);
                if a != b {
                    parent[b as usize] = a;
                }
            }
        }
        self.indices.chunks_exact(3).map(|idx| find(&mut parent, corner[idx[0] as usize])).collect()
    }
}

/// Where a ray met the mesh.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub triangle: usize,
    pub t: f32,
    /// Weights of the triangle's three corners.
    pub bary: [f32; 3],
}

pub const SUPPORTED_EXTENSIONS: &[&str] = &["glb", "gltf", "obj"];

pub fn load(path: &Path) -> Result<Mesh> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let mut mesh = match ext.as_str() {
        "glb" | "gltf" => load_gltf(path),
        "obj" => load_obj(path),
        other => bail!("Umber can't open .{other} files yet. Supported: {}", SUPPORTED_EXTENSIONS.join(", ")),
    }
    .with_context(|| format!("Couldn't open {}", path.display()))?;
    if mesh.indices.is_empty() {
        bail!("{} contains no triangles", path.display());
    }
    mesh.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if mesh.materials.is_empty() {
        mesh.materials.push(Material::named("Material"));
    }
    compute_tangents(&mut mesh.vertices, &mesh.indices);
    Ok(mesh)
}

fn load_gltf(path: &Path) -> Result<Mesh> {
    let (doc, buffers, images) = gltf::import(path)?;
    let mut out = Mesh {
        name: String::new(),
        vertices: Vec::new(),
        indices: Vec::new(),
        submeshes: Vec::new(),
        has_uvs: true,
        materials: Vec::new(),
    };
    let mut any_missing_uvs = false;
    // glTF material index (None = the default material) -> index into out.materials.
    let mut material_slots: std::collections::HashMap<Option<usize>, usize> = Default::default();

    let scene = doc.default_scene().or_else(|| doc.scenes().next());
    let roots: Vec<gltf::Node> = match &scene {
        Some(s) => s.nodes().collect(),
        None => doc.nodes().collect(),
    };
    let mut stack: Vec<(gltf::Node, Mat4)> = roots.into_iter().map(|n| (n, Mat4::IDENTITY)).collect();

    while let Some((node, parent)) = stack.pop() {
        let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
        for child in node.children() {
            stack.push((child, world));
        }
        let Some(gmesh) = node.mesh() else { continue };
        let normal_mat = Mat3::from_mat4(world).inverse().transpose();
        let flips_winding = Mat3::from_mat4(world).determinant() < 0.0;

        for prim in gmesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                log::warn!("Skipping non-triangle primitive in {}", gmesh.name().unwrap_or("mesh"));
                continue;
            }
            let reader = prim.reader(|b| Some(&buffers[b.index()]));
            let Some(positions) = reader.read_positions() else { continue };
            let positions: Vec<Vec3> = positions.map(|p| world.transform_point3(Vec3::from(p))).collect();
            let normals: Option<Vec<Vec3>> =
                reader.read_normals().map(|n| n.map(|n| (normal_mat * Vec3::from(n)).normalize_or_zero()).collect());
            let uvs: Option<Vec<[f32; 2]>> = reader.read_tex_coords(0).map(|t| t.into_f32().collect());
            any_missing_uvs |= uvs.is_none();
            let mut idx: Vec<u32> = match reader.read_indices() {
                Some(i) => i.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            if flips_winding {
                for tri in idx.chunks_exact_mut(3) {
                    tri.swap(1, 2);
                }
            }

            let base = out.vertices.len() as u32;
            for (i, p) in positions.iter().enumerate() {
                out.vertices.push(Vertex {
                    pos: p.to_array(),
                    normal: normals.as_ref().map(|n| n[i].to_array()).unwrap_or_default(),
                    uv: uvs.as_ref().map(|u| u[i]).unwrap_or_default(),
                    tangent: [0.0; 4],
                });
            }
            if normals.is_none() {
                compute_normals(&mut out.vertices[base as usize..], &idx);
            }
            let material = prim.material();
            let slot = *material_slots.entry(material.index()).or_insert_with(|| {
                let pbr = material.pbr_metallic_roughness();
                let n = out.materials.len();
                out.materials.push(Material {
                    name: material.name().map(str::to_owned).unwrap_or_else(|| format!("Material {}", n + 1)),
                    base_color: pbr.base_color_factor(),
                    base_color_texture: pbr
                        .base_color_texture()
                        .and_then(|info| gltf_image_to_rgba(&images[info.texture().source().index()])),
                    roughness: pbr.roughness_factor(),
                    metallic: pbr.metallic_factor(),
                });
                n
            });
            out.submeshes.push(Submesh {
                material: slot,
                index_start: out.indices.len() as u32,
                index_count: idx.len() as u32,
            });
            out.indices.extend(idx.into_iter().map(|i| i + base));
        }
    }
    out.has_uvs = !any_missing_uvs && !out.vertices.is_empty();
    Ok(out)
}

fn gltf_image_to_rgba(data: &gltf::image::Data) -> Option<Image> {
    use gltf::image::Format;
    let px = &data.pixels;
    let rgba: Vec<u8> = match data.format {
        Format::R8G8B8A8 => px.clone(),
        Format::R8G8B8 => px.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        Format::R8G8 => px.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        Format::R8 => px.iter().flat_map(|&c| [c, c, c, 255]).collect(),
        other => {
            log::warn!("Base colour texture format {other:?} not supported yet; ignoring it");
            return None;
        }
    };
    Some(Image { width: data.width, height: data.height, rgba })
}

fn load_obj(path: &Path) -> Result<Mesh> {
    let opts = tobj::LoadOptions { triangulate: true, single_index: true, ..Default::default() };
    let (models, materials) = tobj::load_obj(path, &opts)?;
    let materials = materials.unwrap_or_default();
    let mut out = Mesh {
        name: String::new(),
        vertices: Vec::new(),
        indices: Vec::new(),
        submeshes: Vec::new(),
        has_uvs: true,
        materials: Vec::new(),
    };
    // OBJ material id (None = no material) -> index into out.materials.
    let mut material_slots: std::collections::HashMap<Option<usize>, usize> = Default::default();
    for model in &models {
        let m = &model.mesh;
        let n = m.positions.len() / 3;
        let has_normals = m.normals.len() == n * 3;
        let has_uvs = m.texcoords.len() == n * 2;
        out.has_uvs &= has_uvs;
        let base = out.vertices.len() as u32;
        for i in 0..n {
            out.vertices.push(Vertex {
                pos: [m.positions[i * 3], m.positions[i * 3 + 1], m.positions[i * 3 + 2]],
                normal: if has_normals { [m.normals[i * 3], m.normals[i * 3 + 1], m.normals[i * 3 + 2]] } else { [0.0; 3] },
                // OBJ puts v = 0 at the bottom; everything else in Umber uses top-left.
                uv: if has_uvs { [m.texcoords[i * 2], 1.0 - m.texcoords[i * 2 + 1]] } else { [0.0; 2] },
                tangent: [0.0; 4],
            });
        }
        if !has_normals {
            compute_normals(&mut out.vertices[base as usize..], &m.indices);
        }
        let slot = *material_slots.entry(m.material_id).or_insert_with(|| {
            let mat = m.material_id.and_then(|id| materials.get(id));
            let mut out_mat = Material::named(mat.map(|m| m.name.clone()).unwrap_or_else(|| model.name.clone()));
            if let Some(mat) = mat {
                if let Some(d) = mat.diffuse {
                    out_mat.base_color = [d[0], d[1], d[2], 1.0];
                }
                if let Some(tex) = &mat.diffuse_texture {
                    let tex_path = path.parent().unwrap_or(Path::new(".")).join(tex);
                    match image::open(&tex_path) {
                        Ok(img) => {
                            let img = img.to_rgba8();
                            out_mat.base_color = [1.0; 4];
                            out_mat.base_color_texture =
                                Some(Image { width: img.width(), height: img.height(), rgba: img.into_raw() });
                        }
                        Err(e) => log::warn!("Couldn't load texture {}: {e}", tex_path.display()),
                    }
                }
            }
            out.materials.push(out_mat);
            out.materials.len() - 1
        });
        out.submeshes.push(Submesh {
            material: slot,
            index_start: out.indices.len() as u32,
            index_count: m.indices.len() as u32,
        });
        out.indices.extend(m.indices.iter().map(|i| i + base));
    }
    Ok(out)
}

/// Area-weighted smooth normals. `indices` are local to `verts`.
fn compute_normals(verts: &mut [Vertex], indices: &[u32]) {
    let mut acc = vec![Vec3::ZERO; verts.len()];
    for tri in indices.chunks_exact(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (pa, pb, pc) = (Vec3::from(verts[a].pos), Vec3::from(verts[b].pos), Vec3::from(verts[c].pos));
        let n = (pb - pa).cross(pc - pa);
        acc[a] += n;
        acc[b] += n;
        acc[c] += n;
    }
    for (v, n) in verts.iter_mut().zip(acc) {
        v.normal = n.normalize_or(Vec3::Y).to_array();
    }
}

/// Per-vertex tangents from UV derivatives, accumulated per triangle and made orthogonal to
/// the normal. Close to MikkTSpace on ordinary meshes; export will want the real thing.
fn compute_tangents(verts: &mut [Vertex], indices: &[u32]) {
    let mut tan = vec![Vec3::ZERO; verts.len()];
    let mut bit = vec![Vec3::ZERO; verts.len()];
    for tri in indices.chunks_exact(3) {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (pa, pb, pc) = (Vec3::from(verts[a].pos), Vec3::from(verts[b].pos), Vec3::from(verts[c].pos));
        let (ta, tb, tc) = (Vec2::from(verts[a].uv), Vec2::from(verts[b].uv), Vec2::from(verts[c].uv));
        let (e1, e2) = (pb - pa, pc - pa);
        let (d1, d2) = (tb - ta, tc - ta);
        let det = d1.x * d2.y - d2.x * d1.y;
        if det.abs() < 1e-12 {
            continue;
        }
        let r = 1.0 / det;
        let t = (e1 * d2.y - e2 * d1.y) * r;
        let bt = (e2 * d1.x - e1 * d2.x) * r;
        for i in [a, b, c] {
            tan[i] += t;
            bit[i] += bt;
        }
    }
    for (i, v) in verts.iter_mut().enumerate() {
        let n = Vec3::from(v.normal);
        let t = tan[i] - n * n.dot(tan[i]);
        let t = if t.length_squared() > 1e-20 { t.normalize() } else { n.any_orthonormal_vector() };
        let w = if n.cross(t).dot(bit[i]) < 0.0 { -1.0 } else { 1.0 };
        v.tangent = [t.x, t.y, t.z, w];
    }
}

// ---- Built-in shapes for trying the app without a file ----

pub fn sphere(segments: u32, rings: u32) -> Mesh {
    let mut vertices = Vec::new();
    for r in 0..=rings {
        let v = r as f32 / rings as f32;
        let phi = v * std::f32::consts::PI;
        for s in 0..=segments {
            let u = s as f32 / segments as f32;
            let theta = u * std::f32::consts::TAU;
            let n = Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
            vertices.push(Vertex { pos: n.to_array(), normal: n.to_array(), uv: [u, v], tangent: [0.0; 4] });
        }
    }
    let mut indices = Vec::new();
    let row = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            let b = a + row;
            indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    builtin("Sphere", vertices, indices)
}

pub fn cube() -> Mesh {
    // Each face gets its own cell in a 3x2 atlas so the UV checker reads cleanly, inset by a
    // small gutter so filtering at one face's edge never reads the next face's texels.
    let faces: [(Vec3, Vec3, Vec3); 6] = [
        (Vec3::X, Vec3::NEG_Z, Vec3::Y),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y),
        (Vec3::Y, Vec3::X, Vec3::NEG_Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (i, (n, u, v)) in faces.into_iter().enumerate() {
        let cell = Vec2::new((i % 3) as f32 / 3.0, (i / 3) as f32 / 2.0);
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = n + u * su + v * sv;
            const GUTTER: f32 = 0.01;
            let (fu, fv) = ((su + 1.0) * 0.5, 1.0 - (sv + 1.0) * 0.5);
            let uv = cell + Vec2::new(GUTTER + fu * (1.0 / 3.0 - 2.0 * GUTTER), GUTTER + fv * (0.5 - 2.0 * GUTTER));
            vertices.push(Vertex { pos: (p * 0.8).to_array(), normal: n.to_array(), uv: uv.to_array(), tangent: [0.0; 4] });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    builtin("Cube", vertices, indices)
}

fn builtin(name: &str, mut vertices: Vec<Vertex>, indices: Vec<u32>) -> Mesh {
    compute_tangents(&mut vertices, &indices);
    let mut material = Material::named("Default");
    material.base_color = [0.78, 0.76, 0.72, 1.0];
    Mesh {
        name: name.to_owned(),
        submeshes: vec![Submesh { material: 0, index_start: 0, index_count: indices.len() as u32 }],
        vertices,
        indices,
        has_uvs: true,
        materials: vec![material],
    }
}
