//! Texture sets and the GPU layer stack.
//!
//! Each material gets a texture set: a stack of layers that compute shaders flatten into
//! three maps the viewport samples (and export will write out):
//!
//! - colour: RGBA8, sRGB-encoded
//! - material: RGBA16F, roughness / metalness / height (0.5 = flat) / AO
//! - normal: RGBA8, tangent space, OpenGL convention, derived from height
//!
//! A paint layer owns three textures the brush will draw into: colour with its coverage in
//! alpha, material values, and coverage for each material channel. So a stroke that paints
//! colour and roughness leaves metalness alone. A fill layer is a constant value per channel.
//! Layers hold their textures behind `Rc`, so an undo step can keep a deleted layer alive for
//! free, and the stack itself is cheap to snapshot.

use std::rc::Rc;

use eframe::egui_wgpu::wgpu;
use egui::Color32;

use crate::mesh;

/// Texture set resolution. One size for every set until the resolution picker lands.
pub const SIZE: u32 = 2048;
/// Undo snapshots copy whole tiles of this many texels square.
pub const TILE: u32 = 256;

const ACCUM_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const MAT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const COV_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// A paint layer's colour, material and coverage textures, in that order.
pub const PAINT_FORMATS: [wgpu::TextureFormat; 3] = [COLOR_FORMAT, MAT_FORMAT, COV_FORMAT];
pub const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// "No island texel near here" in the padding map.
const NO_SEED: u32 = u32::MAX;
/// A full 0 to 1 height step across one texel, scaled so a ramp over ~1 % of a 2048 map
/// reads as about 45 degrees.
const NORMAL_STRENGTH: f32 = 20.0 * SIZE as f32 / 2048.0;

// ---------------------------------------------------------------- layer model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Overlay,
    Add,
    Subtract,
    Darken,
    Lighten,
    SoftLight,
}

impl BlendMode {
    pub const ALL: [BlendMode; 9] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Add,
        BlendMode::Subtract,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::SoftLight,
    ];

    pub fn name(self) -> &'static str {
        match self {
            BlendMode::Normal => "Normal",
            BlendMode::Multiply => "Multiply",
            BlendMode::Screen => "Screen",
            BlendMode::Overlay => "Overlay",
            BlendMode::Add => "Add",
            BlendMode::Subtract => "Subtract",
            BlendMode::Darken => "Darken",
            BlendMode::Lighten => "Lighten",
            BlendMode::SoftLight => "Soft light",
        }
    }
}

/// Which channels a fill layer sets.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Channels {
    pub colour: bool,
    pub rough: bool,
    pub metal: bool,
    pub height: bool,
}

impl Channels {
    pub const ALL: Channels = Channels { colour: true, rough: true, metal: true, height: true };
    fn as_vec4(self) -> [f32; 4] {
        let f = |b: bool| if b { 1.0 } else { 0.0 };
        [f(self.colour), f(self.rough), f(self.metal), f(self.height)]
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Fill {
    /// sRGB colour.
    pub colour: Color32,
    pub rough: f32,
    pub metal: f32,
    /// 0.5 is flat.
    pub height: f32,
    pub channels: Channels,
}

#[derive(Clone, PartialEq, Debug)]
pub enum LayerKind {
    Paint,
    Fill(Fill),
}

/// The textures a paint layer owns, all at [`SIZE`].
pub struct PaintPixels {
    pub color: wgpu::Texture,
    pub mat: wgpu::Texture,
    pub cov: wgpu::Texture,
    color_view: wgpu::TextureView,
    mat_view: wgpu::TextureView,
    cov_view: wgpu::TextureView,
}

impl PaintPixels {
    /// GPU memory the layer's textures take.
    pub fn bytes(&self) -> u64 {
        [&self.color, &self.mat, &self.cov].into_iter().map(crate::stats::texture_bytes).sum()
    }

    /// A fully transparent layer.
    pub fn new(device: &wgpu::Device) -> Self {
        let make = |label, format| layer_texture(device, label, format, SIZE);
        let color = make("layer colour", COLOR_FORMAT);
        let mat = make("layer material", MAT_FORMAT);
        let cov = make("layer coverage", COV_FORMAT);
        Self {
            color_view: color.create_view(&Default::default()),
            mat_view: mat.create_view(&Default::default()),
            cov_view: cov.create_view(&Default::default()),
            color,
            mat,
            cov,
        }
    }

    /// A layer with these pixels: colour, material and coverage, tightly packed at [`SIZE`].
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, data: [&[u8]; 3]) -> Self {
        let pixels = Self::new(device);
        for ((tex, bpp), bytes) in pixels.textures().into_iter().zip(data) {
            write_full(queue, tex, bytes, bpp);
        }
        pixels
    }

    /// The three textures with their bytes per texel, in a fixed order.
    pub fn textures(&self) -> [(&wgpu::Texture, u32); 3] {
        [(&self.color, 4), (&self.mat, 8), (&self.cov, 4)]
    }

    /// The same, as owned handles (for undo snapshots and strokes).
    pub fn targets(&self) -> Vec<(wgpu::Texture, u32)> {
        self.textures().into_iter().map(|(t, bpp)| (t.clone(), bpp)).collect()
    }
}

/// Greyscale layer mask in the red channel; white shows the layer.
pub struct Mask {
    pub texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Mask {
    pub fn white(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let texture = layer_texture(device, "layer mask", MASK_FORMAT, SIZE);
        write_full(queue, &texture, &vec![255u8; (SIZE * SIZE * 4) as usize], 4);
        Self { view: texture.create_view(&Default::default()), texture }
    }

    /// A mask from RGBA8 texels at [`SIZE`] (only red counts).
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8]) -> Self {
        let texture = layer_texture(device, "layer mask", MASK_FORMAT, SIZE);
        write_full(queue, &texture, rgba, 4);
        Self { view: texture.create_view(&Default::default()), texture }
    }
}

