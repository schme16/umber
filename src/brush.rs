//! The brush engine: projection painting into paint layers and layer masks.
//!
//! A stroke starts by copying the layer it paints into `base` and clearing the stroke buffer.
//! Each frame the app turns pointer movement into dabs (screen-space ellipses) and
//! [`BrushEngine::paint`] draws the mesh in UV space so every texel tests itself against them
//! (see `shaders/brush.wgsl`). The shader marks the undo tiles it touches; when the stroke ends,
//! [`BrushEngine::end`] reads those marks back and saves the touched tiles of `base` as one undo
//! step.

use std::ops::Range;

use eframe::egui_wgpu::wgpu;
use glam::{Mat4, Vec3};

use crate::texset::{self, TileSnapshot, SIZE, TILE};

/// Dabs per draw. A frame with more (a fast stroke with a tiny brush) draws several times.
const DABS_PER_PASS: usize = 512;
const TILES: usize = ((SIZE / TILE) * (SIZE / TILE)) as usize;
const DIST_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Float;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Dab {
    /// Viewport pixels.
    pub center: [f32; 2],
    /// Unit vector along the ellipse's squashed axis.
    pub axis: [f32; 2],
    /// Pixels.
    pub radius: f32,
    /// 1 is round; smaller flattens the dab along `axis`.
    pub squash: f32,
    /// The most this dab can add to the stroke, 0..1.
    pub strength: f32,
    /// How far each dab moves a texel towards `strength`, 0..1.
    pub flow: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Paint = 0,
    Erase = 1,
    Fill = 2,
}

/// What a stroke lays down.
#[derive(Clone, Copy, Debug)]
pub struct Paint {
    pub mode: Mode,
    /// sRGB colour, 0..1. For masks only red counts (the grey value).
    pub colour: [f32; 3],
    /// Roughness, metalness, height.
    pub material: [f32; 3],
    /// Colour, roughness, metalness, height.
    pub channels: [bool; 4],
    pub hardness: f32,
    /// Fill mode only: the coverage the fill lays down.
    pub fill_opacity: f32,
    /// Paint with the brush texture (see [`BrushEngine::set_texture`]) instead of `colour`.
    pub texture: Option<TexturePaint>,
}

/// How the brush texture is laid on.
#[derive(Clone, Copy, Debug)]
pub struct TexturePaint {
    pub projection: crate::brush_texture::Projection,
    /// Size of one repeat: model units (surface), UV units (UV) or physical pixels (view).
    pub tile: f32,
    /// Radians.
    pub rotation: f32,
}

/// Where the camera is, for projecting texels onto the screen.
#[derive(Clone, Copy)]
pub struct View {
    pub view_proj: Mat4,
    pub eye: Vec3,
    /// Viewport size in physical pixels.
    pub size: [u32; 2],
}

/// The mesh to draw: shared buffers and the index ranges of the texture set being painted.
pub struct MeshDraw<'a> {
    pub vertices: &'a wgpu::Buffer,
    pub indices: &'a wgpu::Buffer,
    pub ranges: Vec<Range<u32>>,
    /// Paint only these triangles (an index buffer and its index count). The whole mesh still
    /// hides what's behind it.
    pub only: Option<(&'a wgpu::Buffer, u32)>,
}

/// What a stroke paints into.
pub enum Target<'a> {
    Layer(&'a texset::PaintPixels),
    Mask(&'a texset::Mask),
}

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    target: [f32; 4],
    colour: [f32; 4],
    material: [f32; 4],
    channels: [f32; 4],
    shape: [f32; 4],
    tex: [f32; 4],
}

struct Active {
    mask: bool,
    /// The textures being painted, with bytes per texel.
    targets: Vec<(wgpu::Texture, u32)>,
    views: Vec<wgpu::TextureView>,
}

struct DistTarget {
    size: [u32; 2],
    view_proj: [f32; 16],
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
}

pub struct BrushEngine {
    layout: wgpu::BindGroupLayout,
    layer_pipeline: wgpu::RenderPipeline,
    mask_pipeline: wgpu::RenderPipeline,
    dist_pipeline: wgpu::RenderPipeline,
    dist_bg: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    dabs: wgpu::Buffer,
    tiles: wgpu::Buffer,
    tiles_read: wgpu::Buffer,
    stroke_view: wgpu::TextureView,
    /// The painted textures as they were when the stroke started: colour (or mask), material,
    /// coverage.
    base: [(wgpu::Texture, wgpu::TextureView); 3],
    dist: Option<DistTarget>,
    no_dist: wgpu::TextureView,
    /// The brush texture's colour and height.
    texture: (wgpu::TextureView, wgpu::TextureView),
    sampler: wgpu::Sampler,
    active: Option<Active>,
}

