//! 3D viewport renderer. Draws the model into an offscreen MSAA target which egui then
//! shows as an image, so the floating panels sit on top of a full-bleed viewport.

use eframe::egui_wgpu::{self, wgpu};
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::mesh::{Mesh, Vertex};
use crate::texset::TextureSet;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SAMPLES: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Lit = 0,
    Colour = 1,
    UvCheck = 2,
    Normals = 3,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    key_dir: [f32; 4],
    fill_dir: [f32; 4],
    /// unused, unused, view mode, unused
    params: [f32; 4],
    /// exposure, unused...
    misc: [f32; 4],
}

pub struct FrameParams {
    pub view: Mat4,
    pub proj: Mat4,
    pub eye: Vec3,
    pub light_yaw: f32,
    pub view_mode: ViewMode,
    pub exposure: f32,
}

struct GpuMesh {
    vertex: wgpu::Buffer,
    index: wgpu::Buffer,
    /// Index range and which texture set (material) draws it.
    submeshes: Vec<(std::ops::Range<u32>, usize)>,
    /// One bind group per texture set.
    materials: Vec<wgpu::BindGroup>,
}

struct Targets {
    size: [u32; 2],
    msaa: wgpu::TextureView,
    resolve: wgpu::TextureView,
    depth: wgpu::TextureView,
    egui_id: egui::TextureId,
}

pub struct ViewportRenderer {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    material_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    mesh: Option<GpuMesh>,
    targets: Option<Targets>,
}

impl ViewportRenderer {
    pub fn new(rs: &egui_wgpu::RenderState) -> Self {
        let device = &rs.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewport shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/viewport.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        // Colour, sampler, material (rough, metal, height, AO), normal.
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("material layout"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(2),
                texture_entry(3),
            ],
        });

        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals bg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewport layout"),
            bind_group_layouts: &[Some(&globals_layout), Some(&material_layout)],
            ..Default::default()
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewport pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Float32x4],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: SAMPLES, ..Default::default() },
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 16,
            ..Default::default()
        });

        Self { pipeline, globals, globals_bg, material_layout, sampler, mesh: None, targets: None }
    }

    /// `sets` holds one texture set per mesh material, in the same order.
    pub fn set_mesh(&mut self, rs: &egui_wgpu::RenderState, mesh: &Mesh, sets: &[TextureSet]) {
        let device = &rs.device;
        let vertex = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh vertices"),
            contents: bytemuck::cast_slice(&mesh.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh indices"),
            contents: bytemuck::cast_slice(&mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        let materials = sets
            .iter()
            .map(|set| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("material bg"),
                    layout: &self.material_layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(set.color_view()) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(set.mat_view()) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(set.normal_view()) },
                    ],
                })
            })
            .collect();
        let submeshes = mesh
            .submeshes
            .iter()
            .map(|s| (s.index_start..s.index_start + s.index_count, s.material.min(sets.len().saturating_sub(1))))
            .collect();
        self.mesh = Some(GpuMesh { vertex, index, submeshes, materials });
    }

    /// The mesh's vertex and index buffers, and the index ranges each texture set draws.
    pub fn mesh_draw(&self, set: usize) -> Option<crate::brush::MeshDraw<'_>> {
        let mesh = self.mesh.as_ref()?;
        let ranges = mesh.submeshes.iter().filter(|(_, m)| *m == set).map(|(r, _)| r.clone()).collect();
        Some(crate::brush::MeshDraw { vertices: &mesh.vertex, indices: &mesh.index, ranges, only: None })
    }

    /// Render one frame at `size` physical pixels and return the egui texture showing it.
    pub fn render(&mut self, rs: &egui_wgpu::RenderState, size: [u32; 2], p: &FrameParams) -> egui::TextureId {
        let size = [size[0].clamp(1, 8192), size[1].clamp(1, 8192)];
        self.ensure_targets(rs, size);
        let targets = self.targets.as_ref().expect("targets just created");

        let key = Vec3::new(p.light_yaw.sin() * 0.75, 0.62, p.light_yaw.cos() * 0.75).normalize();
        let fill = Vec3::new(-key.x, 0.25, -key.z).normalize();
        let g = Globals {
            view_proj: (p.proj * p.view).to_cols_array_2d(),
            cam_pos: p.eye.extend(1.0).to_array(),
            key_dir: key.extend(0.0).to_array(),
            fill_dir: fill.extend(0.0).to_array(),
            params: [0.0, 0.0, p.view_mode as u32 as f32, 0.0],
            misc: [p.exposure, 0.0, 0.0, 0.0],
        };
        rs.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&g));

        let mut encoder = rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("viewport") });
        {
            let bg = srgb_to_linear(crate::theme::VIEWPORT);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewport pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.msaa,
                    depth_slice: None,
                    resolve_target: Some(&targets.resolve),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: bg[0], g: bg[1], b: bg[2], a: 1.0 }),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            if let Some(mesh) = &self.mesh {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, mesh.vertex.slice(..));
                pass.set_index_buffer(mesh.index.slice(..), wgpu::IndexFormat::Uint32);
                for (range, material) in &mesh.submeshes {
                    if let Some(bg) = mesh.materials.get(*material) {
                        pass.set_bind_group(1, bg, &[]);
                        pass.draw_indexed(range.clone(), 0, 0..1);
                    }
                }
            }
        }
        rs.queue.submit([encoder.finish()]);
        targets.egui_id
    }

    fn ensure_targets(&mut self, rs: &egui_wgpu::RenderState, size: [u32; 2]) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let device = &rs.device;
        let extent = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 };
        let make = |label, format, samples, usage, view_formats: &[wgpu::TextureFormat]| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: extent,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats,
            })
        };
        let msaa = make("viewport msaa", COLOR_FORMAT, SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT, &[])
            .create_view(&Default::default());
        // egui shows native textures without any colour conversion, so it gets a plain
        // UNORM view of the same bytes while we render through an sRGB view.
        let resolve_tex = make(
            "viewport resolve",
            wgpu::TextureFormat::Rgba8Unorm,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            &[COLOR_FORMAT],
        );
        let resolve = resolve_tex.create_view(&wgpu::TextureViewDescriptor { format: Some(COLOR_FORMAT), ..Default::default() });
        let display = resolve_tex.create_view(&Default::default());
        let depth = make("viewport depth", DEPTH_FORMAT, SAMPLES, wgpu::TextureUsages::RENDER_ATTACHMENT, &[])
            .create_view(&Default::default());

        let mut renderer = rs.renderer.write();
        let egui_id = match &self.targets {
            Some(old) => {
                renderer.update_egui_texture_from_wgpu_texture(device, &display, wgpu::FilterMode::Nearest, old.egui_id);
                old.egui_id
            }
            None => renderer.register_native_texture(device, &display, wgpu::FilterMode::Nearest),
        };
        self.targets = Some(Targets { size, msaa, resolve, depth, egui_id });
    }
}

fn srgb_to_linear(c: egui::Color32) -> [f64; 3] {
    let f = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    [f(c.r()), f(c.g()), f(c.b())]
}