#[derive(Clone)]
pub struct Layer {
    pub name: String,
    pub kind: LayerKind,
    pub blend: BlendMode,
    pub opacity: f32,
    pub visible: bool,
    /// Thumbnail colour for paint layers; `None` shows an empty (checker) thumbnail.
    pub swatch: Option<Color32>,
    pub pixels: Option<Rc<PaintPixels>>,
    pub mask: Option<Rc<Mask>>,
    pub mask_invert: bool,
}

impl Layer {
    pub fn paint(device: &wgpu::Device, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: LayerKind::Paint,
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
            swatch: None,
            pixels: Some(Rc::new(PaintPixels::new(device))),
            mask: None,
            mask_invert: false,
        }
    }

    pub fn fill(name: impl Into<String>, fill: Fill) -> Self {
        Self {
            name: name.into(),
            kind: LayerKind::Fill(fill),
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
            swatch: None,
            pixels: None,
            mask: None,
            mask_invert: false,
        }
    }

    /// The same settings and the same textures (not a copy of them).
    pub fn same(&self, o: &Layer) -> bool {
        fn same_rc<T>(a: &Option<Rc<T>>, b: &Option<Rc<T>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        self.name == o.name
            && self.kind == o.kind
            && self.blend == o.blend
            && self.opacity == o.opacity
            && self.visible == o.visible
            && self.mask_invert == o.mask_invert
            && same_rc(&self.pixels, &o.pixels)
            && same_rc(&self.mask, &o.mask)
    }

    /// "Paint · Multiply", shown under the layer name.
    pub fn kind_label(&self) -> String {
        let kind = match self.kind {
            LayerKind::Paint => "Paint",
            LayerKind::Fill(_) => "Fill",
        };
        format!("{kind} · {}", self.blend.name())
    }

    pub fn thumb_colour(&self) -> Option<Color32> {
        match &self.kind {
            LayerKind::Fill(f) if f.channels.colour => Some(f.colour),
            LayerKind::Fill(_) => Some(Color32::from_gray(0x80)),
            LayerKind::Paint => self.swatch,
        }
    }

    /// A copy with its own textures (pixels copied on the GPU).
    pub fn duplicate(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let mut copy = self.clone();
        copy.name = format!("{} copy", self.name);
        if let Some(src) = &self.pixels {
            let dst = PaintPixels::new(device);
            let mut enc = device.create_command_encoder(&Default::default());
            for ((s, _), (d, _)) in src.textures().into_iter().zip(dst.textures()) {
                enc.copy_texture_to_texture(s.as_image_copy(), d.as_image_copy(), s.size());
            }
            queue.submit([enc.finish()]);
            copy.pixels = Some(Rc::new(dst));
        }
        copy
    }
}

pub fn same_stack(a: &[Layer], b: &[Layer]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y))
}

// ---------------------------------------------------------------- texture set

pub struct TextureSet {
    pub name: String,
    /// Bottom layer first.
    pub layers: Vec<Layer>,
    pub selected: usize,
    /// Pixels changed under an unchanged stack (an undo of a stroke, say).
    pub dirty: bool,
    /// The stack as the undo history last saw it.
    pub committed: Vec<Layer>,
    composited: Option<Vec<Layer>>,
    outputs: Outputs,
    /// For every texel, the nearest texel the mesh's UVs cover (packed x | y << 16), so the
    /// maps can be padded past island edges. `None` until [`Compositor::set_islands`] runs.
    padding: Option<wgpu::TextureView>,
}

struct Outputs {
    color: wgpu::Texture,
    mat: wgpu::Texture,
    normal: wgpu::Texture,
    color_srgb_view: wgpu::TextureView,
    mat_view: wgpu::TextureView,
    normal_view: wgpu::TextureView,
    /// Single-level views (base level storage views for resolve).
    color_level0: wgpu::TextureView,
    mat_level0: wgpu::TextureView,
    normal_level0: wgpu::TextureView,
    /// Downsample passes: pipeline index, bind group, destination size.
    mip_passes: Vec<(usize, wgpu::BindGroup, u32)>,
}

impl TextureSet {
    /// GPU memory the composited maps take, with their mips.
    pub fn map_bytes(&self) -> u64 {
        let o = &self.outputs;
        [&o.color, &o.mat, &o.normal].into_iter().map(crate::stats::texture_bytes).sum()
    }

    pub fn selected_layer(&self) -> Option<&Layer> {
        self.layers.get(self.selected)
    }

    /// sRGB view of the colour map, all mips. Sampling gives linear colour.
    pub fn color_view(&self) -> &wgpu::TextureView {
        &self.outputs.color_srgb_view
    }

    pub fn mat_view(&self) -> &wgpu::TextureView {
        &self.outputs.mat_view
    }

    pub fn normal_view(&self) -> &wgpu::TextureView {
        &self.outputs.normal_view
    }