impl BrushEngine {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brush"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/brush.wgsl").into()),
        });

        let frag = wgpu::ShaderStages::FRAGMENT;
        let uniform_entry = wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: frag,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let filterable = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: frag,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage_buffer = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: frag,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("brush layout"),
            entries: &[
                uniform_entry,
                storage_buffer(1, true),
                tex(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: frag,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::ReadWrite,
                        format: wgpu::TextureFormat::R32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                tex(4),
                tex(5),
                tex(6),
                storage_buffer(7, false),
                filterable(8),
                filterable(9),
                wgpu::BindGroupLayoutEntry {
                    binding: 10,
                    visibility: frag,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // The distance pass renders into the texture the paint pass reads, so it gets a layout
        // of its own with just the camera.
        let dist_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("brush distance layout"),
            entries: &[uniform_entry],
        });

        let vertex_layout = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<crate::mesh::Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        })];
        let pipeline = |label, layout: &wgpu::BindGroupLayout, vs, fs, targets: &[Option<wgpu::ColorTargetState>], depth: bool| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(layout)],
                ..Default::default()
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &vertex_layout,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets,
                }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                depth_stencil: depth.then(|| wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let out = |format| Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL });
        let [cf, mf, vf] = texset::PAINT_FORMATS;
        let layer_pipeline = pipeline("brush layer", &layout, "vs_uv", "fs_layer", &[out(cf), out(mf), out(vf)], false);
        let mask_pipeline = pipeline("brush mask", &layout, "vs_uv", "fs_mask", &[out(texset::MASK_FORMAT)], false);
        let dist_pipeline = pipeline("brush distance", &dist_layout, "vs_dist", "fs_dist", &[out(DIST_FORMAT)], true);

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("brush uniforms"),
            size: size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dist_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("brush distance"),
            layout: &dist_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() }],
        });
        let dabs = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dabs"),
            size: (DABS_PER_PASS * size_of::<Dab>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tiles = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("stroke tiles"),
            size: (TILES * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let tiles_read = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("stroke tiles readback"),
            size: (TILES * 4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let texture = |label, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let stroke = texture(
            "stroke",
            wgpu::TextureFormat::R32Float,
            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let base = [("stroke base colour", cf), ("stroke base material", mf), ("stroke base coverage", vf)].map(|(label, format)| {
            let t = texture(label, format, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC);
            let v = t.create_view(&Default::default());
            (t, v)
        });

        let white = crate::mesh::Image { width: 1, height: 1, rgba: vec![255; 4] };
        let plain = crate::brush_texture::GpuTexture::upload(device, queue, &crate::brush_texture::Pixels { colour: white.clone(), height: white });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("brush texture"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });

        Self {
            layout,
            layer_pipeline,
            mask_pipeline,
            dist_pipeline,
            dist_bg,
            uniforms,
            dabs,
            tiles,
            tiles_read,
            stroke_view: stroke.create_view(&Default::default()),
            base,
            dist: None,
            no_dist: texture("no distance", DIST_FORMAT, wgpu::TextureUsages::TEXTURE_BINDING).create_view(&Default::default()),
            texture: (plain.colour, plain.height),
            sampler,
            active: None,
        }
    }

    /// The texture strokes paint with when [`Paint::texture`] is set.
    pub fn set_texture(&mut self, texture: &crate::brush_texture::GpuTexture) {
        self.texture = (texture.colour.clone(), texture.height.clone());
    }

    /// Start a stroke on `target`: remember how it looks now and clear the stroke buffer.
    pub fn begin(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, target: Target<'_>) {
        let (mask, targets) = match target {
            Target::Layer(p) => (false, p.targets()),
            Target::Mask(m) => (true, vec![(m.texture.clone(), 4)]),
        };
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("stroke begin") });
        for ((tex, _), (base, _)) in targets.iter().zip(&self.base) {
            enc.copy_texture_to_texture(tex.as_image_copy(), base.as_image_copy(), tex.size());
        }
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear stroke"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.stroke_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        queue.write_buffer(&self.tiles, 0, &[0u8; TILES * 4]);
        queue.submit([enc.finish()]);
        let views = targets.iter().map(|(t, _)| t.create_view(&Default::default())).collect();
        self.active = Some(Active { mask, targets, views });
    }

    /// Lay `dabs` (or, in fill mode, the whole of `mesh`) into the active stroke.
    pub fn paint(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, mesh: &MeshDraw<'_>, view: &View, paint: &Paint, dabs: &[Dab]) {
        if self.active.is_none() {
            return;
        }
        let size = [view.size[0].max(1), view.size[1].max(1)];
        let mut u = Uniforms {
            view_proj: view.view_proj.to_cols_array_2d(),
            eye: view.eye.extend(1.0).to_array(),
            target: [size[0] as f32, size[1] as f32, 0.0, paint.mode as u32 as f32],
            colour: [paint.colour[0], paint.colour[1], paint.colour[2], 1.0],
            material: [paint.material[0], paint.material[1], paint.material[2], 0.0],
            channels: paint.channels.map(|c| if c { 1.0 } else { 0.0 }),
            shape: [paint.hardness, paint.fill_opacity, 0.0, 0.0],
            tex: match paint.texture {
                Some(t) => [t.projection.code() as f32, 1.0 / t.tile.max(1e-6), t.rotation.cos(), t.rotation.sin()],
                None => [0.0, 1.0, 1.0, 0.0],
            },
        };
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
        if paint.mode != Mode::Fill {
            self.ensure_distance(device, queue, mesh, view, size);
        }

        // A fill never reads the distance buffer, but the bind group still needs a texture there.
        let dist = self.dist.as_ref().map(|d| &d.color).unwrap_or(&self.no_dist);
        let active = self.active.as_ref().expect("checked above");
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("brush"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.dabs.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(dist) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&self.stroke_view) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&self.base[0].1) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&self.base[1].1) },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&self.base[2].1) },
                wgpu::BindGroupEntry { binding: 7, resource: self.tiles.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(&self.texture.0) },
                wgpu::BindGroupEntry { binding: 9, resource: wgpu::BindingResource::TextureView(&self.texture.1) },
                wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });

        let chunks: Vec<&[Dab]> = if paint.mode == Mode::Fill { vec![&[]] } else { dabs.chunks(DABS_PER_PASS).collect() };
        for chunk in chunks {
            u.target[2] = chunk.len() as f32;
            queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
            if !chunk.is_empty() {
                queue.write_buffer(&self.dabs, 0, bytemuck::cast_slice(chunk));
            }
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("brush") });
            {
                let attachments: Vec<_> = active
                    .views
                    .iter()
                    .map(|view| {
                        Some(wgpu::RenderPassColorAttachment {
                            view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })
                    })
                    .collect();
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("brush"),
                    color_attachments: &attachments,
                    ..Default::default()
                });
                pass.set_pipeline(if active.mask { &self.mask_pipeline } else { &self.layer_pipeline });
                pass.set_bind_group(0, &bg, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                match mesh.only {
                    Some((only, count)) => {
                        pass.set_index_buffer(only.slice(..), wgpu::IndexFormat::Uint32);
                        pass.draw_indexed(0..count, 0, 0..1);
                    }
                    None => {
                        pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                        for range in &mesh.ranges {
                            pass.draw_indexed(range.clone(), 0, 0..1);
                        }
                    }
                }
            }
            queue.submit([enc.finish()]);
        }
    }

    /// Finish the stroke. Returns the undo step for it, or `None` when it changed nothing.
    pub fn end(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<TileSnapshot> {
        let active = self.active.take()?;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("stroke tiles") });
        enc.copy_buffer_to_buffer(&self.tiles, 0, &self.tiles_read, 0, (TILES * 4) as u64);
        queue.submit([enc.finish()]);
        self.tiles_read.slice(..).map_async(wgpu::MapMode::Read, |r| {
            if let Err(e) = r {
                log::error!("reading stroke tiles failed: {e}");
            }
        });
        if let Err(e) = device.poll(wgpu::PollType::wait_indefinitely()) {
            log::error!("waiting for stroke tiles failed: {e}");
            return None;
        }
        let flags: Vec<u32> = {
            let Ok(mapped) = self.tiles_read.slice(..).get_mapped_range() else { return None };
            bytemuck::cast_slice(&mapped).to_vec()
        };
        self.tiles_read.unmap();
        let n = SIZE / TILE;
        let touched: Vec<[u32; 2]> =
            flags.iter().enumerate().filter(|(_, f)| **f != 0).map(|(i, _)| [i as u32 % n, i as u32 / n]).collect();
        if touched.is_empty() {
            return None;
        }
        let sources: Vec<&wgpu::Texture> = self.base.iter().take(active.targets.len()).map(|(t, _)| t).collect();
        Some(TileSnapshot::capture_from(device, queue, &sources, active.targets, touched))
    }

    /// Render the camera's distance to the nearest surface, unless the camera hasn't moved.
    fn ensure_distance(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, mesh: &MeshDraw<'_>, view: &View, size: [u32; 2]) {
        let key = view.view_proj.to_cols_array();
        if self.dist.as_ref().is_some_and(|d| d.size == size && d.view_proj == key) {
            return;
        }
        let extent = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 };
        let make = |label, format, usage| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let target = match self.dist.take() {
            Some(d) if d.size == size => DistTarget { view_proj: key, ..d },
            _ => DistTarget {
                size,
                view_proj: key,
                color: make("brush distance", DIST_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING),
                depth: make("brush depth", DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT),
            },
        };
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("brush distance") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("brush distance"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 1e30, g: 0.0, b: 0.0, a: 0.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            pass.set_pipeline(&self.dist_pipeline);
            pass.set_bind_group(0, &self.dist_bg, &[]);
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            // Every material occludes, not just the one being painted.
            let count = (mesh.indices.size() / 4) as u32;
            pass.draw_indexed(0..count, 0, 0..1);
        }
        queue.submit([enc.finish()]);
        self.dist = Some(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::Vertex;
    use crate::texset::tests::{gpu, read_texel};
    use eframe::egui_wgpu::wgpu::util::DeviceExt;

    /// A 2 x 2 square facing +Z with UVs over the whole texture, seen from +Z.
    fn quad(device: &wgpu::Device) -> (wgpu::Buffer, wgpu::Buffer, View) {
        let v = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [(x + 1.0) * 0.5, (1.0 - y) * 0.5], tangent: [1.0, 0.0, 0.0, 1.0] };
        let verts = [v(-1.0, -1.0), v(1.0, -1.0), v(1.0, 1.0), v(-1.0, 1.0)];
        let idx: [u32; 6] = [0, 1, 2, 0, 2, 3];
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&idx), usage: wgpu::BufferUsages::INDEX });
        let eye = Vec3::new(0.0, 0.0, 3.0);
        let view_proj = Mat4::perspective_rh(0.8, 1.0, 0.1, 100.0) * Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
        (vb, ib, View { view_proj, eye, size: [256, 256] })
    }

    fn paint() -> Paint {
        Paint { mode: Mode::Paint, colour: [1.0, 0.0, 0.0], material: [0.2, 1.0, 0.5], channels: [true, true, false, false], hardness: 0.8, fill_opacity: 1.0, texture: None }
    }

    #[test]
    fn dab_paints_where_the_camera_looks_and_undo_restores() {
        let (device, queue) = gpu();
        let (vb, ib, view) = quad(&device);
        let mesh = MeshDraw { vertices: &vb, indices: &ib, ranges: vec![0..6], only: None };
        let pixels = texset::PaintPixels::new(&device);
        let mut brush = BrushEngine::new(&device, &queue);
        brush.begin(&device, &queue, Target::Layer(&pixels));
        let dab = Dab { center: [128.0, 128.0], axis: [1.0, 0.0], radius: 20.0, squash: 1.0, strength: 1.0, flow: 1.0 };
        brush.paint(&device, &queue, &mesh, &view, &paint(), &[dab]);

        let mid = SIZE / 2;
        assert_eq!(read_texel(&device, &queue, &pixels.color, mid, mid, 4), vec![255, 0, 0, 255], "centre painted red");
        assert_eq!(read_texel(&device, &queue, &pixels.color, 10, 10, 4)[3], 0, "corner untouched");
        let cov = read_texel(&device, &queue, &pixels.cov, mid, mid, 4);
        assert_eq!((cov[0], cov[1]), (255, 0), "roughness covered, metalness not");

        let undo = brush.end(&device, &queue).expect("the stroke changed something");
        assert!(undo.bytes() > 0);
        undo.restore(&device, &queue);
        assert_eq!(read_texel(&device, &queue, &pixels.color, mid, mid, 4)[3], 0, "undo clears the dab");
    }

    #[test]
    fn opacity_caps_overlapping_dabs_and_erase_removes() {
        let (device, queue) = gpu();
        let (vb, ib, view) = quad(&device);
        let mesh = MeshDraw { vertices: &vb, indices: &ib, ranges: vec![0..6], only: None };
        let pixels = texset::PaintPixels::new(&device);
        let mut brush = BrushEngine::new(&device, &queue);
        let mid = SIZE / 2;
        let half = Dab { center: [128.0, 128.0], axis: [1.0, 0.0], radius: 20.0, squash: 1.0, strength: 0.5, flow: 1.0 };

        brush.begin(&device, &queue, Target::Layer(&pixels));
        for _ in 0..5 {
            brush.paint(&device, &queue, &mesh, &view, &paint(), &[half, half]);
        }
        let a = read_texel(&device, &queue, &pixels.color, mid, mid, 4)[3];
        assert!((a as i32 - 128).abs() <= 1, "one stroke at 50% stays at 50%, got {a}");
        brush.end(&device, &queue);

        let erase = Paint { mode: Mode::Erase, ..paint() };
        let full = Dab { strength: 1.0, ..half };
        brush.begin(&device, &queue, Target::Layer(&pixels));
        brush.paint(&device, &queue, &mesh, &view, &erase, &[full]);
        assert_eq!(read_texel(&device, &queue, &pixels.color, mid, mid, 4)[3], 0, "erased");
        brush.end(&device, &queue);
    }

    #[test]
    fn texture_paints_its_colours_and_skips_see_through_parts() {
        let (device, queue) = gpu();
        let (vb, ib, view) = quad(&device);
        let mesh = MeshDraw { vertices: &vb, indices: &ib, ranges: vec![0..6], only: None };
        // 8 x 8: the left half opaque red, the right half see-through blue.
        let mut rgba = Vec::new();
        for _ in 0..8 {
            for x in 0..8 {
                rgba.extend_from_slice(if x < 4 { &[255, 0, 0, 255] } else { &[0, 0, 255, 0] });
            }
        }
        let img = crate::mesh::Image { width: 8, height: 8, rgba };
        let tex = crate::brush_texture::GpuTexture::upload(&device, &queue, &crate::brush_texture::Pixels::from_image(img));
        let pixels = texset::PaintPixels::new(&device);
        let mut brush = BrushEngine::new(&device, &queue);
        brush.set_texture(&tex);
        let textured = Paint {
            mode: Mode::Fill,
            colour: [0.0, 1.0, 0.0],
            texture: Some(TexturePaint { projection: crate::brush_texture::Projection::Uv, tile: 1.0, rotation: 0.0 }),
            ..paint()
        };
        brush.begin(&device, &queue, Target::Layer(&pixels));
        brush.paint(&device, &queue, &mesh, &view, &textured, &[]);
        let left = read_texel(&device, &queue, &pixels.color, SIZE / 4, SIZE / 2, 4);
        assert_eq!(left, vec![255, 0, 0, 255], "the texture's red, not the brush colour");
        let right = read_texel(&device, &queue, &pixels.color, SIZE * 3 / 4, SIZE / 2, 4);
        assert_eq!(right[3], 0, "nothing lands where the texture is see-through");
        brush.end(&device, &queue);
    }

    #[test]
    fn only_the_chosen_triangles_are_painted() {
        let (device, queue) = gpu();
        let (vb, ib, view) = quad(&device);
        // The first of the quad's two triangles: below the diagonal from (-1,-1) to (1,1).
        let first = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&[0u32, 1, 2]),
            usage: wgpu::BufferUsages::INDEX,
        });
        let mesh = MeshDraw { vertices: &vb, indices: &ib, ranges: vec![0..6], only: Some((&first, 3)) };
        let pixels = texset::PaintPixels::new(&device);
        let mut brush = BrushEngine::new(&device, &queue);
        brush.begin(&device, &queue, Target::Layer(&pixels));
        let dab = Dab { center: [128.0, 128.0], axis: [1.0, 0.0], radius: 400.0, squash: 1.0, strength: 1.0, flow: 1.0 };
        brush.paint(&device, &queue, &mesh, &view, &paint(), &[dab]);
        assert_eq!(read_texel(&device, &queue, &pixels.color, SIZE * 3 / 4, SIZE * 3 / 4, 4)[3], 255, "chosen triangle painted");
        assert_eq!(read_texel(&device, &queue, &pixels.color, SIZE / 4, SIZE / 4, 4)[3], 0, "the other one left alone");
        brush.end(&device, &queue);
    }

    #[test]
    fn hidden_surfaces_are_not_painted() {
        let (device, queue) = gpu();
        let (_, _, view) = quad(&device);
        // The painted square at z = 0, and a smaller square in front of its middle (another
        // material, not painted) that hides it from the camera.
        let v = |x: f32, y: f32, z: f32| Vertex { pos: [x, y, z], normal: [0.0, 0.0, 1.0], uv: [(x + 1.0) * 0.5, (1.0 - y) * 0.5], tangent: [1.0, 0.0, 0.0, 1.0] };
        let verts = [v(-1.0, -1.0, 0.0), v(1.0, -1.0, 0.0), v(1.0, 1.0, 0.0), v(-1.0, 1.0, 0.0), v(-0.3, -0.3, 1.0), v(0.3, -0.3, 1.0), v(0.3, 0.3, 1.0), v(-0.3, 0.3, 1.0)];
        let idx: [u32; 12] = [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7];
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&idx), usage: wgpu::BufferUsages::INDEX });
        let mesh = MeshDraw { vertices: &vb, indices: &ib, ranges: vec![0..6], only: None };
        let pixels = texset::PaintPixels::new(&device);
        let mut brush = BrushEngine::new(&device, &queue);
        brush.begin(&device, &queue, Target::Layer(&pixels));
        // Covers the whole view.
        let dab = Dab { center: [128.0, 128.0], axis: [1.0, 0.0], radius: 400.0, squash: 1.0, strength: 1.0, flow: 1.0 };
        brush.paint(&device, &queue, &mesh, &view, &paint(), &[dab]);
        let mid = SIZE / 2;
        assert_eq!(read_texel(&device, &queue, &pixels.color, mid, mid, 4)[3], 0, "hidden middle stays clean");
        assert_eq!(read_texel(&device, &queue, &pixels.color, 40, 40, 4)[3], 255, "visible corner painted");
        brush.end(&device, &queue);
    }

    #[test]
    fn maps_are_padded_past_island_edges() {
        let (device, queue) = gpu();
        // One triangle covering the left half of the UV square only.
        let v = |u: f32, w: f32| Vertex { pos: [u, w, 0.0], normal: [0.0, 0.0, 1.0], uv: [u, w], tangent: [1.0, 0.0, 0.0, 1.0] };
        let verts = [v(0.0, 0.0), v(0.5, 0.0), v(0.5, 1.0), v(0.0, 1.0)];
        let idx: [u32; 6] = [0, 1, 2, 0, 2, 3];
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&idx), usage: wgpu::BufferUsages::INDEX });
        let mut comp = texset::Compositor::new(&device, &queue);
        let mut set = comp.new_set(&device, &queue, &crate::mesh::Material::named("Test"));
        // Green inside the island, blue outside it: the blue must never show.
        let layer = texset::Layer::paint(&device, "p");
        let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for _ in 0..SIZE {
            for x in 0..SIZE {
                rgba.extend_from_slice(if x < SIZE / 2 { &[10, 200, 30, 255] } else { &[0, 0, 255, 255] });
            }
        }
        queue.write_texture(
            layer.pixels.as_ref().unwrap().color.as_image_copy(),
            &rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIZE * 4), rows_per_image: Some(SIZE) },
            wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        );
        set.layers = vec![layer];
        comp.update(&device, &queue, &mut set);
        let color = set.output_textures()[0].clone();
        assert_eq!(read_texel(&device, &queue, &color, SIZE * 3 / 4, SIZE / 2, 4), vec![0, 0, 255, 255], "unpadded first");
        comp.set_islands(&device, &queue, &mut set, &vb, &ib, &[0..6]);
        comp.update(&device, &queue, &mut set);
        let px = read_texel(&device, &queue, &color, SIZE * 3 / 4, SIZE / 2, 4);
        assert_eq!(px, vec![10, 200, 30, 255], "outside the island takes the nearest island colour");
    }
}
