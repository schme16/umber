//! Project files: save work and open it again.
//!
//! A `.umber` file is a zip, so any archive tool can look inside:
//!
//! - `project.json`: the layer stacks and their settings, the camera, brush and colours, where
//!   the model came from and where it exports to.
//! - `mesh.bin`: the model as Umber holds it (after any unwrap), so a project opens without its
//!   source file. Vertices then indices, little-endian, counts in `project.json`.
//! - per paint layer `sets/<set>/<layer>/`: `colour.png` (8-bit, sRGB, coverage in alpha),
//!   `material.rgba16f` (raw half floats: roughness, metalness, height; kept exact) and
//!   `coverage.png`; per mask, `mask.png` (greyscale).
//!
//! Saving copies every texture into staging buffers on the GPU in one go, which is instant and
//! keeps the snapshot consistent however the brush carries on, then a worker reads them back,
//! compresses and writes. The file is written under a temporary name and renamed into place, so
//! a crash mid-save never leaves half a file.
//!
//! Recovery: while there are unsaved changes, the same writer keeps a copy of the work in the
//! recovery folder, which the welcome card offers back after a crash.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail, ensure};
use eframe::egui_wgpu::wgpu;
use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::mesh::{Material, Mesh, Submesh, Vertex};
use crate::texset::{BlendMode, Channels, Compositor, Fill, Layer, LayerKind, Mask, PaintPixels, SIZE, TextureSet};

pub const EXTENSION: &str = "umber";
/// Bumped when a change means older Umbers can't read the file.
const FORMAT: u32 = 1;
const MANIFEST: &str = "project.json";
const MESH: &str = "mesh.bin";

// ---------------------------------------------------------------- manifest

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    /// The Umber that wrote it, for anyone reading the file by hand.
    written_by: String,
    /// Texture size of every set.
    size: u32,
    mesh: MeshInfo,
    sets: Vec<SetInfo>,
    settings: Settings,
}

#[derive(Serialize, Deserialize)]
struct MeshInfo {
    name: String,
    has_uvs: bool,
    vertices: usize,
    indices: usize,
    submeshes: Vec<SubmeshInfo>,
    materials: Vec<MaterialInfo>,
}

#[derive(Serialize, Deserialize)]
struct SubmeshInfo {
    material: usize,
    index_start: u32,
    index_count: u32,
}

#[derive(Serialize, Deserialize)]
struct MaterialInfo {
    name: String,
    base_color: [f32; 4],
    roughness: f32,
    metallic: f32,
}

#[derive(Serialize, Deserialize)]
struct SetInfo {
    name: String,
    selected: usize,
    /// Bottom layer first.
    layers: Vec<LayerInfo>,
}

#[derive(Serialize, Deserialize)]
struct LayerInfo {
    name: String,
    /// `None` for a paint layer.
    fill: Option<FillInfo>,
    blend: String,
    opacity: f32,
    visible: bool,
    /// Thumbnail colour of a paint layer.
    swatch: Option<[u8; 3]>,
    has_mask: bool,
    mask_invert: bool,
}

#[derive(Serialize, Deserialize)]
struct FillInfo {
    /// sRGB.
    colour: [u8; 4],
    rough: f32,
    metal: f32,
    height: f32,
    /// Colour, roughness, metalness, height.
    channels: [bool; 4],
}

/// The app's state that isn't the model or its layers. Every field has a default, so a file
/// from an older Umber (or one edited by hand) still opens.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Settings {
    pub camera: Option<CameraInfo>,
    /// The brush panel's settings, as the app serialises them.
    pub brush: serde_json::Value,
    pub primary: Option<[u8; 4]>,
    pub secondary: Option<[u8; 4]>,
    pub active_set: usize,
    pub view_mode: String,
    pub light_yaw: Option<f32>,
    pub exposure: Option<f32>,
    /// The file the model was first opened from.
    pub model_path: Option<PathBuf>,
    pub export_dir: Option<PathBuf>,
    pub export_size: Option<u32>,
    /// Only in a recovery file: the project it is unsaved work on, if it was ever saved.
    pub project_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct CameraInfo {
    pub target: [f32; 3],
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
}

// ---------------------------------------------------------------- saving