    /// Colour (sRGB bytes), material (rough, metal, height, AO as half floats) and normal.
    pub fn output_textures(&self) -> [&wgpu::Texture; 3] {
        [&self.outputs.color, &self.outputs.mat, &self.outputs.normal]
    }

    fn needs_composite(&self) -> bool {
        self.dirty || !self.composited.as_ref().is_some_and(|c| same_stack(c, &self.layers))
    }
}

// ---------------------------------------------------------------- compositor

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerParams {
    fill_color: [f32; 4],
    fill_mat: [f32; 4],
    channels: [f32; 4],
    mode: u32,
    kind: u32,
    opacity: f32,
    mask_invert: u32,
}

const PARAM_STRIDE: u64 = 256;

/// Shared GPU state for flattening layer stacks. One per app; works on any texture set.
pub struct Compositor {
    blend: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    /// sRGB colour, linear float, normal.
    mips: [wgpu::ComputePipeline; 3],
    accum: [(wgpu::TextureView, wgpu::TextureView); 2],
    params: wgpu::Buffer,
    params_capacity: u64,
    resolve_params: wgpu::Buffer,
    white: wgpu::TextureView,
    empty_color: wgpu::TextureView,
    empty_mat: wgpu::TextureView,
    no_padding: wgpu::TextureView,
    islands: wgpu::RenderPipeline,
    jump: wgpu::ComputePipeline,
    /// Paint transfer to new UVs: 8-bit targets, then float.
    transfer: [wgpu::RenderPipeline; 2],
}

