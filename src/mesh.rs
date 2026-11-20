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

    /// A face number for every triangle, the way a person would see faces: neighbouring
    /// triangles (sharing an edge in space, whatever their UVs) are one face when they bend
    /// less than `edge_degrees` from each other. A shallow curve stays one face; a box corner
    /// starts another.
    pub fn faces(&self, edge_degrees: f32) -> Vec<u32> {
        use std::collections::HashMap;
        let mut welded: HashMap<[u32; 3], u32> = HashMap::new();
        let corner: Vec<u32> = self
            .vertices
            .iter()
            .map(|v| {
                let next = welded.len() as u32;
                *welded.entry(v.pos.map(f32::to_bits)).or_insert(next)
            })
            .collect();
        let tris = self.triangle_count();
        let normals: Vec<Vec3> = self
            .indices
            .chunks_exact(3)
            .map(|t| {
                let p = |i: u32| Vec3::from(self.vertices[i as usize].pos);
                (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0])).normalize_or_zero()
            })
            .collect();
        let mut by_edge: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
        for (t, idx) in self.indices.chunks_exact(3).enumerate() {
            for k in 0..3 {
                let (a, b) = (corner[idx[k] as usize], corner[idx[(k + 1) % 3] as usize]);
                if a != b {
                    by_edge.entry((a.min(b), a.max(b))).or_default().push(t as u32);
                }
            }
        }
        let mut parent: Vec<u32> = (0..tris as u32).collect();
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                parent[x as usize] = parent[parent[x as usize] as usize];
                x = parent[x as usize];
            }
            x
        }
        let min_dot = edge_degrees.to_radians().cos();
        for list in by_edge.values() {
            for (i, &a) in list.iter().enumerate() {
                for &b in &list[i + 1..] {
                    let (na, nb) = (normals[a as usize], normals[b as usize]);
                    // A sliver with no direction joins whatever it touches.
                    let joined = na == Vec3::ZERO || nb == Vec3::ZERO || na.dot(nb) >= min_dot;
                    if joined {
                        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                        if ra != rb {
                            parent[rb as usize] = ra;
                        }
                    }
                }
            }
        }
        (0..tris as u32).map(|t| find(&mut parent, t)).collect()
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

pub const SUPPORTED_EXTENSIONS: &[&str] = &["glb", "gltf", "obj", "fbx"];