/// What goes into each texture's file.
#[derive(Clone, Copy)]
enum Encoding {
    /// RGBA8 as an RGBA PNG.
    Rgba8Png,
    /// RGBA8 as a greyscale PNG of its red channel.
    RedPng,
    /// Half floats as they are, deflated.
    Raw,
}

/// A texture copied into a buffer the worker can map.
struct Readback {
    file: String,
    encoding: Encoding,
    buffer: wgpu::Buffer,
    size: u32,
    bpp: u32,
    padded_row: u32,
}

impl Readback {
    fn start(device: &wgpu::Device, enc: &mut wgpu::CommandEncoder, tex: &wgpu::Texture, bpp: u32, file: String, encoding: Encoding) -> Self {
        let size = tex.width();
        let padded_row = (size * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("project save"),
            size: padded_row as u64 * size as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded_row), rows_per_image: Some(size) },
            },
            tex.size(),
        );
        Self { file, encoding, buffer, size, bpp, padded_row }
    }

    /// Wait for the copy and return tightly packed rows.
    fn read(self, device: &wgpu::Device) -> Result<(String, Encoding, u32, Vec<u8>)> {
        let slice = self.buffer.slice(..);
        let (send, recv) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = send.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).context("waiting for the GPU")?;
        recv.recv().context("GPU readback dropped")?.context("reading a texture back")?;
        let row = (self.size * self.bpp) as usize;
        let mut out = Vec::with_capacity(row * self.size as usize);
        {
            let mapped = slice.get_mapped_range().context("reading a texture back")?;
            for y in 0..self.size as usize {
                let start = y * self.padded_row as usize;
                out.extend_from_slice(&mapped[start..start + row]);
            }
        }
        self.buffer.unmap();
        Ok((self.file, self.encoding, self.size, out))
    }
}

/// Everything a save needs, taken from the app on the main thread. Send it to a worker and
/// call [`Snapshot::write`] there.
pub struct Snapshot {
    device: wgpu::Device,
    manifest: Manifest,
    mesh: Vec<u8>,
    textures: Vec<Readback>,
}