impl Compositor {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/composite.wgsl").into()),
        });
        let pipeline = |module: &wgpu::ShaderModule, entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let blend = pipeline(&module, "blend_layer");
        let resolve = pipeline(&module, "resolve");
        let mips = [("rgba8unorm", "srgb"), ("rgba16float", "linear"), ("rgba8unorm", "normal")].map(|(fmt, mode)| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("mips"),
                source: wgpu::ShaderSource::Wgsl(mip_shader(fmt, mode).into()),
            });
            pipeline(&module, "main")
        });

        let accum_tex = |label| {
            layer_texture(device, label, ACCUM_FORMAT, SIZE).create_view(&Default::default())
        };
        let accum = [(accum_tex("accum colour a"), accum_tex("accum mat a")), (accum_tex("accum colour b"), accum_tex("accum mat b"))];

        let tiny = |label, format, bytes: &[u8]| {
            let t = layer_texture(device, label, format, 1);
            write_full(queue, &t, bytes, bytes.len() as u32);
            t.create_view(&Default::default())
        };
        let white = tiny("white mask", MASK_FORMAT, &[255; 4]);
        let empty_color = tiny("empty colour", COLOR_FORMAT, &[0; 4]);
        let empty_mat = tiny("empty mat", MAT_FORMAT, &[0; 8]);
        let no_padding = tiny("no padding", wgpu::TextureFormat::R32Uint, &NO_SEED.to_le_bytes());

        let padding = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("padding"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/padding.wgsl").into()),
        });
        let islands = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("uv islands"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &padding,
                entry_point: Some("vs_islands"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<mesh::Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: std::mem::offset_of!(mesh::Vertex, uv) as u64,
                        shader_location: 0,
                    }],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &padding,
                entry_point: Some("fs_islands"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::R32Uint,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let jump = pipeline(&padding, "jump");

        let transfer_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("transfer"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/transfer.wgsl").into()),
        });
        let transfer = [COLOR_FORMAT, MAT_FORMAT].map(|format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("paint transfer"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &transfer_module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: 16,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &transfer_module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        });

        let resolve_params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resolve params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&resolve_params, 0, bytemuck::cast_slice(&[NORMAL_STRENGTH, 0.0, 0.0, 0.0]));

        let params_capacity = 16;
        Self {
            params: Self::params_buffer(device, params_capacity),
            params_capacity,
            blend,
            resolve,
            mips,
            accum,
            resolve_params,
            white,
            empty_color,
            empty_mat,
            no_padding,
            islands,
            jump,
            transfer,
        }
    }

    fn params_buffer(device: &wgpu::Device, layers: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("layer params"),
            size: layers * PARAM_STRIDE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// A texture set seeded from what the model file said: a fill layer with the material's
    /// colour, roughness and metalness, and the base colour texture as a paint layer above it.
    pub fn new_set(&self, device: &wgpu::Device, queue: &wgpu::Queue, material: &mesh::Material) -> TextureSet {
        let c = material.base_color;
        let base = Fill {
            colour: Color32::from_rgb(linear_to_srgb8(c[0]), linear_to_srgb8(c[1]), linear_to_srgb8(c[2])),
            rough: material.roughness.clamp(0.0, 1.0),
            metal: material.metallic.clamp(0.0, 1.0),
            height: 0.5,
            channels: Channels::ALL,
        };
        let mut layers = vec![Layer::fill("Base material", base)];
        if let Some(image) = &material.base_color_texture {
            let mut layer = Layer::paint(device, "Imported texture");
            let (rgba, swatch) = prepare_import(image, material.base_color);
            let pixels = layer.pixels.as_ref().expect("paint layer has pixels");
            write_full(queue, &pixels.color, &rgba, 4);
            layer.swatch = Some(swatch);
            layers.push(layer);
        }
        let selected = layers.len() - 1;
        self.set_with(device, material.name.clone(), layers, selected)
    }

    /// A texture set with these layers (bottom first), as a project file reopens it.
    pub fn set_with(&self, device: &wgpu::Device, name: String, layers: Vec<Layer>, selected: usize) -> TextureSet {
        let selected = selected.min(layers.len().saturating_sub(1));
        TextureSet {
            name,
            committed: layers.clone(),
            layers,
            selected,
            dirty: true,
            composited: None,
            outputs: self.outputs(device),
            padding: None,
        }
    }

    fn outputs(&self, device: &wgpu::Device) -> Outputs {
        let levels = 32 - SIZE.leading_zeros();
        let make = |label, format, view_formats: &'static [wgpu::TextureFormat]| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
                mip_level_count: levels,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats,
            })
        };
        let color = make("set colour", COLOR_FORMAT, &[wgpu::TextureFormat::Rgba8UnormSrgb]);
        let mat = make("set material", MAT_FORMAT, &[]);
        let normal = make("set normal", COLOR_FORMAT, &[]);
        let level = |t: &wgpu::Texture, l: u32| {
            t.create_view(&wgpu::TextureViewDescriptor { base_mip_level: l, mip_level_count: Some(1), ..Default::default() })
        };

        let mut mip_passes = Vec::new();
        for (kind, tex) in [(0usize, &color), (1, &mat), (2, &normal)] {
            for l in 1..levels {
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("mip"),
                    layout: &self.mips[kind].get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&level(tex, l - 1)) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&level(tex, l)) },
                    ],
                });
                mip_passes.push((kind, bg, (SIZE >> l).max(1)));
            }
        }

        Outputs {
            color_srgb_view: color.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
                // sRGB formats can't be storage; this view is only ever sampled.
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                ..Default::default()
            }),
            mat_view: mat.create_view(&Default::default()),
            normal_view: normal.create_view(&Default::default()),
            color_level0: level(&color, 0),
            mat_level0: level(&mat, 0),
            normal_level0: level(&normal, 0),
            mip_passes,
            color,
            mat,
            normal,
        }
    }

    /// Work out where the set's UV islands are, so the maps get padded past their edges and
    /// filtering never pulls in colour from outside an island (the seam lines you'd otherwise
    /// see). Draws the set's triangles in UV space, then a jump flood finds, for every texel,
    /// the nearest texel inside an island.
    pub fn set_islands(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        set: &mut TextureSet,
        vertices: &wgpu::Buffer,
        indices: &wgpu::Buffer,
        ranges: &[std::ops::Range<u32>],
    ) {
        let make = |label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Uint,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default())
        };
        let ping = [make("padding a"), make("padding b")];
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("uv islands") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("uv islands"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &ping[0],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: NO_SEED as f64, g: 0.0, b: 0.0, a: 0.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.islands);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            for r in ranges {
                pass.draw_indexed(r.clone(), 0, 0..1);
            }
        }
        // Steps SIZE/2, SIZE/4 .. 1, then 1 again to mop up the flood's rare misses.
        let mut steps = Vec::new();
        let mut k = SIZE / 2;
        while k >= 1 {
            steps.push(k);
            k /= 2;
        }
        steps.push(1);
        let layout = self.jump.get_bind_group_layout(0);
        let mut cur = 0;
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("jump flood"), timestamp_writes: None });
            pass.set_pipeline(&self.jump);
            for step in steps {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("jump step"),
                    size: 16,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(&buf, 0, bytemuck::cast_slice(&[step, 0, 0, 0]));
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("jump flood"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: view(&ping[cur]) },
                        wgpu::BindGroupEntry { binding: 2, resource: view(&ping[1 - cur]) },
                    ],
                });
                pass.set_bind_group(0, &bg, &[]);
                pass.dispatch_workgroups(SIZE.div_ceil(8), SIZE.div_ceil(8), 1);
                cur = 1 - cur;
            }
        }
        queue.submit([enc.finish()]);
        let [a, b] = ping;
        set.padding = Some(if cur == 0 { a } else { b });
        set.dirty = true;
    }

    /// The set's stack with every paint layer's pixels and every mask moved to a new UV
    /// layout. `uvs` holds, per vertex, the new UV then the old one; `indices` and `ranges`
    /// pick the set's triangles. Call it before the set's islands are worked out for the new
    /// layout: it reads the old ones. Fill layers carry over as they are, and the old
    /// textures are untouched, so the old stack still works for undo.
    pub fn transfer(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        set: &TextureSet,
        uvs: &wgpu::Buffer,
        indices: &wgpu::Buffer,
        ranges: &[std::ops::Range<u32>],
    ) -> Vec<Layer> {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("paint transfer") });
        let mut copy = |src: &wgpu::Texture, dst: &wgpu::Texture| {
            let pipeline = &self.transfer[usize::from(src.format() == MAT_FORMAT)];
            let src_view = src.create_view(&Default::default());
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("transfer source"),
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: view(&src_view) },
                    wgpu::BindGroupEntry { binding: 1, resource: view(set.padding.as_ref().unwrap_or(&self.no_padding)) },
                ],
            });
            let dst_view = dst.create_view(&Default::default());
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("paint transfer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_vertex_buffer(0, uvs.slice(..));
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            for r in ranges {
                pass.draw_indexed(r.clone(), 0, 0..1);
            }
        };
        let moved = set
            .layers
            .iter()
            .map(|layer| {
                let mut out = layer.clone();
                if let Some(src) = &layer.pixels {
                    let dst = PaintPixels::new(device);
                    for ((s, _), (d, _)) in src.textures().into_iter().zip(dst.textures()) {
                        copy(s, d);
                    }
                    out.pixels = Some(Rc::new(dst));
                }
                if let Some(src) = &layer.mask {
                    let texture = layer_texture(device, "layer mask", MASK_FORMAT, SIZE);
                    copy(&src.texture, &texture);
                    out.mask = Some(Rc::new(Mask { view: texture.create_view(&Default::default()), texture }));
                }
                out
            })
            .collect();
        queue.submit([enc.finish()]);
        moved
    }

    /// Flatten the set's stack into its maps if anything changed since last time.
    /// Returns whether it did any work.
    pub fn update(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, set: &mut TextureSet) -> bool {
        if !set.needs_composite() {
            return false;
        }
        let visible: Vec<&Layer> = set.layers.iter().filter(|l| l.visible && l.opacity > 0.0).collect();

        let mut params = vec![LayerParams::default()]; // kind 0: clear
        for layer in &visible {
            let mut p = LayerParams {
                mode: layer.blend as u32,
                kind: 1,
                opacity: layer.opacity.clamp(0.0, 1.0),
                mask_invert: layer.mask_invert as u32,
                ..Default::default()
            };
            if let LayerKind::Fill(f) = &layer.kind {
                p.kind = 2;
                p.fill_color = [f.colour.r() as f32 / 255.0, f.colour.g() as f32 / 255.0, f.colour.b() as f32 / 255.0, 1.0];
                p.fill_mat = [f.rough, f.metal, f.height, 0.0];
                p.channels = f.channels.as_vec4();
            }
            params.push(p);
        }
        if params.len() as u64 > self.params_capacity {
            self.params_capacity = (params.len() as u64).next_power_of_two();
            self.params = Self::params_buffer(device, self.params_capacity);
        }
        let mut bytes = vec![0u8; params.len() * PARAM_STRIDE as usize];
        for (i, p) in params.iter().enumerate() {
            let at = i * PARAM_STRIDE as usize;
            bytes[at..at + size_of::<LayerParams>()].copy_from_slice(bytemuck::bytes_of(p));
        }
        queue.write_buffer(&self.params, 0, &bytes);

        let layout = self.blend.get_bind_group_layout(0);
        let groups: Vec<wgpu::BindGroup> = (0..params.len())
            .map(|i| {
                let (src, dst) = (&self.accum[(i + 1) % 2], &self.accum[i % 2]);
                let layer = i.checked_sub(1).map(|j| visible[j]);
                let pixels = layer.and_then(|l| l.pixels.as_deref());
                let mask = layer.and_then(|l| l.mask.as_deref()).map(|m| &m.view).unwrap_or(&self.white);
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("blend layer"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &self.params,
                                offset: i as u64 * PARAM_STRIDE,
                                size: wgpu::BufferSize::new(size_of::<LayerParams>() as u64),
                            }),
                        },
                        wgpu::BindGroupEntry { binding: 1, resource: view(&src.0) },
                        wgpu::BindGroupEntry { binding: 2, resource: view(&src.1) },
                        wgpu::BindGroupEntry { binding: 3, resource: view(&dst.0) },
                        wgpu::BindGroupEntry { binding: 4, resource: view(&dst.1) },
                        wgpu::BindGroupEntry { binding: 5, resource: view(pixels.map(|p| &p.color_view).unwrap_or(&self.empty_color)) },
                        wgpu::BindGroupEntry { binding: 6, resource: view(pixels.map(|p| &p.mat_view).unwrap_or(&self.empty_mat)) },
                        wgpu::BindGroupEntry { binding: 7, resource: view(pixels.map(|p| &p.cov_view).unwrap_or(&self.empty_color)) },
                        wgpu::BindGroupEntry { binding: 8, resource: view(mask) },
                    ],
                })
            })
            .collect();

        let last = &self.accum[(params.len() - 1) % 2];
        let out = &set.outputs;
        let resolve_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("resolve"),
            layout: &self.resolve.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 1, resource: view(&last.0) },
                wgpu::BindGroupEntry { binding: 2, resource: view(&last.1) },
                wgpu::BindGroupEntry { binding: 9, resource: self.resolve_params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 10, resource: view(&out.color_level0) },
                wgpu::BindGroupEntry { binding: 11, resource: view(&out.mat_level0) },
                wgpu::BindGroupEntry { binding: 12, resource: view(&out.normal_level0) },
                wgpu::BindGroupEntry { binding: 13, resource: view(set.padding.as_ref().unwrap_or(&self.no_padding)) },
            ],
        });

        let groups_of = |n: u32| n.div_ceil(8);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("composite") });
        let timer = crate::stats::timestamps("Compositing layers");
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("composite"),
                timestamp_writes: timer.as_ref().map(crate::stats::Slot::compute),
            });
            pass.set_pipeline(&self.blend);
            for g in &groups {
                pass.set_bind_group(0, g, &[]);
                pass.dispatch_workgroups(groups_of(SIZE), groups_of(SIZE), 1);
            }
            pass.set_pipeline(&self.resolve);
            pass.set_bind_group(0, &resolve_group, &[]);
            pass.dispatch_workgroups(groups_of(SIZE), groups_of(SIZE), 1);
            for (kind, bg, size) in &out.mip_passes {
                pass.set_pipeline(&self.mips[*kind]);
                pass.set_bind_group(0, bg, &[]);
                pass.dispatch_workgroups(groups_of(*size), groups_of(*size), 1);
            }
        }
        queue.submit([enc.finish()]);

        set.composited = Some(set.layers.clone());
        set.dirty = false;
        true
    }
}