pub fn load(path: &Path) -> Result<Mesh> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let mut mesh = match ext.as_str() {
        "glb" | "gltf" => load_gltf(path),
        "obj" => load_obj(path),
        "fbx" => load_fbx(path),
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

fn load_fbx(path: &Path) -> Result<Mesh> {
    let opts = ufbx::LoadOpts {
        // Y up, metres, geometry baked into the target space: what glTF and Godot use.
        target_axes: ufbx::CoordinateAxes::right_handed_y_up(),
        target_unit_meters: 1.0,
        space_conversion: ufbx::SpaceConversion::ModifyGeometry,
        generate_missing_normals: true,
        // Blender writes its PBR materials as Phong in a fixed way; this reads them back as PBR.
        use_blender_pbr_material: true,
        ignore_animation: true,
        load_external_files: false,
        ..Default::default()
    };
    let scene = ufbx::load_file(&path.to_string_lossy(), opts).map_err(|e| anyhow::anyhow!("{}", e.description.as_ref() as &str))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut out = Mesh {
        name: String::new(),
        vertices: Vec::new(),
        indices: Vec::new(),
        submeshes: Vec::new(),
        has_uvs: true,
        materials: Vec::new(),
    };
    // FBX material element id (None = no material) -> index into out.materials.
    let mut material_slots: std::collections::HashMap<Option<u32>, usize> = Default::default();
    let mut tri = Vec::new();

    for node in scene.nodes.iter() {
        let Some(fmesh) = &node.mesh else { continue };
        if !node.visible {
            continue;
        }
        let m = &node.geometry_to_world;
        let world = Mat4::from_cols_array(&[
            m.m00 as f32, m.m10 as f32, m.m20 as f32, 0.0,
            m.m01 as f32, m.m11 as f32, m.m21 as f32, 0.0,
            m.m02 as f32, m.m12 as f32, m.m22 as f32, 0.0,
            m.m03 as f32, m.m13 as f32, m.m23 as f32, 1.0,
        ]);
        let normal_mat = Mat3::from_mat4(world).inverse().transpose();
        let flips_winding = Mat3::from_mat4(world).determinant() < 0.0;
        let has_uvs = fmesh.vertex_uv.exists;
        out.has_uvs &= has_uvs;

        for part in fmesh.material_parts.iter() {
            if part.num_triangles == 0 {
                continue;
            }
            // Nodes can override the mesh's materials, as instances do.
            let material = node.materials.get(part.index as usize).or_else(|| fmesh.materials.get(part.index as usize));
            let slot = *material_slots.entry(material.map(|m| m.element.element_id)).or_insert_with(|| {
                out.materials.push(match material {
                    Some(m) => fbx_material(m, dir),
                    None => Material::named(node.element.name.to_string()),
                });
                out.materials.len() - 1
            });

            // Corners welded by position, normal and UV, so the mesh shares vertices like any other.
            let mut welded: std::collections::HashMap<[u32; 8], u32> = Default::default();
            let base = out.vertices.len();
            let mut idx = Vec::with_capacity(part.num_triangles * 3);
            for &face in part.face_indices.iter() {
                let face = fmesh.faces[face as usize];
                let n = ufbx::triangulate_face_vec(&mut tri, fmesh, face);
                for t in 0..n as usize {
                    let corners = [tri[t * 3], tri[t * 3 + 1], tri[t * 3 + 2]];
                    let corners = if flips_winding { [corners[0], corners[2], corners[1]] } else { corners };
                    for c in corners {
                        let c = c as usize;
                        let p = fmesh.vertex_position[c];
                        let pos = world.transform_point3(Vec3::new(p.x as f32, p.y as f32, p.z as f32));
                        let normal = if fmesh.vertex_normal.exists {
                            let n = fmesh.vertex_normal[c];
                            (normal_mat * Vec3::new(n.x as f32, n.y as f32, n.z as f32)).normalize_or_zero()
                        } else {
                            Vec3::ZERO
                        };
                        // FBX puts v = 0 at the bottom; Umber uses top-left.
                        let uv = if has_uvs {
                            let t = fmesh.vertex_uv[c];
                            [t.x as f32, 1.0 - t.y as f32]
                        } else {
                            [0.0; 2]
                        };
                        let v = Vertex { pos: pos.to_array(), normal: normal.to_array(), uv, tangent: [0.0; 4] };
                        let key = [v.pos[0], v.pos[1], v.pos[2], v.normal[0], v.normal[1], v.normal[2], uv[0], uv[1]].map(f32::to_bits);
                        let next = (out.vertices.len() - base) as u32;
                        let local = *welded.entry(key).or_insert_with(|| {
                            out.vertices.push(v);
                            next
                        });
                        idx.push(local);
                    }
                }
            }
            if !fmesh.vertex_normal.exists {
                compute_normals(&mut out.vertices[base..], &idx);
            }
            out.submeshes.push(Submesh { material: slot, index_start: out.indices.len() as u32, index_count: idx.len() as u32 });
            out.indices.extend(idx.into_iter().map(|i| i + base as u32));
        }
    }
    out.has_uvs &= !out.vertices.is_empty();
    Ok(out)
}

/// An FBX material's base colour, roughness, metalness and base colour texture, from its PBR
/// maps where the exporter wrote them and the classic diffuse ones where it didn't.
fn fbx_material(m: &ufbx::Material, dir: &Path) -> Material {
    let mut out = Material::named(m.element.name.to_string());
    let (pbr, fbx) = (&m.pbr, &m.fbx);
    let colour = if pbr.base_color.has_value { &pbr.base_color } else { &fbx.diffuse_color };
    if colour.has_value {
        let c = colour.value_vec4;
        let factor = if pbr.base_factor.has_value { pbr.base_factor.value_vec4.x } else { 1.0 };
        out.base_color = [(c.x * factor) as f32, (c.y * factor) as f32, (c.z * factor) as f32, 1.0];
    }
    // Plain Phong materials have no real roughness or metalness; guessing one from shininess
    // tends to come out as a mirror, so those keep Umber's defaults.
    if m.features.pbr.enabled {
        if pbr.roughness.has_value {
            out.roughness = pbr.roughness.value_vec4.x as f32;
        }
        if pbr.metalness.has_value {
            out.metallic = pbr.metalness.value_vec4.x as f32;
        }
    }
    let texture = pbr.base_color.texture.as_ref().or(fbx.diffuse_color.texture.as_ref());
    if let Some(img) = texture.and_then(|tex| fbx_texture(tex, dir)) {
        out.base_color = [1.0; 4];
        out.base_color_texture = Some(img);
    }
    out
}

/// A texture's pixels: embedded in the file, at the path it names, or beside the model with
/// the same file name (how most files arrive once they've been moved between machines).
fn fbx_texture(tex: &ufbx::Texture, dir: &Path) -> Option<Image> {
    let decoded = if !tex.content.is_empty() {
        image::load_from_memory(&tex.content).ok()
    } else {
        let names = [tex.absolute_filename.to_string(), tex.relative_filename.to_string(), tex.filename.to_string()];
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for n in names.iter().filter(|n| !n.is_empty()) {
            let p = Path::new(n);
            candidates.push(if p.is_absolute() { p.to_owned() } else { dir.join(p) });
            // Windows paths keep their backslashes on Linux; the last part is the file name.
            if let Some(file) = n.rsplit(['/', '\\']).next() {
                candidates.push(dir.join(file));
                candidates.push(dir.join("textures").join(file));
            }
        }
        candidates.into_iter().find_map(|p| image::open(&p).ok())
    };
    match decoded {
        Some(img) => {
            let img = img.to_rgba8();
            Some(Image { width: img.width(), height: img.height(), rgba: img.into_raw() })
        }
        None => {
            log::warn!("Couldn't find the texture {}", tex.filename.as_ref() as &str);
            None
        }
    }
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
pub(crate) fn compute_tangents(verts: &mut [Vertex], indices: &[u32]) {
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

#[cfg(test)]
mod tests {

    #[test]
    fn faces_split_at_corners_and_follow_shallow_curves() {
        let cube = super::cube();
        let faces = cube.faces(40.0);
        let mut distinct = faces.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 6, "a cube has six faces");
        // A strip bent 10 degrees at each of five folds is one face; at 60 it would be three.
        let mut verts = Vec::new();
        let mut idx = Vec::new();
        let (mut x, mut z, mut angle) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..6 {
            for y in [0.0, 1.0] {
                verts.push(Vertex { pos: [x, y, z], normal: [0.0, 0.0, 1.0], uv: [i as f32 / 5.0, y], tangent: [1.0, 0.0, 0.0, 1.0] });
            }
            x += angle.to_radians().cos();
            z -= angle.to_radians().sin();
            angle += 10.0;
            if i > 0 {
                let b = (i as u32 - 1) * 2;
                idx.extend_from_slice(&[b, b + 2, b + 3, b, b + 3, b + 1]);
            }
        }
        let n = idx.len() as u32;
        let strip = Mesh { name: "strip".into(), vertices: verts, indices: idx, submeshes: vec![Submesh { index_start: 0, index_count: n, material: 0 }], has_uvs: true, materials: vec![Material::named("m")] };
        let f = strip.faces(40.0);
        assert!(f.iter().all(|&v| v == f[0]), "a shallow curve is one face");
        assert!(strip.faces(5.0).iter().any(|&v| v != f[0]), "a tight edge angle splits it");
    }
    use super::*;

    /// A Z-up quad in metres with one material, as Blender or Maya might write it in ASCII.
    const QUAD_FBX: &str = r#"; FBX 7.4.0 project file
FBXHeaderExtension:  {
	FBXHeaderVersion: 1003
	FBXVersion: 7400
}
GlobalSettings:  {
	Version: 1000
	Properties70:  {
		P: "UpAxis", "int", "Integer", "",2
		P: "UpAxisSign", "int", "Integer", "",1
		P: "FrontAxis", "int", "Integer", "",1
		P: "FrontAxisSign", "int", "Integer", "",-1
		P: "CoordAxis", "int", "Integer", "",0
		P: "CoordAxisSign", "int", "Integer", "",1
		P: "UnitScaleFactor", "double", "Number", "",100
	}
}
Objects:  {
	Geometry: 1000, "Geometry::Quad", "Mesh" {
		Vertices: *12 {
			a: 0,0,0,1,0,0,1,1,0,0,1,0
		}
		PolygonVertexIndex: *4 {
			a: 0,1,2,-4
		}
		LayerElementUV: 0 {
			Version: 101
			Name: "UVMap"
			MappingInformationType: "ByPolygonVertex"
			ReferenceInformationType: "IndexToDirect"
			UV: *8 {
				a: 0,0,1,0,1,1,0,1
			}
			UVIndex: *4 {
				a: 0,1,2,3
			}
		}
		LayerElementMaterial: 0 {
			Version: 101
			MappingInformationType: "AllSame"
			ReferenceInformationType: "IndexToDirect"
			Materials: *1 {
				a: 0
			}
		}
		Layer: 0 {
			Version: 100
			LayerElement:  {
				Type: "LayerElementUV"
				TypedIndex: 0
			}
			LayerElement:  {
				Type: "LayerElementMaterial"
				TypedIndex: 0
			}
		}
	}
	Model: 2000, "Model::Quad", "Mesh" {
		Version: 232
	}
	Material: 3000, "Material::Brass", "" {
		Version: 102
		ShadingModel: "phong"
		Properties70:  {
			P: "DiffuseColor", "Color", "", "A",0.8,0.6,0.2
		}
	}
}
Connections:  {
	C: "OO",2000,0
	C: "OO",1000,2000
	C: "OO",3000,2000
}
"#;

    #[test]
    fn fbx_quad_comes_in_y_up_with_its_material() {
        let path = std::env::temp_dir().join(format!("umber-quad-{}.fbx", std::process::id()));
        std::fs::write(&path, QUAD_FBX).unwrap();
        let mesh = load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(mesh.triangle_count(), 2);
        assert!(mesh.has_uvs);
        assert_eq!(mesh.materials.len(), 1);
        assert_eq!(mesh.materials[0].name, "Brass");
        assert!((mesh.materials[0].base_color[0] - 0.8).abs() < 1e-4);
        // Z up became Y up: the quad lies flat, a metre across, facing up.
        let (lo, hi) = mesh.bounds();
        assert!(lo.y.abs() < 1e-5 && hi.y.abs() < 1e-5, "{lo} {hi}");
        assert!(((hi - lo).x - 1.0).abs() < 1e-4 && ((hi - lo).z - 1.0).abs() < 1e-4, "{lo} {hi}");
        assert!(mesh.vertices.iter().all(|v| v.normal[1] > 0.99));
        // Welded: four corners, not six.
        assert_eq!(mesh.vertices.len(), 4);
        // The FBX corner at uv (0, 0) is at the bottom of the image, which is v = 1 here.
        assert!(mesh.vertices.iter().any(|v| v.uv == [0.0, 1.0]));
    }

    /// Opens every model in $UMBER_TEST_MODELS (skipped when it isn't set).
    #[test]
    fn opens_every_model_in_a_folder() {
        let Some(dir) = std::env::var_os("UMBER_TEST_MODELS") else { return };
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let t = std::time::Instant::now();
            match load(&path) {
                Ok(m) => {
                    let (lo, hi) = m.bounds();
                    let textured = m.materials.iter().filter(|m| m.base_color_texture.is_some()).count();
                    let rm: Vec<String> = m.materials.iter().map(|m| format!("{:.2}/{:.2}", m.roughness, m.metallic)).collect();
                    println!("  rough/metal {}", rm.join(" "));
                    println!(
                        "{}: {} tris, {} materials ({textured} textured), uvs {}, size {:.2?}, {:?}",
                        m.name, m.triangle_count(), m.materials.len(), m.has_uvs, (hi - lo).to_array(), t.elapsed()
                    );
                }
                Err(e) => println!("{}: FAILED {e:#}", path.display()),
            }
        }
    }
}