impl Snapshot {
    /// Copy every layer texture into staging buffers (on the GPU, so this returns at once).
    pub fn take(device: &wgpu::Device, queue: &wgpu::Queue, mesh: &Mesh, sets: &[TextureSet], settings: Settings) -> Snapshot {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("project save") });
        let mut textures = Vec::new();
        let mut set_infos = Vec::new();
        for (s, set) in sets.iter().enumerate() {
            let mut layers = Vec::new();
            for (l, layer) in set.layers.iter().enumerate() {
                let dir = format!("sets/{s}/{l}");
                if let Some(px) = &layer.pixels {
                    let [(colour, cb), (mat, mb), (cov, vb)] = px.textures();
                    textures.push(Readback::start(device, &mut enc, colour, cb, format!("{dir}/colour.png"), Encoding::Rgba8Png));
                    textures.push(Readback::start(device, &mut enc, mat, mb, format!("{dir}/material.rgba16f"), Encoding::Raw));
                    textures.push(Readback::start(device, &mut enc, cov, vb, format!("{dir}/coverage.png"), Encoding::Rgba8Png));
                }
                if let Some(mask) = &layer.mask {
                    textures.push(Readback::start(device, &mut enc, &mask.texture, 4, format!("{dir}/mask.png"), Encoding::RedPng));
                }
                layers.push(layer_info(layer));
            }
            set_infos.push(SetInfo { name: set.name.clone(), selected: set.selected, layers });
        }
        queue.submit([enc.finish()]);

        let mut bytes = Vec::with_capacity(mesh.vertices.len() * size_of::<Vertex>() + mesh.indices.len() * 4);
        bytes.extend_from_slice(bytemuck::cast_slice(&mesh.vertices));
        bytes.extend_from_slice(bytemuck::cast_slice(&mesh.indices));
        let manifest = Manifest {
            format: FORMAT,
            written_by: format!("Umber {}", env!("CARGO_PKG_VERSION")),
            size: SIZE,
            mesh: MeshInfo {
                name: mesh.name.clone(),
                has_uvs: mesh.has_uvs,
                vertices: mesh.vertices.len(),
                indices: mesh.indices.len(),
                submeshes: mesh
                    .submeshes
                    .iter()
                    .map(|s| SubmeshInfo { material: s.material, index_start: s.index_start, index_count: s.index_count })
                    .collect(),
                materials: mesh
                    .materials
                    .iter()
                    .map(|m| MaterialInfo { name: m.name.clone(), base_color: m.base_color, roughness: m.roughness, metallic: m.metallic })
                    .collect(),
            },
            sets: set_infos,
            settings,
        };
        Snapshot { device: device.clone(), manifest, mesh: bytes, textures }
    }

    /// Steps [`Snapshot::write`] counts through: one per texture, plus the mesh and the rename.
    pub fn steps(&self) -> u32 {
        self.textures.len() as u32 + 2
    }

    /// Compress and write the file. Slow: run it on a worker. `done` counts steps.
    pub fn write(self, path: &Path, done: &AtomicU32) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("couldn't make {}", dir.display()))?;
        }
        let tmp = temp_path(path);
        let result = self.write_to(&tmp, done);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return result;
        }
        std::fs::rename(&tmp, path).with_context(|| format!("couldn't write {}", path.display()))?;
        done.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn write_to(self, tmp: &Path, done: &AtomicU32) -> Result<()> {
        use zip::write::SimpleFileOptions;
        let file = File::create(tmp).with_context(|| format!("couldn't write {}", tmp.display()))?;
        let mut zip = zip::ZipWriter::new(BufWriter::new(file));
        // PNGs are compressed already; everything else is deflated, quickly.
        let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(1));

        zip.start_file(MANIFEST, deflated)?;
        serde_json::to_writer_pretty(&mut zip, &self.manifest)?;
        zip.start_file(MESH, deflated)?;
        zip.write_all(&self.mesh)?;
        done.fetch_add(1, Ordering::Relaxed);

        for readback in self.textures {
            let (name, encoding, size, data) = readback.read(&self.device)?;
            match encoding {
                Encoding::Rgba8Png => {
                    zip.start_file(name, stored)?;
                    png(&mut zip, &data, size, image::ExtendedColorType::Rgba8)?;
                }
                Encoding::RedPng => {
                    let red: Vec<u8> = data.chunks_exact(4).map(|p| p[0]).collect();
                    zip.start_file(name, stored)?;
                    png(&mut zip, &red, size, image::ExtendedColorType::L8)?;
                }
                Encoding::Raw => {
                    zip.start_file(name, deflated)?;
                    zip.write_all(&data)?;
                }
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
        zip.finish()?.flush()?;
        Ok(())
    }
}

fn png(out: &mut impl Write, data: &[u8], size: u32, colour: image::ExtendedColorType) -> Result<()> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    PngEncoder::new_with_quality(out, CompressionType::Fast, FilterType::Adaptive).write_image(data, size, size, colour)?;
    Ok(())
}

/// Beside the target, so the rename stays on one filesystem.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

fn layer_info(layer: &Layer) -> LayerInfo {
    let fill = match &layer.kind {
        LayerKind::Paint => None,
        LayerKind::Fill(f) => Some(FillInfo {
            colour: f.colour.to_array(),
            rough: f.rough,
            metal: f.metal,
            height: f.height,
            channels: [f.channels.colour, f.channels.rough, f.channels.metal, f.channels.height],
        }),
    };
    LayerInfo {
        name: layer.name.clone(),
        fill,
        blend: format!("{:?}", layer.blend),
        opacity: layer.opacity,
        visible: layer.visible,
        swatch: layer.swatch.map(|c| [c.r(), c.g(), c.b()]),
        has_mask: layer.mask.is_some(),
        mask_invert: layer.mask_invert,
    }
}

/// Start a save on a worker. The returned counter goes up to `steps` as it writes.
pub fn save_in_background(snapshot: Snapshot, path: PathBuf) -> (Arc<AtomicU32>, u32, std::sync::mpsc::Receiver<Result<()>>) {
    let done = Arc::new(AtomicU32::new(0));
    let steps = snapshot.steps();
    let (send, recv) = std::sync::mpsc::channel();
    let counter = done.clone();
    std::thread::spawn(move || {
        let _ = send.send(snapshot.write(&path, &counter));
    });
    (done, steps, recv)
}

// ---------------------------------------------------------------- opening

/// A project file read into memory, before any of it is on the GPU.
pub struct Loaded {
    pub mesh: Mesh,
    pub settings: Settings,
    sets: Vec<LoadedSet>,
}