/// 2x2 box downsample into the next mip level. sRGB colour averages in linear light and
/// normals are renormalised.
fn mip_shader(format: &str, mode: &str) -> String {
    let (decode, encode) = match mode {
        "srgb" => (
            "vec4<f32>(select(pow((c.rgb + 0.055) / 1.055, vec3<f32>(2.4)), c.rgb / 12.92, c.rgb <= vec3<f32>(0.04045)), c.a)",
            "vec4<f32>(select(1.055 * pow(s.rgb, vec3<f32>(1.0 / 2.4)) - 0.055, s.rgb * 12.92, s.rgb <= vec3<f32>(0.0031308)), s.a)",
        ),
        "normal" => ("c", "vec4<f32>(normalize(s.xyz * 2.0 - vec3<f32>(1.0)) * 0.5 + vec3<f32>(0.5), 1.0)"),
        _ => ("c", "s"),
    };
    format!(
        r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<{format}, write>;

fn load(p: vec2<u32>, sd: vec2<u32>) -> vec4<f32> {{
    let c = textureLoad(src, vec2<i32>(min(p, sd - vec2<u32>(1u))), 0);
    return {decode};
}}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {{
    let d = textureDimensions(dst);
    if (gid.x >= d.x || gid.y >= d.y) {{
        return;
    }}
    let sd = textureDimensions(src);
    let b = gid.xy * 2u;
    let s = (load(b, sd) + load(b + vec2<u32>(1u, 0u), sd) + load(b + vec2<u32>(0u, 1u), sd) + load(b + vec2<u32>(1u, 1u), sd)) * 0.25;
    textureStore(dst, vec2<i32>(gid.xy), {encode});
}}
"#
    )
}

