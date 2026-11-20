//! Export to Godot. One folder gets a `.glb` with the painted maps inside, and beside it, per
//! texture set, the same maps as PNGs and an `ORMMaterial3D` `.tres` that points at them.
//!
//! The GLB names its images so that Godot's importer, which extracts embedded images to
//! `<glb stem>_<image name>.png`, lands on the PNGs already there and uses them instead of
//! writing second copies. So the model and the `.tres` share one set of textures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde_json::json;

use crate::mesh::Mesh;
use crate::texset::{self, TextureSet};
use eframe::wgpu;

/// Texture sizes on offer, largest first. Smaller ones come from the maps' own mips.
pub const SIZES: [u32; 3] = [2048, 1024, 512];

/// One texture set's maps, read back from the GPU.
pub struct SetMaps {
    pub size: u32,
    /// RGB, sRGB-encoded.
    pub albedo: Vec<u8>,
    /// R occlusion, G roughness, B metalness: the order glTF and Godot's ORM textures use.
    pub orm: Vec<u8>,
    /// RGB tangent-space normal, +Y up (the OpenGL convention glTF and Godot both use).
    pub normal: Vec<u8>,
    /// False when nothing is occluded, so materials leave AO off.
    pub uses_ao: bool,
}

/// Read a texture set's composited maps back at `size` (one of [`SIZES`]).
pub fn read_maps(device: &wgpu::Device, queue: &wgpu::Queue, set: &TextureSet, size: u32) -> SetMaps {
    let level = (texset::SIZE / size.clamp(1, texset::SIZE)).trailing_zeros();
    let [color, mat, normal] = set.output_textures();
    let (size, color) = texset::read_level(device, queue, color, level, 4);
    let (_, mat) = texset::read_level(device, queue, mat, level, 8);
    let (_, normal) = texset::read_level(device, queue, normal, level, 4);
    let unit = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut uses_ao = false;
    let orm = mat
        .chunks_exact(8)
        .flat_map(|m| {
            let rough = texset::f16_to_f32(m[0], m[1]);
            let metal = texset::f16_to_f32(m[2], m[3]);
            let ao = texset::f16_to_f32(m[6], m[7]);
            uses_ao |= ao < 0.99;
            [unit(ao), unit(rough), unit(metal)]
        })
        .collect();
    SetMaps {
        size,
        albedo: color.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        orm,
        normal: normal.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        uses_ao,
    }
}

/// A Godot project the export folder sits inside.
#[derive(Clone, Debug)]
pub struct GodotProject {
    pub root: PathBuf,
    pub name: String,
}