struct LoadedSet {
    name: String,
    selected: usize,
    layers: Vec<LoadedLayer>,
}

struct LoadedLayer {
    info: LayerInfo,
    /// Colour, material, coverage.
    pixels: Option<[Vec<u8>; 3]>,
    /// RGBA8, the mask in red.
    mask: Option<Vec<u8>>,
}

pub fn is_project(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

/// Read and decode a project file. Pure CPU work; [`Loaded::into_sets`] puts it on the GPU.
pub fn read(path: &Path) -> Result<Loaded> {
    let file = File::open(path).with_context(|| format!("couldn't open {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).with_context(|| format!("{} isn't an Umber project", display_name(path)))?;
    let manifest: Manifest = {
        let entry = zip.by_name(MANIFEST).with_context(|| format!("{} isn't an Umber project", display_name(path)))?;
        serde_json::from_reader(entry).context("project.json is damaged")?
    };
    if manifest.format > FORMAT {
        bail!("{} was saved by a newer Umber; update Umber to open it", display_name(path));
    }
    ensure!(manifest.size == SIZE, "{} uses {}px textures, and this Umber only knows {SIZE}px", display_name(path), manifest.size);

    let m = &manifest.mesh;
    let bytes = entry_bytes(&mut zip, MESH)?;
    let vsize = m.vertices * size_of::<Vertex>();
    ensure!(bytes.len() == vsize + m.indices * 4, "the model in {} is damaged", display_name(path));
    let vertices: Vec<Vertex> = bytemuck::pod_collect_to_vec(&bytes[..vsize]);
    let indices: Vec<u32> = bytemuck::pod_collect_to_vec(&bytes[vsize..]);
    ensure!(indices.iter().all(|&i| (i as usize) < vertices.len()), "the model in {} is damaged", display_name(path));
    let mesh = Mesh {
        name: m.name.clone(),
        vertices,
        indices,
        submeshes: m
            .submeshes
            .iter()
            .map(|s| Submesh { material: s.material, index_start: s.index_start, index_count: s.index_count })
            .collect(),
        has_uvs: m.has_uvs,
        materials: m
            .materials
            .iter()
            .map(|mi| Material { base_color: mi.base_color, roughness: mi.roughness, metallic: mi.metallic, ..Material::named(mi.name.clone()) })
            .collect(),
    };
    ensure!(manifest.sets.len() == mesh.materials.len(), "{} has a texture set missing", display_name(path));

    let texels = (SIZE * SIZE) as usize;
    let mut sets = Vec::new();
    for (s, set) in manifest.sets.into_iter().enumerate() {
        let mut layers = Vec::new();
        for (l, info) in set.layers.into_iter().enumerate() {
            let dir = format!("sets/{s}/{l}");
            let pixels = if info.fill.is_none() {
                let colour = png_entry(&mut zip, &format!("{dir}/colour.png"), false)?;
                let material = entry_bytes(&mut zip, &format!("{dir}/material.rgba16f"))?;
                let coverage = png_entry(&mut zip, &format!("{dir}/coverage.png"), false)?;
                ensure!(material.len() == texels * 8, "layer {} in {} is damaged", info.name, display_name(path));
                Some([colour, material, coverage])
            } else {
                None
            };
            let mask = if info.has_mask { Some(png_entry(&mut zip, &format!("{dir}/mask.png"), true)?) } else { None };
            layers.push(LoadedLayer { info, pixels, mask });
        }
        sets.push(LoadedSet { name: set.name, selected: set.selected, layers });
    }
    Ok(Loaded { mesh, settings: manifest.settings, sets })
}

impl Loaded {
    /// Upload every layer and build the texture sets, one per material.
    pub fn into_sets(self, device: &wgpu::Device, queue: &wgpu::Queue, compositor: &Compositor) -> (Mesh, Settings, Vec<TextureSet>) {
        let sets = self
            .sets
            .into_iter()
            .map(|set| {
                let layers: Vec<Layer> = set.layers.into_iter().map(|l| restore_layer(device, queue, l)).collect();
                compositor.set_with(device, set.name, layers, set.selected)
            })
            .collect();
        (self.mesh, self.settings, sets)
    }
}

fn restore_layer(device: &wgpu::Device, queue: &wgpu::Queue, l: LoadedLayer) -> Layer {
    let info = l.info;
    let mut layer = match (info.fill, l.pixels) {
        (Some(f), _) => Layer::fill(
            info.name,
            Fill {
                colour: Color32::from_rgba_premultiplied(f.colour[0], f.colour[1], f.colour[2], f.colour[3]),
                rough: f.rough,
                metal: f.metal,
                height: f.height,
                channels: Channels { colour: f.channels[0], rough: f.channels[1], metal: f.channels[2], height: f.channels[3] },
            },
        ),
        (None, pixels) => {
            let mut layer = Layer::paint(device, info.name);
            if let Some([c, m, v]) = pixels {
                layer.pixels = Some(std::rc::Rc::new(PaintPixels::upload(device, queue, [&c, &m, &v])));
            }
            layer
        }
    };
    layer.blend = BlendMode::ALL.into_iter().find(|b| format!("{b:?}") == info.blend).unwrap_or(BlendMode::Normal);
    layer.opacity = info.opacity.clamp(0.0, 1.0);
    layer.visible = info.visible;
    layer.swatch = info.swatch.map(|[r, g, b]| Color32::from_rgb(r, g, b));
    layer.mask = l.mask.map(|rgba| std::rc::Rc::new(Mask::upload(device, queue, &rgba)));
    layer.mask_invert = info.mask_invert;
    layer
}

fn entry_bytes<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip.by_name(name).with_context(|| format!("{name} is missing from the project"))?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out).with_context(|| format!("{name} is damaged"))?;
    Ok(out)
}

/// A PNG at [`SIZE`] as RGBA8 texels. A greyscale one (a mask) goes into red, the rest white.
fn png_entry<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str, grey: bool) -> Result<Vec<u8>> {
    let bytes = entry_bytes(zip, name)?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).with_context(|| format!("{name} is damaged"))?;
    ensure!(img.width() == SIZE && img.height() == SIZE, "{name} is the wrong size");
    Ok(if grey {
        img.into_luma8().into_raw().into_iter().flat_map(|v| [v, 255, 255, 255]).collect()
    } else {
        img.into_rgba8().into_raw()
    })
}