// ---------------------------------------------------------------- tiled undo snapshots

/// Copies of some tiles of a paint layer, taken before a change so it can be undone.
pub struct TileSnapshot {
    /// The textures the tiles belong to, with bytes per texel.
    targets: Vec<(wgpu::Texture, u32)>,
    tiles: Vec<[u32; 2]>,
    /// One buffer per target texture, tiles packed one after another.
    buffers: Vec<wgpu::Buffer>,
}

impl TileSnapshot {
    /// Save `tiles` (tile coordinates, not texels) of `targets`.
    #[cfg(test)]
    pub fn capture(device: &wgpu::Device, queue: &wgpu::Queue, targets: Vec<(wgpu::Texture, u32)>, tiles: Vec<[u32; 2]>) -> Self {
        let sources: Vec<wgpu::Texture> = targets.iter().map(|(t, _)| t.clone()).collect();
        Self::capture_from(device, queue, &sources.iter().collect::<Vec<_>>(), targets, tiles)
    }

    /// Save `tiles` of `sources` as the earlier state of `targets` (same formats, one each).
    /// Strokes use this to save the copy they took before painting.
    pub fn capture_from(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        sources: &[&wgpu::Texture],
        targets: Vec<(wgpu::Texture, u32)>,
        tiles: Vec<[u32; 2]>,
    ) -> Self {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("tile capture") });
        let snap = Self::encode_capture(device, &mut enc, sources, targets, tiles);
        queue.submit([enc.finish()]);
        snap
    }

    fn encode_capture(
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        sources: &[&wgpu::Texture],
        targets: Vec<(wgpu::Texture, u32)>,
        tiles: Vec<[u32; 2]>,
    ) -> Self {
        let buffers = sources
            .iter()
            .zip(&targets)
            .map(|(tex, (_, bpp))| {
                let (tex, bpp) = (*tex, *bpp);
                let tile_bytes = (TILE * TILE * bpp) as u64;
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("undo tiles"),
                    size: tile_bytes * tiles.len() as u64,
                    usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                for (k, t) in tiles.iter().enumerate() {
                    enc.copy_texture_to_buffer(tile_copy(tex, *t), tile_buffer(&buffer, k as u64 * tile_bytes, bpp), tile_extent());
                }
                buffer
            })
            .collect();
        Self { targets, tiles, buffers }
    }

    /// Put the saved tiles back, and return a snapshot of what they replaced (for redo).
    pub fn restore(self, device: &wgpu::Device, queue: &wgpu::Queue) -> TileSnapshot {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("tile restore") });
        let sources: Vec<wgpu::Texture> = self.targets.iter().map(|(t, _)| t.clone()).collect();
        let current = Self::encode_capture(device, &mut enc, &sources.iter().collect::<Vec<_>>(), self.targets.clone(), self.tiles.clone());
        for ((tex, bpp), buffer) in self.targets.iter().zip(&self.buffers) {
            let tile_bytes = (TILE * TILE * bpp) as u64;
            for (k, t) in self.tiles.iter().enumerate() {
                enc.copy_buffer_to_texture(tile_buffer(buffer, k as u64 * tile_bytes, *bpp), tile_copy(tex, *t), tile_extent());
            }
        }
        queue.submit([enc.finish()]);
        current
    }

    pub fn bytes(&self) -> u64 {
        self.buffers.iter().map(|b| b.size()).sum()
    }
}