/// The nearest `project.godot` at or above `dir`.
pub fn godot_project(dir: &Path) -> Option<GodotProject> {
    let root = dir.ancestors().find(|d| d.join("project.godot").is_file())?;
    let text = std::fs::read_to_string(root.join("project.godot")).unwrap_or_default();
    let name = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("config/name=").map(|v| v.trim().trim_matches('"').to_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    Some(GodotProject { root: root.to_owned(), name })
}

/// The files written for one material.
#[derive(Clone, Debug)]
pub struct MaterialFiles {
    /// The material's own name, kept as Godot's resource name.
    pub name: String,
    /// What the GLB calls its images: `<glb stem>_<image>.png` is the PNG beside it.
    pub images: [String; 3],
    /// Albedo, ORM, normal PNGs.
    pub maps: [String; 3],
    pub tres: String,
}

/// Where everything goes and what it is called, worked out before anything is written so the
/// Export panel can show it.
#[derive(Clone, Debug)]
pub struct Plan {
    pub dir: PathBuf,
    pub glb: String,
    pub materials: Vec<MaterialFiles>,
    pub godot: Option<GodotProject>,
}

const MAP_SUFFIXES: [&str; 3] = ["albedo", "orm", "normal"];

impl Plan {
    pub fn new(dir: &Path, model_name: &str, material_names: &[&str]) -> Plan {
        let stem = Path::new(model_name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let stem = file_safe(&stem, "model");
        // One material needs no name in its files; several get theirs, kept unique.
        let mut taken = std::collections::HashSet::new();
        let materials = material_names
            .iter()
            .enumerate()
            .map(|(i, &name)| {
                let tag = if material_names.len() == 1 {
                    String::new()
                } else {
                    let base = file_safe(name, &format!("material_{}", i + 1));
                    let mut tag = base.clone();
                    let mut n = 2;
                    while !taken.insert(tag.clone()) {
                        tag = format!("{base}_{n}");
                        n += 1;
                    }
                    tag
                };
                let joined = |suffix: &str| if tag.is_empty() { suffix.to_owned() } else { format!("{tag}_{suffix}") };
                MaterialFiles {
                    name: name.to_owned(),
                    images: MAP_SUFFIXES.map(joined),
                    maps: MAP_SUFFIXES.map(|s| format!("{stem}_{}.png", joined(s))),
                    tres: if tag.is_empty() { format!("{stem}.tres") } else { format!("{stem}_{tag}.tres") },
                }
            })
            .collect();
        Plan { dir: dir.to_owned(), glb: format!("{stem}.glb"), materials, godot: godot_project(dir) }
    }

    /// Every file name, the model first.
    pub fn files(&self) -> Vec<&str> {
        let mut out = vec![self.glb.as_str()];
        for m in &self.materials {
            out.push(&m.tres);
            out.extend(m.maps.iter().map(String::as_str));
        }
        out
    }

    /// How a `.tres` names a file beside it: `res://` inside a Godot project, else a bare name
    /// (Godot reads paths in `.tres` files relative to the file).
    fn resource_path(&self, file: &str) -> String {
        match self.res_dir() {
            Some(dir) if dir.ends_with('/') => format!("{dir}{file}"),
            Some(dir) => format!("{dir}/{file}"),
            None => file.to_owned(),
        }
    }

    /// The export folder as Godot sees it, when it's inside a project.
    pub fn res_dir(&self) -> Option<String> {
        let rel = self.dir.strip_prefix(&self.godot.as_ref()?.root).ok()?;
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        Some(format!("res://{}", parts.join("/")))
    }
}

/// Lowercase letters, digits and underscores, so names survive every filesystem and engine.
fn file_safe(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_owned();
    if out.is_empty() { fallback.to_owned() } else { out }
}

/// Steps done so far, for a progress bar: one per map encoded, one for the model.
pub fn total_steps(plan: &Plan) -> u32 {
    plan.materials.len() as u32 * 3 + 1
}

/// Encode and write everything in the plan. `maps` runs parallel to `plan.materials` (and to
/// the mesh's materials). Each file is written beside its final name and renamed into place,
/// so Godot, watching the folder, never imports half a file.
pub fn write(plan: &Plan, mesh: &Mesh, maps: &[SetMaps], done: &AtomicU32) -> Result<()> {
    std::fs::create_dir_all(&plan.dir).with_context(|| format!("Couldn't create {}", plan.dir.display()))?;

    // Encode every PNG at once; this is most of the time an export takes.
    let jobs: Vec<(&[u8], u32)> =
        maps.iter().flat_map(|m| [(&m.albedo[..], m.size), (&m.orm[..], m.size), (&m.normal[..], m.size)]).collect();
    let pngs: Vec<Result<Vec<u8>>> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|&(rgb, size)| {
                s.spawn(move || {
                    let png = encode_png(rgb, size);
                    done.fetch_add(1, Ordering::Relaxed);
                    png
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("PNG encoder panicked")).collect()
    });
    let pngs = pngs.into_iter().collect::<Result<Vec<_>>>()?;

    for (i, files) in plan.materials.iter().enumerate() {
        for (k, name) in files.maps.iter().enumerate() {
            write_file(&plan.dir.join(name), &pngs[i * 3 + k])?;
        }
        let tres = godot_material(plan, files, maps[i].uses_ao);
        write_file(&plan.dir.join(&files.tres), tres.as_bytes())?;
    }
    let glb = glb(plan, mesh, maps, &pngs)?;
    write_file(&plan.dir.join(&plan.glb), &glb)?;
    done.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn encode_png(rgb: &[u8], size: u32) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(rgb, size, size, image::ExtendedColorType::Rgb8)?;
    Ok(out)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.umber-tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("Couldn't write {}", path.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("Couldn't write {}", path.display()))?;
    Ok(())
}

/// An `ORMMaterial3D`: Godot's StandardMaterial3D that reads occlusion, roughness and
/// metalness from one packed texture.
fn godot_material(plan: &Plan, files: &MaterialFiles, uses_ao: bool) -> String {
    let quoted = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::from("[gd_resource type=\"ORMMaterial3D\" load_steps=4 format=3]\n\n");
    for (k, map) in files.maps.iter().enumerate() {
        out += &format!(
            "[ext_resource type=\"Texture2D\" path=\"{}\" id=\"{}_{}\"]\n",
            quoted(&plan.resource_path(map)),
            k + 1,
            MAP_SUFFIXES[k]
        );
    }
    out += "\n[resource]\n";
    out += &format!("resource_name = \"{}\"\n", quoted(&files.name));
    out += "albedo_texture = ExtResource(\"1_albedo\")\n";
    out += "orm_texture = ExtResource(\"2_orm\")\n";
    if uses_ao {
        out += "ao_enabled = true\n";
    }
    out += "normal_enabled = true\n";
    out += "normal_texture = ExtResource(\"3_normal\")\n";
    out
}

/// A binary glTF of the whole model: one mesh, one primitive per material, its maps embedded.
/// No tangents: glTF readers (Godot included) generate MikkTSpace ones, which is what the spec
/// asks for when a file has none.
fn glb(plan: &Plan, mesh: &Mesh, maps: &[SetMaps], pngs: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut push_view = |bin: &mut Vec<u8>, bytes: &[u8], target: Option<u32>| -> usize {
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut view = json!({ "buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len() });
        if let Some(t) = target {
            view["target"] = json!(t);
        }
        bin.extend_from_slice(bytes);
        views.push(view);
        views.len() - 1
    };
    const ARRAY_BUFFER: u32 = 34962;
    const ELEMENT_ARRAY_BUFFER: u32 = 34963;
    const FLOAT: u32 = 5126;
    const UNSIGNED_INT: u32 = 5125;

    let mut primitives = Vec::new();
    for material in 0..plan.materials.len() {
        // This material's triangles, with their vertices gathered into a buffer of their own.
        let mut remap = std::collections::HashMap::new();
        let mut verts = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        for sub in mesh.submeshes.iter().filter(|s| s.material == material) {
            let range = sub.index_start as usize..(sub.index_start + sub.index_count) as usize;
            for &i in &mesh.indices[range] {
                let local = *remap.entry(i).or_insert_with(|| {
                    verts.push(mesh.vertices[i as usize]);
                    verts.len() as u32 - 1
                });
                indices.push(local);
            }
        }
        if indices.is_empty() {
            continue;
        }
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for v in &verts {
            for k in 0..3 {
                lo[k] = lo[k].min(v.pos[k]);
                hi[k] = hi[k].max(v.pos[k]);
            }
        }
        let positions: Vec<[f32; 3]> = verts.iter().map(|v| v.pos).collect();
        let normals: Vec<[f32; 3]> =
            verts.iter().map(|v| glam::Vec3::from(v.normal).normalize_or(glam::Vec3::Y).to_array()).collect();
        let uvs: Vec<[f32; 2]> = verts.iter().map(|v| v.uv).collect();
        let n = verts.len();

        let mut attr = |bin: &mut Vec<u8>, bytes: &[u8], kind: &str, extra: Option<(serde_json::Value, serde_json::Value)>| {
            let view = push_view(bin, bytes, Some(ARRAY_BUFFER));
            let mut a = json!({ "bufferView": view, "componentType": FLOAT, "count": n, "type": kind });
            if let Some((min, max)) = extra {
                a["min"] = min;
                a["max"] = max;
            }
            accessors.push(a);
            accessors.len() - 1
        };
        let pos = attr(&mut bin, bytemuck::cast_slice(&positions), "VEC3", Some((json!(lo), json!(hi))));
        let nrm = attr(&mut bin, bytemuck::cast_slice(&normals), "VEC3", None);
        let uv = attr(&mut bin, bytemuck::cast_slice(&uvs), "VEC2", None);
        let view = push_view(&mut bin, bytemuck::cast_slice(&indices), Some(ELEMENT_ARRAY_BUFFER));
        accessors.push(json!({ "bufferView": view, "componentType": UNSIGNED_INT, "count": indices.len(), "type": "SCALAR" }));
        primitives.push(json!({
            "attributes": { "POSITION": pos, "NORMAL": nrm, "TEXCOORD_0": uv },
            "indices": accessors.len() - 1,
            "material": material,
        }));
    }

    let mut images = Vec::new();
    for (i, files) in plan.materials.iter().enumerate() {
        for k in 0..3 {
            let view = push_view(&mut bin, &pngs[i * 3 + k], None);
            images.push(json!({ "name": files.images[k], "bufferView": view, "mimeType": "image/png" }));
        }
    }
    let textures: Vec<_> = (0..images.len()).map(|i| json!({ "sampler": 0, "source": i })).collect();
    let materials: Vec<_> = plan
        .materials
        .iter()
        .enumerate()
        .map(|(i, files)| {
            let t = i * 3;
            let mut m = json!({
                "name": files.name,
                "pbrMetallicRoughness": {
                    "baseColorTexture": { "index": t },
                    "metallicRoughnessTexture": { "index": t + 1 },
                },
                "normalTexture": { "index": t + 2 },
            });
            if maps[i].uses_ao {
                m["occlusionTexture"] = json!({ "index": t + 1 });
            }
            m
        })
        .collect();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let name = plan.glb.trim_end_matches(".glb");
    let doc = json!({
        "asset": { "version": "2.0", "generator": concat!("Umber ", env!("CARGO_PKG_VERSION")) },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "name": name, "mesh": 0 }],
        "meshes": [{ "name": name, "primitives": primitives }],
        "materials": materials,
        "textures": textures,
        "images": images,
        // Linear, trilinear mips, repeat.
        "samplers": [{ "magFilter": 9729, "minFilter": 9987, "wrapS": 10497, "wrapT": 10497 }],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{ "byteLength": bin.len() }],
    });
    let mut json = serde_json::to_vec(&doc)?;
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }

    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_maps(size: u32, albedo: [u8; 3], rough: u8, metal: u8) -> SetMaps {
        let n = (size * size) as usize;
        SetMaps {
            size,
            albedo: albedo.repeat(n),
            orm: [255, rough, metal].repeat(n),
            normal: [128, 128, 255].repeat(n),
            uses_ao: false,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("umber-export-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn names_are_safe_and_unique() {
        let plan = Plan::new(Path::new("/x"), "My Crate (v2).glb", &["Wood", "wood", "Métal!"]);
        assert_eq!(plan.glb, "my_crate_v2.glb");
        assert_eq!(plan.materials[0].tres, "my_crate_v2_wood.tres");
        assert_eq!(plan.materials[1].tres, "my_crate_v2_wood_2.tres");
        assert_eq!(plan.materials[2].maps[1], "my_crate_v2_m_tal_orm.png");
        // Godot extracts each GLB image to <glb stem>_<image>.png; that must be the PNG beside it.
        for m in &plan.materials {
            for k in 0..3 {
                assert_eq!(format!("my_crate_v2_{}.png", m.images[k]), m.maps[k]);
            }
        }
        let single = Plan::new(Path::new("/x"), "crate.obj", &["Anything"]);
        assert_eq!(single.files(), ["crate.glb", "crate.tres", "crate_albedo.png", "crate_orm.png", "crate_normal.png"]);
    }

    #[test]
    fn godot_paths_inside_a_project() {
        let root = scratch("godot");
        let dir = root.join("models").join("props");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(root.join("project.godot"), "[application]\n\nconfig/name=\"Sewer Shift\"\n").unwrap();
        let plan = Plan::new(&dir, "crate.glb", &["Wood"]);
        assert_eq!(plan.godot.as_ref().unwrap().name, "Sewer Shift");
        assert_eq!(plan.res_dir().as_deref(), Some("res://models/props"));
        let tres = godot_material(&plan, &plan.materials[0], false);
        assert!(tres.contains("path=\"res://models/props/crate_albedo.png\""), "{tres}");
        assert!(!tres.contains("ao_enabled"));

        let at_root = Plan::new(&root, "crate.glb", &["Wood"]);
        assert_eq!(at_root.res_dir().as_deref(), Some("res://"));
        assert!(godot_material(&at_root, &at_root.materials[0], true).contains("path=\"res://crate_orm.png\""));

        let outside = Plan::new(&std::env::temp_dir(), "crate.glb", &["Wood"]);
        if outside.godot.is_none() {
            assert!(godot_material(&outside, &outside.materials[0], false).contains("path=\"crate_normal.png\""));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The maps read back from a real texture set, at full size and from a mip.
    #[test]
    fn reads_maps_back_from_the_gpu() {
        let (device, queue) = crate::texset::tests::gpu();
        let mut compositor = crate::texset::Compositor::new(&device, &queue);
        let mut material = crate::mesh::Material::named("Copper");
        material.base_color = [0.5, 0.25, 0.1, 1.0];
        material.roughness = 0.3;
        material.metallic = 1.0;
        let mut set = compositor.new_set(&device, &queue, &material);
        compositor.update(&device, &queue, &mut set);
        for size in [2048, 1024] {
            let maps = read_maps(&device, &queue, &set, size);
            assert_eq!(maps.size, size);
            assert_eq!(maps.albedo.len(), (size * size * 3) as usize);
            let mid = ((size / 2 * size + size / 2) * 3) as usize;
            let want = [0.5f32, 0.25, 0.1].map(texset::linear_to_srgb8);
            for k in 0..3 {
                assert!(maps.albedo[mid + k].abs_diff(want[k]) <= 2, "{size}: albedo {:?} vs {want:?}", &maps.albedo[mid..mid + 3]);
            }
            assert_eq!(maps.orm[mid], 255, "nothing occluded");
            assert!(maps.orm[mid + 1].abs_diff(77) <= 1, "roughness {}", maps.orm[mid + 1]);
            assert_eq!(maps.orm[mid + 2], 255, "metal");
            // 0.5 lands on 127 or 128 depending on the GPU's rounding; both read as flat.
            let n = &maps.normal[mid..mid + 3];
            assert!(n[0].abs_diff(128) <= 1 && n[1].abs_diff(128) <= 1 && n[2] == 255, "flat {n:?}");
            assert!(!maps.uses_ao);
        }
    }

    /// Write a two-material cube and read it back with Umber's own glTF importer.
    #[test]
    fn glb_round_trips() {
        let mut cube = crate::mesh::cube();
        // Second material on the last two faces.
        let split = cube.indices.len() as u32 - 12;
        cube.submeshes = vec![
            crate::mesh::Submesh { material: 0, index_start: 0, index_count: split },
            crate::mesh::Submesh { material: 1, index_start: split, index_count: 12 },
        ];
        cube.materials.push(crate::mesh::Material::named("Brass"));
        let dir = scratch("glb");
        let plan = Plan::new(&dir, "Cube", &["Default", "Brass"]);
        let maps = [flat_maps(8, [200, 100, 50], 64, 0), flat_maps(8, [30, 60, 90], 32, 255)];
        let done = AtomicU32::new(0);
        write(&plan, &cube, &maps, &done).unwrap();
        assert_eq!(done.load(Ordering::Relaxed), total_steps(&plan));
        for f in plan.files() {
            assert!(dir.join(f).is_file(), "{f} missing");
        }
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with("umber-tmp")));

        let back = crate::mesh::load(&dir.join("cube.glb")).unwrap();
        assert_eq!(back.triangle_count(), cube.triangle_count());
        assert_eq!(back.materials.len(), 2);
        assert_eq!(back.materials[1].name, "Brass");
        let tex = back.materials[0].base_color_texture.as_ref().unwrap();
        assert_eq!((tex.width, &tex.rgba[..4]), (8, &[200, 100, 50, 255][..]));
        let (lo, hi) = back.bounds();
        assert!((lo - glam::Vec3::splat(-0.8)).length() < 1e-5 && (hi - glam::Vec3::splat(0.8)).length() < 1e-5);
        // UVs keep the top-left origin both sides use.
        let uv_sum = |m: &Mesh| m.vertices.iter().map(|v| v.uv[0] + 3.0 * v.uv[1]).sum::<f32>();
        assert!((uv_sum(&back) - uv_sum(&cube)).abs() < 1e-3);

        let (doc, _, _) = gltf::import(dir.join("cube.glb")).unwrap();
        let brass = doc.materials().nth(1).unwrap();
        assert!(brass.pbr_metallic_roughness().metallic_roughness_texture().is_some());
        assert!(brass.normal_texture().is_some() && brass.occlusion_texture().is_none());
        let names: Vec<_> = doc.images().map(|i| i.name().unwrap().to_owned()).collect();
        assert_eq!(names, ["default_albedo", "default_orm", "default_normal", "brass_albedo", "brass_orm", "brass_normal"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