pub fn display_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

// ---------------------------------------------------------------- recovery

/// Where unsaved work is kept: `$XDG_STATE_HOME/umber/recovery`, or the scripted-capture override.
pub fn recovery_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("UMBER_RECOVERY_DIR") {
        return Some(PathBuf::from(dir));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("umber/recovery"))
}

/// A fresh recovery file name for a model called `name`.
pub fn new_recovery_path(name: &str) -> Option<PathBuf> {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(40).collect();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    Some(recovery_dir()?.join(format!("{safe}-{stamp}.{EXTENSION}")))
}

/// Clear out half-written files an Umber left when it was killed mid-save (an hour old, so a
/// second Umber still writing one is left alone).
pub fn sweep_recovery_dir() {
    let Some(dir) = recovery_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        let stale = path.extension().is_some_and(|e| e == "tmp")
            && path.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Unsaved work left behind by an Umber that closed without saving (or crashed).
pub struct Recoverable {
    pub path: PathBuf,
    /// The model's name.
    pub name: String,
    pub modified: std::time::SystemTime,
}

/// Recovery files, newest first. Reads only each file's small manifest.
pub fn recoverable() -> Vec<Recoverable> {
    let Some(dir) = recovery_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut found: Vec<Recoverable> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_project(p))
        .filter_map(|path| {
            let modified = path.metadata().and_then(|m| m.modified()).ok()?;
            let name = manifest_name(&path).unwrap_or_else(|| display_name(&path));
            Some(Recoverable { path, name, modified })
        })
        .collect();
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    found
}