/// Every tile touched by the texel rectangle `min..max` (exclusive), clamped to the set.
#[cfg(test)]
pub fn tiles_in_rect(min: [u32; 2], max: [u32; 2]) -> Vec<[u32; 2]> {
    let n = SIZE / TILE;
    let (x0, y0) = ((min[0] / TILE).min(n), (min[1] / TILE).min(n));
    let (x1, y1) = (max[0].div_ceil(TILE).min(n), max[1].div_ceil(TILE).min(n));
    (y0..y1).flat_map(|y| (x0..x1).map(move |x| [x, y])).collect()
}

fn tile_copy(tex: &wgpu::Texture, t: [u32; 2]) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture: tex,
        mip_level: 0,
        origin: wgpu::Origin3d { x: t[0] * TILE, y: t[1] * TILE, z: 0 },
        aspect: wgpu::TextureAspect::All,
    }
}

fn tile_buffer(buffer: &wgpu::Buffer, offset: u64, bpp: u32) -> wgpu::TexelCopyBufferInfo<'_> {
    wgpu::TexelCopyBufferInfo {
        buffer,
        layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(TILE * bpp), rows_per_image: Some(TILE) },
    }
}

fn tile_extent() -> wgpu::Extent3d {
    wgpu::Extent3d { width: TILE, height: TILE, depth_or_array_layers: 1 }
}

// ---------------------------------------------------------------- helpers

fn view(v: &wgpu::TextureView) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(v)
}

fn layer_texture(device: &wgpu::Device, label: &str, format: wgpu::TextureFormat, size: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_full(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], bpp: u32) {
    let size = texture.size();
    queue.write_texture(
        texture.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size.width * bpp), rows_per_image: Some(size.height) },
        size,
    );
}

/// The imported base colour texture, resized to the set and tinted by the material's colour
/// factor, with full coverage. Also returns its average colour for the layer thumbnail.
fn prepare_import(image: &mesh::Image, factor: [f32; 4]) -> (Vec<u8>, Color32) {
    let src = image::RgbaImage::from_raw(image.width, image.height, image.rgba.clone()).expect("image size matches its pixels");
    let mut rgba = if image.width == SIZE && image.height == SIZE {
        src.into_raw()
    } else {
        image::imageops::resize(&src, SIZE, SIZE, image::imageops::FilterType::Triangle).into_raw()
    };
    let tinted = factor[..3].iter().any(|&f| (f - 1.0).abs() > 1e-4);
    let to_linear: Vec<f32> = (0..256).map(|v| srgb8_to_linear(v as u8)).collect();
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        if tinted {
            for c in 0..3 {
                px[c] = linear_to_srgb8(to_linear[px[c] as usize] * factor[c]);
            }
        }
        px[3] = 255;
        if i % 61 == 0 {
            for c in 0..3 {
                sum[c] += px[c] as u64;
            }
            n += 1;
        }
    }
    let n = n.max(1);
    (rgba, Color32::from_rgb((sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8))
}

/// Read one texel of mip 0 back as bytes.
pub fn read_texel(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture, x: u32, y: u32, bpp: u32) -> Vec<u8> {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) } },
        wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    buffer.slice(..).get_mapped_range().expect("mapped")[..bpp as usize].to_vec()
}

/// Read a whole mip level back as tightly packed rows. Returns its width (the maps are square).
pub fn read_level(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture, level: u32, bpp: u32) -> (u32, Vec<u8>) {
    let size = (tex.width() >> level).max(1);
    let row = size * bpp;
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("read level"),
        size: padded as u64 * size as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: level, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(size) },
        },
        wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity((row * size) as usize);
    for y in 0..size as usize {
        out.extend_from_slice(&mapped[y * padded as usize..y * padded as usize + row as usize]);
    }
    (size, out)
}

/// A half float's bits as f32.
pub fn f16_to_f32(lo: u8, hi: u8) -> f32 {
    half::f16::from_le_bytes([lo, hi]).to_f32()
}