fn manifest_name(path: &Path) -> Option<String> {
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(path).ok()?)).ok()?;
    let entry = zip.by_name(MANIFEST).ok()?;
    let manifest: serde_json::Value = serde_json::from_reader(entry).ok()?;
    let saved = manifest["settings"]["project_path"].as_str().map(|p| display_name(Path::new(p)));
    saved.or_else(|| manifest["mesh"]["name"].as_str().map(str::to_owned))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texset::{f16_to_f32, read_level, tests::gpu};

    /// Paint, a mask, a fill and every setting survive a save and reopen exactly.
    #[test]
    fn round_trip() {
        let (device, queue) = gpu();
        let compositor = Compositor::new(&device, &queue);
        let mut mesh = crate::mesh::cube();
        mesh.materials[0].name = "Crate".into();
        let mut set = compositor.new_set(&device, &queue, &mesh.materials[0]);

        let texels = (SIZE * SIZE) as usize;
        let colour: Vec<u8> = (0..texels * 4).map(|i| (i * 7 % 251) as u8).collect();
        let material: Vec<u8> = (0..texels).flat_map(|i| {
            let h = half::f16::from_f32((i % 1000) as f32 / 999.0).to_le_bytes();
            [h[0], h[1], h[0], h[1], 0, 0x3c, 0, 0x3c]
        }).collect();
        let coverage: Vec<u8> = (0..texels * 4).map(|i| (i * 13 % 256) as u8).collect();
        let mask: Vec<u8> = (0..texels).flat_map(|i| [(i % 256) as u8, 255, 255, 255]).collect();
        let mut paint = Layer::paint(&device, "Rust");
        paint.pixels = Some(std::rc::Rc::new(PaintPixels::upload(&device, &queue, [&colour, &material, &coverage])));
        paint.mask = Some(std::rc::Rc::new(Mask::upload(&device, &queue, &mask)));
        paint.mask_invert = true;
        paint.blend = BlendMode::Overlay;
        paint.opacity = 0.4;
        paint.swatch = Some(Color32::from_rgb(1, 2, 3));
        set.layers.push(paint);
        set.selected = 1;
        if let LayerKind::Fill(f) = &mut set.layers[0].kind {
            f.rough = 0.25;
            f.channels.metal = false;
        }
        set.layers[0].visible = false;

        let settings = Settings { active_set: 0, export_size: Some(1024), view_mode: "Normals".into(), ..Default::default() };
        let dir = std::env::temp_dir().join(format!("umber-test-{}", std::process::id()));
        let path = dir.join("crate.umber");
        let snapshot = Snapshot::take(&device, &queue, &mesh, std::slice::from_ref(&set), settings);
        let done = AtomicU32::new(0);
        let steps = snapshot.steps();
        snapshot.write(&path, &done).unwrap();
        assert_eq!(done.load(Ordering::Relaxed), steps);

        let (mesh2, settings2, sets2) = read(&path).unwrap().into_sets(&device, &queue, &compositor);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(mesh2.indices, mesh.indices);
        assert_eq!(bytemuck::cast_slice::<Vertex, u8>(&mesh2.vertices), bytemuck::cast_slice::<Vertex, u8>(&mesh.vertices));
        assert_eq!(mesh2.materials[0].name, "Crate");
        assert_eq!(settings2.export_size, Some(1024));
        assert_eq!(settings2.view_mode, "Normals");

        let set2 = &sets2[0];
        assert_eq!(set2.selected, 1);
        assert_eq!(set2.layers.len(), 2);
        assert!(!set2.layers[0].visible);
        assert_eq!(set2.layers[0].kind, set.layers[0].kind);
        let l = &set2.layers[1];
        assert_eq!((l.name.as_str(), l.blend, l.opacity, l.mask_invert, l.swatch), ("Rust", BlendMode::Overlay, 0.4, true, Some(Color32::from_rgb(1, 2, 3))));
        let px = l.pixels.as_ref().unwrap();
        assert!(read_level(&device, &queue, &px.color, 0, 4).1 == colour, "colour changed");
        let mat_back = read_level(&device, &queue, &px.mat, 0, 8).1;
        assert!(mat_back == material, "material changed");
        assert_eq!(f16_to_f32(mat_back[8 * 500], mat_back[8 * 500 + 1]), half::f16::from_f32(500.0 / 999.0).to_f32());
        assert!(read_level(&device, &queue, &px.cov, 0, 4).1 == coverage, "coverage changed");
        let mask_back = read_level(&device, &queue, &l.mask.as_ref().unwrap().texture, 0, 4).1;
        assert!(mask_back.chunks_exact(4).zip(mask.chunks_exact(4)).all(|(a, b)| a[0] == b[0]), "mask changed");
    }

    #[test]
    fn not_a_project() {
        let path = std::env::temp_dir().join(format!("umber-not-a-project-{}.umber", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let err = read(&path).err().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(format!("{err:#}").contains("isn't an Umber project"), "{err:#}");
    }
}