pub fn srgb8_to_linear(v: u8) -> f32 {
    let v = v as f32 / 255.0;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) use super::read_texel;

    pub(crate) fn gpu() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).expect("a GPU adapter");
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("a GPU device")
    }

    fn fill(colour: Color32) -> Fill {
        Fill { colour, rough: 0.25, metal: 1.0, height: 0.5, channels: Channels::ALL }
    }

    #[test]
    fn transfer_moves_paint_to_new_uvs() {
        use wgpu::util::DeviceExt;
        let (device, queue) = gpu();
        let comp = Compositor::new(&device, &queue);
        // Red on the left half, nothing on the right.
        let layer = Layer::paint(&device, "p");
        let half: Vec<u8> = (0..SIZE * SIZE).flat_map(|i| if i % SIZE < SIZE / 2 { [255, 0, 0, 255] } else { [0; 4] }).collect();
        write_full(&queue, &layer.pixels.as_ref().unwrap().color, &half, 4);
        // A quad that used to sit on the left half and now sits on the right half.
        let corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let pairs: Vec<[f32; 4]> = corners.iter().map(|c| [0.5 + c[0] * 0.5, c[1], c[0] * 0.5, c[1]]).collect();
        let uvs = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&pairs),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytemuck::cast_slice(&[0u32, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });
        let fill = Layer::fill("f", fill(Color32::WHITE));
        let mut set = comp.new_set(&device, &queue, &mesh::Material::named("t"));
        set.layers = vec![fill.clone(), layer.clone()];
        let moved = comp.transfer(&device, &queue, &set, &uvs, &indices, &[0..6]);
        assert!(moved[0].same(&fill), "fill layers carry over as they are");
        let color = &moved[1].pixels.as_ref().unwrap().color;
        assert_eq!(read_texel(&device, &queue, color, SIZE * 3 / 4, SIZE / 2, 4), vec![255, 0, 0, 255]);
        assert_eq!(read_texel(&device, &queue, color, SIZE / 4, SIZE / 2, 4), vec![0; 4]);
        // The old layer still holds the old layout, for undo.
        let old = &layer.pixels.as_ref().unwrap().color;
        assert_eq!(read_texel(&device, &queue, old, SIZE / 4, SIZE / 2, 4), vec![255, 0, 0, 255]);
    }

    #[test]
    fn fill_layers_blend() {
        let (device, queue) = gpu();
        let mut comp = Compositor::new(&device, &queue);
        let mut set = comp.new_set(&device, &queue, &mesh::Material::named("Test"));
        set.layers = vec![Layer::fill("a", fill(Color32::from_rgb(200, 100, 50)))];
        assert!(comp.update(&device, &queue, &mut set));
        assert!(!comp.update(&device, &queue, &mut set), "unchanged stack recomposites");
        let color = set.output_textures()[0].clone();
        assert_eq!(read_texel(&device, &queue, &color, 10, 10, 4), vec![200, 100, 50, 255]);

        // Multiply by mid grey at half opacity: 200 -> mix(200, 200 * 128/255, 0.5) ~ 150.
        let mut top = Layer::fill("b", fill(Color32::from_gray(128)));
        top.blend = BlendMode::Multiply;
        top.opacity = 0.5;
        set.layers.push(top);
        assert!(comp.update(&device, &queue, &mut set));
        let px = read_texel(&device, &queue, &color, 10, 10, 4);
        assert!((px[0] as i32 - 150).abs() <= 1, "{px:?}");

        // Hidden layers drop out.
        set.layers[1].visible = false;
        comp.update(&device, &queue, &mut set);
        assert_eq!(read_texel(&device, &queue, &color, 10, 10, 4)[0], 200);
    }

    #[test]
    fn paint_layer_coverage_per_channel() {
        let (device, queue) = gpu();
        let mut comp = Compositor::new(&device, &queue);
        let mut set = comp.new_set(&device, &queue, &mesh::Material::named("Test"));
        let paint = Layer::paint(&device, "p");
        let px = paint.pixels.clone().unwrap();
        // Colour fully covered red; material covered only for roughness (value 1.0).
        let red: Vec<u8> = [255u8, 0, 0, 255].repeat((SIZE * SIZE) as usize);
        write_full(&queue, &px.color, &red, 4);
        let one = half::f16::from_f32(1.0).to_bits().to_le_bytes();
        let mat: Vec<u8> = [one, one, one, one].concat().repeat((SIZE * SIZE) as usize);
        write_full(&queue, &px.mat, &mat, 8);
        write_full(&queue, &px.cov, &[255u8, 0, 0, 0].repeat((SIZE * SIZE) as usize), 4);
        set.layers = vec![Layer::fill("base", fill(Color32::from_gray(10))), paint];
        comp.update(&device, &queue, &mut set);
        let [color, mat_out, _] = set.output_textures();
        assert_eq!(read_texel(&device, &queue, color, 5, 5, 4), vec![255, 0, 0, 255]);
        let m = read_texel(&device, &queue, mat_out, 5, 5, 8);
        let f = |i: usize| half::f16::from_le_bytes([m[i * 2], m[i * 2 + 1]]).to_f32();
        assert_eq!((f(0), f(1), f(2)), (1.0, 1.0, 0.5), "roughness painted, metal and height untouched");
    }

    #[test]
    fn tile_snapshots_undo_and_redo() {
        let (device, queue) = gpu();
        let pixels = Rc::new(PaintPixels::new(&device));
        let tile = [1u32, 2u32];
        let tiles = tiles_in_rect([TILE + 10, 2 * TILE + 10], [TILE + 20, 2 * TILE + 20]);
        assert_eq!(tiles, vec![tile]);
        assert_eq!(tiles_in_rect([TILE - 1, 0], [TILE + 1, 1]).len(), 2);

        let write_tile = |value: u8| {
            queue.write_texture(
                tile_copy(&pixels.color, tile),
                &vec![value; (TILE * TILE * 4) as usize],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(TILE * 4), rows_per_image: Some(TILE) },
                tile_extent(),
            );
        };
        let at = |x, y| read_texel(&device, &queue, &pixels.color, x, y, 4)[0];
        write_tile(40);
        let before = TileSnapshot::capture(&device, &queue, pixels.targets(), tiles);
        write_tile(200);
        assert_eq!(at(TILE + 5, 2 * TILE + 5), 200);
        let redo = before.restore(&device, &queue);
        assert_eq!(at(TILE + 5, 2 * TILE + 5), 40, "undo puts the old tile back");
        assert_eq!(at(0, 0), 0, "other tiles untouched");
        redo.restore(&device, &queue);
        assert_eq!(at(TILE + 5, 2 * TILE + 5), 200, "redo puts the change back");
    }
}
