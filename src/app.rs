//! Application state and the Graphite shell: top bar, full-bleed viewport, floating panels.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};

use eframe::egui_wgpu;
use eframe::egui_wgpu::wgpu::util::DeviceExt;
use egui::{Align, Align2, Color32, CornerRadius, Layout, Margin, Order, Pos2, Rect, Sense, Stroke, UiBuilder, Vec2};

use crate::brush::{self, BrushEngine, Dab};
use crate::camera::OrbitCamera;
use crate::export;
use glam::Vec3;
use crate::history::{Applied, Change, History};
use crate::mesh::{self, Mesh};
use crate::pen::{PenInput, PressureCurve};
use crate::renderer::{FrameParams, ViewMode, ViewportRenderer};
use crate::texset::{self, BlendMode, Channels, Compositor, Fill, Layer, LayerKind, Mask, TextureSet};
use crate::theme::{self, *};
use crate::unwrap::{self, Health};
use crate::{icons, widgets};

/// Overlapping texels on the UV coverage map, and numbers worth a second look.
const WARN: Color32 = Color32::from_rgb(0xe0, 0x7d, 0x62);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Workspace {
    Paint,
    Uv,
    Materials,
    Export,
}

/// How the UV panel lays a model out. Each maps to an [`unwrap::Mode`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnwrapMethod {
    Auto,
    Planar,
    Box,
    Cylinder,
    Sphere,
}

impl UnwrapMethod {
    const ALL: [(UnwrapMethod, &'static str); 5] = [
        (UnwrapMethod::Auto, "Auto"),
        (UnwrapMethod::Planar, "Planar"),
        (UnwrapMethod::Box, "Box"),
        (UnwrapMethod::Cylinder, "Cylinder"),
        (UnwrapMethod::Sphere, "Sphere"),
    ];

    fn about(self) -> &'static str {
        match self {
            UnwrapMethod::Auto => "Cuts seams where they show least and lays the pieces out. Right for most models.",
            UnwrapMethod::Planar => "Flat from one side, like a photo. The back gets its own island; faces seen edge-on get squeezed.",
            UnwrapMethod::Box => "Each face flat from the side it faces most. Good for hard surfaces, crates and buildings.",
            UnwrapMethod::Cylinder => "Wrapped around an axis and unrolled, with flat caps. Good for limbs, bottles, pillars and trunks.",
            UnwrapMethod::Sphere => "Wrapped like a globe. Good for heads, rocks and fruit; squeezes near the poles.",
        }
    }
}

/// Which way a projection looks or turns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnwrapAxis {
    /// Along the camera, as the viewport shows the model now.
    View,
    X,
    Y,
    Z,
}

impl UnwrapAxis {
    fn vector(self) -> Vec3 {
        match self {
            UnwrapAxis::X => Vec3::X,
            UnwrapAxis::Y | UnwrapAxis::View => Vec3::Y,
            UnwrapAxis::Z => Vec3::Z,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Brush,
    Eraser,
    Smudge,
    Fill,
    Clone,
    Decal,
    Mask,
    Picker,
}

impl Tool {
    const ALL: [Tool; 8] =
        [Tool::Brush, Tool::Eraser, Tool::Smudge, Tool::Fill, Tool::Clone, Tool::Decal, Tool::Mask, Tool::Picker];

    fn icon(self) -> char {
        match self {
            Tool::Brush => icons::BRUSH,
            Tool::Eraser => icons::ERASER,
            Tool::Smudge => icons::SMUDGE,
            Tool::Fill => icons::FILL,
            Tool::Clone => icons::CLONE,
            Tool::Decal => icons::DECAL,
            Tool::Mask => icons::MASK,
            Tool::Picker => icons::PICKER,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Tool::Brush => "Brush",
            Tool::Eraser => "Eraser",
            Tool::Smudge => "Smudge",
            Tool::Fill => "Fill",
            Tool::Clone => "Clone stamp",
            Tool::Decal => "Decal",
            Tool::Mask => "Mask",
            Tool::Picker => "Colour picker",
        }
    }

    fn key(self) -> egui::Key {
        use egui::Key;
        match self {
            Tool::Brush => Key::B,
            Tool::Eraser => Key::E,
            Tool::Smudge => Key::S,
            Tool::Fill => Key::G,
            Tool::Clone => Key::C,
            Tool::Decal => Key::D,
            Tool::Mask => Key::M,
            Tool::Picker => Key::I,
        }
    }

    fn shows_brush_cursor(self) -> bool {
        !matches!(self, Tool::Fill | Tool::Picker)
    }

    /// Tools that paint strokes.
    fn strokes(self) -> bool {
        matches!(self, Tool::Brush | Tool::Eraser | Tool::Mask)
    }

    /// Tools on the rail that don't work yet.
    fn later(self) -> bool {
        matches!(self, Tool::Smudge | Tool::Clone | Tool::Decal)
    }
}

/// Dabs sit this fraction of the brush's diameter apart.
const SPACING: f32 = 0.1;

struct BrushSettings {
    size: f32,
    opacity: f32,
    flow: f32,
    /// Fraction of the radius painted at full strength before the edge fades.
    hardness: f32,
    /// The material values the brush paints, alongside the colour.
    rough: f32,
    metal: f32,
    /// 0.5 is flat.
    height: f32,
    pen_size: bool,
    pen_opacity: bool,
    pen_tilt: bool,
    curve: PressureCurve,
    paint_colour: bool,
    paint_rough: bool,
    paint_metal: bool,
    paint_height: bool,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            size: 64.0,
            opacity: 0.85,
            flow: 1.0,
            hardness: 0.6,
            rough: 0.5,
            metal: 0.0,
            height: 0.7,
            pen_size: true,
            pen_opacity: true,
            pen_tilt: false,
            curve: PressureCurve::default(),
            paint_colour: true,
            paint_rough: true,
            paint_metal: false,
            paint_height: false,
        }
    }
}

/// A stroke in progress.
struct StrokeState {
    set: usize,
    /// The stroke paints the layer's mask rather than its pixels.
    mask: bool,
    paint: brush::Paint,
    /// Where the path carries on from, in points.
    last: Pos2,
    /// Distance along the path to the next dab, in points.
    next: f32,
    /// Pen pressure (after the curve) at `last`.
    pressure: f32,
}

/// An unwrap or repack running on a worker thread.
struct UnwrapJob {
    mode: unwrap::Mode,
    progress: unwrap::Progress,
    result: mpsc::Receiver<anyhow::Result<unwrap::Unwrapped>>,
    /// The mesh it started from; the result is dropped if the mesh changed since.
    generation: u64,
}

/// An export writing files on a worker thread.
struct ExportJob {
    plan: export::Plan,
    done: Arc<AtomicU32>,
    result: mpsc::Receiver<anyhow::Result<()>>,
}

enum LayerAction {
    Up,
    Down,
    Duplicate,
    Delete,
}

pub struct UmberApp {
    rs: egui_wgpu::RenderState,
    renderer: ViewportRenderer,
    camera: OrbitCamera,
    mesh: Option<Mesh>,
    toast: Option<(String, f64)>,
    /// Seconds since start, as of this frame.
    now: f64,

    workspace: Workspace,
    tool: Tool,
    brush: BrushSettings,
    pen: PenInput,
    pen_open: bool,
    primary: Color32,
    secondary: Color32,
    colour_open: bool,

    compositor: Compositor,
    brush_engine: BrushEngine,
    stroke: Option<StrokeState>,
    /// UV island of every triangle, worked out the first time the fill tool needs it.
    islands: Option<Vec<u32>>,
    /// One texture set per mesh material.
    sets: Vec<TextureSet>,
    /// The set the layers panel shows and the brush paints.
    active_set: usize,
    history: History,
    /// Bumped whenever the mesh is replaced (open, unwrap, or undoing one).
    mesh_generation: u64,
    unwrap_job: Option<UnwrapJob>,
    /// UV health of one set, with its coverage map, worked out when the UV panel shows it.
    uv_health: Option<(usize, Health, egui::TextureHandle)>,
    /// "Not now" on the offer to unwrap a model that has no UVs.
    no_uv_offer_dismissed: bool,
    /// The UV panel's last choices, kept for the next unwrap.
    unwrap_method: UnwrapMethod,
    planar_axis: UnwrapAxis,
    /// Shared by cylinder and sphere.
    round_axis: UnwrapAxis,

    view_mode: ViewMode,
    light_yaw: f32,
    exposure: f32,

    /// Where the open model came from, so the export folder picker can start beside it.
    model_path: Option<PathBuf>,
    /// Asked for on the first export, then reused, so exporting again is one click.
    export_dir: Option<PathBuf>,
    /// One of [`export::SIZES`].
    export_size: u32,
    export_job: Option<ExportJob>,
    /// What the last export wrote, and when.
    last_export: Option<(String, f64)>,
}

impl UmberApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        theme::install(&cc.egui_ctx);
        let rs = cc.wgpu_render_state.clone().expect("Umber needs the wgpu renderer");
        let info = rs.adapter.get_info();
        log::info!("GPU: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
        let renderer = ViewportRenderer::new(&rs);
        let compositor = Compositor::new(&rs.device, &rs.queue);
        let brush_engine = BrushEngine::new(&rs.device);
        let mut app = Self {
            rs,
            renderer,
            camera: OrbitCamera::default(),
            mesh: None,
            toast: None,
            now: 0.0,
            workspace: Workspace::Paint,
            tool: Tool::Brush,
            brush: BrushSettings::default(),
            pen: PenInput::start(cc.egui_ctx.clone()),
            pen_open: false,
            primary: Color32::from_rgb(0x3b, 0x2a, 0x1a),
            secondary: Color32::from_rgb(0xe8, 0xcf, 0xa4),
            colour_open: false,
            compositor,
            brush_engine,
            stroke: None,
            islands: None,
            sets: Vec::new(),
            active_set: 0,
            history: History::default(),
            mesh_generation: 0,
            unwrap_job: None,
            uv_health: None,
            no_uv_offer_dismissed: false,
            unwrap_method: UnwrapMethod::Auto,
            planar_axis: UnwrapAxis::View,
            round_axis: UnwrapAxis::Y,
            view_mode: ViewMode::Lit,
            light_yaw: 0.8,
            exposure: 1.0,
            model_path: None,
            // Scripted captures set this, since a folder picker would open on the real desktop.
            export_dir: std::env::var_os("UMBER_EXPORT_DIR").map(PathBuf::from),
            export_size: export::SIZES[0],
            export_job: None,
            last_export: None,
        };
        if let Some(path) = initial_file {
            app.open_path(&path, 0.0);
        }
        app
    }

    fn set_mesh(&mut self, mesh: Mesh) {
        if self.stroke.take().is_some() {
            self.brush_engine.end(&self.rs.device, &self.rs.queue);
        }
        if let Some(job) = self.unwrap_job.take() {
            job.progress.cancel();
        }
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        self.sets = mesh.materials.iter().map(|m| self.compositor.new_set(device, queue, m)).collect();
        self.active_set = 0;
        self.history.clear();
        let (center, radius) = mesh.bounding_sphere();
        self.camera = OrbitCamera::default();
        self.camera.frame(center, radius);
        if !mesh.has_uvs {
            self.view_mode = ViewMode::UvCheck;
        }
        self.no_uv_offer_dismissed = false;
        self.mesh = Some(mesh);
        self.mesh_to_gpu();
    }

    /// Upload the current mesh and work out each set's UV islands again.
    fn mesh_to_gpu(&mut self) {
        let Some(mesh) = &self.mesh else { return };
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        self.renderer.set_mesh(&self.rs, mesh, &self.sets);
        for (i, set) in self.sets.iter_mut().enumerate() {
            if let Some(draw) = self.renderer.mesh_draw(i) {
                self.compositor.set_islands(device, queue, set, draw.vertices, draw.indices, &draw.ranges);
            }
        }
        self.islands = None;
        self.uv_health = None;
        self.mesh_generation += 1;
    }

    // ---------------------------------------------------------------- unwrap

    fn start_unwrap(&mut self, mode: unwrap::Mode) {
        if self.unwrap_job.is_some() {
            return;
        }
        let Some(mesh) = self.mesh.clone() else { return };
        self.end_stroke();
        let progress = unwrap::Progress::default();
        let (send, result) = mpsc::channel();
        let worker = progress.clone();
        std::thread::spawn(move || {
            let _ = send.send(unwrap::run(&mesh, mode, &worker));
        });
        self.unwrap_job = Some(UnwrapJob { mode, progress, result, generation: self.mesh_generation });
    }

    /// The unwrap the UV panel's choices add up to, looking from the camera as it is now.
    fn chosen_unwrap(&self) -> unwrap::Mode {
        use unwrap::Projection;
        let round = self.round_axis.vector();
        let projection = match self.unwrap_method {
            UnwrapMethod::Auto => return unwrap::Mode::Unwrap,
            UnwrapMethod::Planar if self.planar_axis == UnwrapAxis::View => {
                let view = self.camera.view();
                let up = Vec3::new(view.x_axis.y, view.y_axis.y, view.z_axis.y);
                Projection::Planar { dir: (self.camera.eye() - self.camera.target).normalize_or(Vec3::Z), up }
            }
            UnwrapMethod::Planar => {
                let dir = self.planar_axis.vector();
                // Looking down from above, the front of the model is at the bottom.
                let up = if self.planar_axis == UnwrapAxis::Y { Vec3::NEG_Z } else { Vec3::Y };
                Projection::Planar { dir, up }
            }
            UnwrapMethod::Box => Projection::Box,
            UnwrapMethod::Cylinder => Projection::Cylinder { axis: round },
            UnwrapMethod::Sphere => Projection::Sphere { axis: round },
        };
        unwrap::Mode::Project(projection)
    }

    fn poll_unwrap(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.unwrap_job else { return };
        let outcome = match job.result.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("The unwrap stopped unexpectedly")),
        };
        let job = self.unwrap_job.take().expect("checked above");
        match outcome {
            Ok(done) if job.generation == self.mesh_generation => self.apply_unwrap(done, job.mode),
            Ok(_) => {}
            Err(_) if job.progress.is_cancelled() => {}
            Err(e) => {
                log::error!("{e:#}");
                self.toast = Some((format!("{e:#}"), self.now));
            }
        }
    }

    /// Swap in the new layout, carry every layer's paint over to it, and make the whole
    /// thing one undo step.
    fn apply_unwrap(&mut self, done: unwrap::Unwrapped, mode: unwrap::Mode) {
        self.end_stroke();
        self.history.record_stack_changes(&mut self.sets);
        let Some(old) = self.mesh.replace(done.mesh) else { return };
        let mesh = self.mesh.as_ref().expect("just set");
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        // Paint only moves when it had somewhere to be: without UVs every texel sat at 0,0.
        let carry = old.has_uvs;
        self.renderer.set_mesh(&self.rs, mesh, &self.sets);
        let pairs: Vec<[f32; 4]> =
            mesh.vertices.iter().zip(&done.old_uv).map(|(v, o)| [v.uv[0], v.uv[1], o[0], o[1]]).collect();
        let uvs = device.create_buffer_init(&egui_wgpu::wgpu::util::BufferInitDescriptor {
            label: Some("new and old uvs"),
            contents: bytemuck::cast_slice(&pairs),
            usage: egui_wgpu::wgpu::BufferUsages::VERTEX,
        });
        let mut stacks = Vec::with_capacity(self.sets.len());
        for (i, set) in self.sets.iter_mut().enumerate() {
            let moved = match (carry, self.renderer.mesh_draw(i)) {
                (true, Some(draw)) => self.compositor.transfer(device, queue, set, &uvs, draw.indices, &draw.ranges),
                _ => set.layers.clone(),
            };
            stacks.push((std::mem::replace(&mut set.layers, moved), set.selected));
            set.committed = set.layers.clone();
            set.dirty = true;
        }
        self.history.push(Change::Mesh { mesh: Box::new(old), stacks });
        self.mesh_to_gpu();
        let what = mode.done();
        let moved = if carry { " Paint moved with it; Ctrl+Z puts it back." } else { "" };
        self.toast = Some((format!("{what} {}.{moved}", self.mesh.as_ref().map_or("", |m| m.name.as_str())), self.now));
    }

    // ---------------------------------------------------------------- export

    fn export_plan(&self, dir: &Path) -> Option<export::Plan> {
        let mesh = self.mesh.as_ref()?;
        let names: Vec<&str> = mesh.materials.iter().map(|m| m.name.as_str()).collect();
        Some(export::Plan::new(dir, &mesh.name, &names))
    }

    /// The top bar's button and Ctrl+E: the first time it asks where, after that it just goes.
    fn export_to_godot(&mut self) {
        if self.export_job.is_some() {
            return;
        }
        let Some(mesh) = &self.mesh else {
            self.toast = Some(("Open a model first, then export it.".into(), self.now));
            return;
        };
        if !mesh.has_uvs {
            self.toast = Some(("This model has no UVs yet, so its maps wouldn't line up. Unwrap it first.".into(), self.now));
            self.workspace = Workspace::Uv;
            return;
        }
        let dir = match self.export_dir.clone() {
            Some(dir) => dir,
            None => match self.pick_export_dir() {
                Some(dir) => dir,
                None => return,
            },
        };
        self.start_export(dir);
    }

    fn pick_export_dir(&mut self) -> Option<PathBuf> {
        let start = self.export_dir.clone().or_else(|| self.model_path.as_ref().and_then(|p| p.parent().map(Path::to_owned)));
        let mut dialog = rfd::FileDialog::new().set_title("Export to which folder? One inside your Godot project works best");
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        let dir = dialog.pick_folder()?;
        self.export_dir = Some(dir.clone());
        Some(dir)
    }

    /// Read the maps back now (it takes a moment on the GPU), then encode and write them on a
    /// worker thread so the window keeps drawing.
    fn start_export(&mut self, dir: PathBuf) {
        let (Some(plan), Some(mesh)) = (self.export_plan(&dir), self.mesh.clone()) else { return };
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        for set in &mut self.sets {
            self.compositor.update(device, queue, set);
        }
        let maps: Vec<export::SetMaps> = self.sets.iter().map(|s| export::read_maps(device, queue, s, self.export_size)).collect();
        let done = Arc::new(AtomicU32::new(0));
        let (tx, rx) = mpsc::channel();
        let (job_plan, job_done) = (plan.clone(), done.clone());
        std::thread::spawn(move || {
            let _ = tx.send(export::write(&job_plan, &mesh, &maps, &job_done));
        });
        self.export_job = Some(ExportJob { plan, done, result: rx });
    }

    fn poll_export(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.export_job else { return };
        let outcome = match job.result.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("The export stopped unexpectedly")),
        };
        let job = self.export_job.take().expect("checked above");
        match outcome {
            Ok(()) => {
                let plan = &job.plan;
                let maps = plan.materials.len() * 3;
                let what = match plan.materials.len() {
                    1 => format!("{}, its material and {maps} maps", plan.glb),
                    n => format!("{}, {n} materials and {maps} maps", plan.glb),
                };
                let msg = match (&plan.godot, plan.res_dir()) {
                    (Some(g), Some(res)) => format!("Exported {what} to {res} in {}. Godot picks them up when you switch to it.", g.name),
                    _ => format!("Exported {what} to {}.", home_relative(&plan.dir)),
                };
                self.toast = Some((msg.clone(), self.now));
                self.last_export = Some((msg, self.now));
            }
            Err(e) => {
                log::error!("{e:#}");
                self.toast = Some((format!("{e:#}"), self.now));
            }
        }
    }

    fn export_panel(&mut self, ctx: &egui::Context, vp: Rect) {
        let width = 320.0;
        let pos = egui::pos2(vp.right() - 16.0 - width, vp.top() + 16.0);
        egui::Area::new(egui::Id::unique("export panel")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(width - 26.0);
                let tallest = vp.height() - 56.0;
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().max_height(tallest).min_scrolled_height(tallest).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("Export to Godot").font(theme::semibold(14.0)));
                    let Some(mesh) = &self.mesh else {
                        ui.label(egui::RichText::new("Open a model to export it.").color(TEXT_3));
                        return;
                    };
                    let has_uvs = mesh.has_uvs;
                    ui.label(
                        egui::RichText::new("A .glb with the maps inside, and a Godot material for each texture set that uses the same maps.")
                            .color(TEXT_2),
                    );
                    panel_divider(ui);

                    widgets::caps_label(ui, "Folder");
                    ui.add_space(4.0);
                    let plan = self.export_dir.as_ref().and_then(|d| self.export_plan(d));
                    let choose = match &plan {
                        Some(plan) => {
                            // Inside a project, the path Godot shows is the one that means something.
                            let (path, note) = match (&plan.godot, plan.res_dir()) {
                                (Some(g), Some(res)) => (res, format!("In {}, a Godot project.", g.name)),
                                _ => (
                                    home_relative(&plan.dir),
                                    "Not inside a Godot project. The materials point at the maps beside them, so keep the files together.".to_owned(),
                                ),
                            };
                            ui.label(egui::RichText::new(path).font(theme::mono(12.0)).color(TEXT)).on_hover_text(plan.dir.display().to_string());
                            ui.add_space(2.0);
                            ui.label(egui::RichText::new(note).color(TEXT_2));
                            ui.add_space(6.0);
                            widgets::button(ui, "Change folder", false, 32.0).clicked()
                        }
                        None => {
                            ui.label(egui::RichText::new("Pick a folder inside your Godot project, and Umber remembers it for next time.").color(TEXT_2));
                            ui.add_space(6.0);
                            widgets::button(ui, "Choose folder", false, 32.0).clicked()
                        }
                    };
                    panel_divider(ui);

                    widgets::caps_label(ui, "Texture size");
                    ui.add_space(6.0);
                    let sizes = export::SIZES.map(|s| (s, if s == 2048 { "2048" } else if s == 1024 { "1024" } else { "512" }));
                    widgets::segmented_fill(ui, &mut self.export_size, &sizes, 34.0);
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Pixels along each side of every map. Smaller suits phones and distant props.").color(TEXT_3));

                    if let Some(plan) = &plan {
                        panel_divider(ui);
                        widgets::caps_label(ui, "Writes");
                        ui.add_space(4.0);
                        for f in plan.files() {
                            ui.label(egui::RichText::new(f).font(theme::mono(12.0)).color(TEXT_2));
                        }
                    }
                    panel_divider(ui);

                    let job = self.export_job.as_ref().map(|j| j.done.load(Ordering::Relaxed) as f32 / export::total_steps(&j.plan) as f32);
                    let mut go = false;
                    match job {
                        Some(fraction) => {
                            ui.label(egui::RichText::new(format!("Exporting… {}", pct(fraction))).color(TEXT_2));
                            ui.add_space(6.0);
                            let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), Sense::hover());
                            ui.painter().rect_filled(bar, CornerRadius::same(3), TRACK);
                            let done = Rect::from_min_size(bar.min, egui::vec2(bar.width() * fraction.clamp(0.02, 1.0), bar.height()));
                            ui.painter().rect_filled(done, CornerRadius::same(3), ACCENT);
                        }
                        None if !has_uvs => {
                            ui.label(egui::RichText::new("This model has no UVs yet, so its maps wouldn't line up. Unwrap it in the UV workspace first.").color(WARN));
                        }
                        None => {
                            let label = if plan.is_some() { "Export" } else { "Choose folder and export" };
                            go = widgets::button_full(ui, label, true, 40.0).clicked();
                            if let Some((msg, at)) = &self.last_export {
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new(format!("{} {}", ago(self.now - at), msg)).color(TEXT_3));
                            }
                        }
                    }
                    if choose && self.pick_export_dir().is_some() {
                        // A new folder is a new place; the last export's note belongs to the old one.
                        self.last_export = None;
                    }
                    if go {
                        self.export_to_godot();
                    }
                });
            });
        });
    }

    fn open_path(&mut self, path: &Path, now: f64) {
        match mesh::load(path) {
            Ok(mesh) => {
                // A model without UVs gets the unwrap card instead.
                let note = mesh.has_uvs.then(|| format!("Opened {} · {} triangles", mesh.name, fmt_count(mesh.triangle_count())));
                self.set_mesh(mesh);
                self.model_path = Some(path.to_owned());
                self.last_export = None;
                self.toast = note.map(|n| (n, now));
            }
            Err(e) => {
                log::error!("{e:#}");
                self.toast = Some((format!("{e:#}"), now));
            }
        }
    }

    fn open_dialog(&mut self, now: f64) {
        let picked = rfd::FileDialog::new()
            .set_title("Open a model")
            .add_filter("3D models", mesh::SUPPORTED_EXTENSIONS)
            .pick_file();
        if let Some(path) = picked {
            self.open_path(&path, now);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context, now: f64) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| Some(f.path().to_path_buf())).collect());
        if let Some(path) = dropped.first() {
            self.open_path(path, now);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.open_dialog(now);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::E)) {
            self.export_to_godot();
        }
        // Only typing in a text field blocks shortcuts. Any clicked button or slider also
        // holds egui focus, and must not swallow Ctrl+Z or the tool keys.
        if ctx.text_edit_focused() {
            return;
        }
        // Redo first: Ctrl+Z also matches Ctrl+Shift+Z.
        let (redo, undo) = ctx.input_mut(|i| {
            let redo = i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y);
            (redo, i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z))
        });
        if redo {
            self.redo();
        }
        if undo {
            self.undo();
        }
        ctx.input(|i| {
            if i.modifiers.any() {
                return;
            }
            for tool in Tool::ALL {
                if i.key_pressed(tool.key()) {
                    self.tool = tool;
                }
            }
            if i.key_pressed(egui::Key::OpenBracket) {
                self.brush.size = (self.brush.size / 1.15).max(1.0);
            }
            if i.key_pressed(egui::Key::CloseBracket) {
                self.brush.size = (self.brush.size * 1.15).min(1000.0);
            }
            if i.key_pressed(egui::Key::X) {
                std::mem::swap(&mut self.primary, &mut self.secondary);
            }
            if i.key_pressed(egui::Key::F) {
                if let Some(m) = &self.mesh {
                    let (c, r) = m.bounding_sphere();
                    self.camera.frame(c, r);
                }
            }
        });
    }

    fn undo(&mut self) {
        self.end_stroke();
        self.history.record_stack_changes(&mut self.sets);
        let Some(mesh) = self.mesh.as_mut() else { return };
        let applied = self.history.undo(&mut self.sets, mesh, &self.rs.device, &self.rs.queue);
        self.after_history(applied);
    }

    fn redo(&mut self) {
        self.end_stroke();
        self.history.record_stack_changes(&mut self.sets);
        let Some(mesh) = self.mesh.as_mut() else { return };
        let applied = self.history.redo(&mut self.sets, mesh, &self.rs.device, &self.rs.queue);
        self.after_history(applied);
    }

    fn after_history(&mut self, applied: Option<Applied>) {
        match applied {
            Some(Applied { mesh: true, .. }) => self.mesh_to_gpu(),
            Some(Applied { set, .. }) => self.active_set = set,
            None => {}
        }
    }

    // ---------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui, now: f64) {
        let rect = ui.max_rect();
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, TOPBAR_LINE));

        ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)), |ui| {
            let (mark, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
            ui.painter().rect_filled(mark, CornerRadius::same(7), ACCENT);
            draw_logo_u(ui.painter(), mark);
            ui.label(egui::RichText::new("Umber").font(theme::semibold(14.0)));
            widgets::vdivider(ui, 18.0);
            let name = self.mesh.as_ref().map(|m| m.name.as_str()).unwrap_or("No model open");
            ui.label(egui::RichText::new(name).color(TEXT_2));
        });

        let labels = ["Paint", "UV", "Materials", "Export"];
        let w = widgets::segmented_width(ui.ctx(), &labels);
        let seg_rect = Rect::from_center_size(rect.center(), egui::vec2(w, 38.0));
        ui.scope_builder(UiBuilder::new().max_rect(seg_rect), |ui| {
            widgets::segmented(
                ui,
                &mut self.workspace,
                &[
                    (Workspace::Paint, labels[0]),
                    (Workspace::Uv, labels[1]),
                    (Workspace::Materials, labels[2]),
                    (Workspace::Export, labels[3]),
                ],
                38.0,
            );
        });

        ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Center)), |ui| {
            let exporting = self.export_job.as_ref().map(|j| j.done.load(Ordering::Relaxed) as f32 / export::total_steps(&j.plan) as f32);
            let label = match exporting {
                Some(f) => format!("Exporting {}", pct(f)),
                None => "Export to Godot".to_owned(),
            };
            let tip = match &self.export_dir {
                Some(dir) => format!("Export again to {} (Ctrl+E)", home_relative(dir)),
                None => "Write a .glb, a Godot material and its maps (Ctrl+E)".to_owned(),
            };
            if widgets::button(ui, &label, exporting.is_none(), 34.0).on_hover_text(tip).clicked() && exporting.is_none() {
                self.export_to_godot();
            }
            ui.add_space(4.0);
            if widgets::icon_action(ui, icons::REDO, self.history.can_redo(), 36.0, "Redo (Ctrl+Shift+Z)").clicked() {
                self.redo();
            }
            if widgets::icon_action(ui, icons::UNDO, self.history.can_undo(), 36.0, "Undo (Ctrl+Z)").clicked() {
                self.undo();
            }
            if widgets::icon_button(ui, icons::FOLDER_OPEN, false, 36.0, "Open a model (Ctrl+O)").clicked() {
                self.open_dialog(now);
            }
        });
    }

    // ---------------------------------------------------------------- viewport

    fn viewport(&mut self, ui: &mut egui::Ui) -> Rect {
        let rect = ui.max_rect();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();

        // Navigation: right or middle drag orbits, Shift pans, Alt + left drag orbits for pen-only use.
        let (shift, alt) = ui.input(|i| (i.modifiers.shift, i.modifiers.alt));
        let delta = response.drag_delta();
        let nav_drag = response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
            || (alt && response.dragged_by(egui::PointerButton::Primary));
        if nav_drag {
            if shift {
                self.camera.pan(delta.x, delta.y, rect.height());
            } else {
                self.camera.orbit(delta.x, delta.y);
            }
        }
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.camera.zoom(scroll / 50.0);
            }
        }

        let size = [(rect.width() * ppp).round() as u32, (rect.height() * ppp).round() as u32];
        let aspect = rect.width() / rect.height().max(1.0);
        let (view, proj) = (self.camera.view(), self.camera.projection(aspect));

        // The brush, fill and picker. Painting happens before compositing, so a dab shows up
        // in the same frame it was laid.
        let in_paint = self.workspace == Workspace::Paint && self.mesh.is_some();
        let (primary_down, pressed, origin, moves) = ui.input(|i| {
            let moves: Vec<Pos2> =
                i.events.iter().filter_map(|e| if let egui::Event::PointerMoved(p) = e { Some(*p) } else { None }).collect();
            (i.pointer.primary_down(), i.pointer.primary_pressed(), i.pointer.press_origin(), moves)
        });
        if self.stroke.is_some() && (!primary_down || !in_paint) {
            self.end_stroke();
        }
        let cam = brush::View { view_proj: proj * view, eye: self.camera.eye(), size };
        let has_uvs = self.mesh.as_ref().is_some_and(|m| m.has_uvs);
        if in_paint && pressed && !alt && response.hovered() && self.stroke.is_none() && !has_uvs {
            self.no_uv_offer_dismissed = false;
            self.toast = Some(("This model has no UVs yet, so there's nowhere for paint to go. Auto-unwrap it first.".into(), self.now));
        } else if in_paint && pressed && !alt && response.hovered() && self.stroke.is_none() {
            if let Some(origin) = origin {
                match self.tool {
                    t if t.strokes() => {
                        if self.begin_stroke(origin, rect) {
                            self.stroke_frame(&[origin], rect, ppp, &cam);
                        }
                    }
                    Tool::Fill => self.fill_at(origin, rect, shift, &cam),
                    Tool::Picker => self.pick_at(origin, rect),
                    t if t.later() => self.toast = Some((format!("{} comes later, with smart layers.", t.name()), self.now)),
                    _ => {}
                }
            }
        } else if self.stroke.is_some() {
            self.stroke_frame(&moves, rect, ppp, &cam);
        }

        for set in &mut self.sets {
            self.compositor.update(&self.rs.device, &self.rs.queue, set);
        }
        let params = FrameParams {
            view,
            proj,
            eye: self.camera.eye(),
            light_yaw: self.light_yaw,
            view_mode: if self.workspace == Workspace::Uv { ViewMode::UvCheck } else { self.view_mode },
            exposure: self.exposure,
        };
        let tex = self.renderer.render(&self.rs, size, &params);
        ui.painter().image(tex, rect, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);

        // Brush cursor: outer ring is the full size, inner ring shows the hardness falloff.
        let painting = self.workspace == Workspace::Paint && self.mesh.is_some() && self.tool.shows_brush_cursor();
        if painting && !nav_drag && !alt {
            if let Some(pos) = response.hover_pos() {
                self.brush_cursor(ui.painter(), pos);
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            }
        } else if self.workspace == Workspace::Paint && self.mesh.is_some() && !nav_drag && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
            ui.painter().rect_stroke(rect.shrink(8.0), CornerRadius::same(16), Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
        }
        rect
    }

    /// Outer ring is the brush at full pressure. While the pen is down, a filled disc shows the
    /// size and strength the current pressure gives. Tilt flattens the ring into an ellipse.
    fn brush_cursor(&self, p: &egui::Painter, pos: Pos2) {
        let pen = self.pen.status();
        let r = self.brush.size * 0.5;
        let light = Color32::from_rgb(0xf4, 0xf2, 0xee);

        let (axis, squash) = self.tilt_shape(&pen);
        let ellipse = |radius: f32| -> Vec<Pos2> {
            let perp = axis.rot90();
            (0..64)
                .map(|i| {
                    let a = i as f32 / 64.0 * std::f32::consts::TAU;
                    pos + axis * (a.cos() * radius * squash) + perp * (a.sin() * radius)
                })
                .collect()
        };

        if pen.active && pen.sample.touching {
            let k = self.brush.curve.apply(pen.sample.pressure);
            let size_k = if self.brush.pen_size { k } else { 1.0 };
            let alpha_k = if self.brush.pen_opacity { k } else { 1.0 };
            let fill = Color32::from_rgba_unmultiplied(0xc9, 0xa0, 0x63, (40.0 + 120.0 * alpha_k * self.brush.opacity) as u8);
            p.add(egui::Shape::convex_polygon(ellipse(r * size_k), fill, Stroke::NONE));
        }
        p.add(egui::Shape::closed_line(ellipse(r + 1.0), Stroke::new(1.0, Color32::from_black_alpha(120))));
        p.add(egui::Shape::closed_line(ellipse(r), Stroke::new(1.5, light)));
        if self.brush.hardness < 0.95 && r * self.brush.hardness > 3.0 {
            p.add(egui::Shape::closed_line(ellipse(r * self.brush.hardness), Stroke::new(1.0, light.gamma_multiply(0.35))));
        }
        p.circle_filled(pos, 1.5, light);
    }


    /// Tilt squashes the dab across the direction the pen leans: (axis, squash).
    fn tilt_shape(&self, pen: &crate::pen::PenStatus) -> (Vec2, f32) {
        if self.brush.pen_tilt && pen.active {
            let t = Vec2::from(pen.sample.tilt);
            let lean = t.length().min(1.0);
            (if lean > 0.02 { t / t.length() } else { Vec2::X }, 1.0 - 0.6 * lean)
        } else {
            (Vec2::X, 1.0)
        }
    }

    // ---------------------------------------------------------------- painting

    /// The mesh under a point of the viewport.
    fn pick(&self, pos: Pos2, rect: Rect) -> Option<mesh::Hit> {
        let mesh = self.mesh.as_ref()?;
        let x = (pos.x - rect.left()) / rect.width() * 2.0 - 1.0;
        let y = 1.0 - (pos.y - rect.top()) / rect.height() * 2.0;
        let inv = (self.camera.projection(rect.width() / rect.height().max(1.0)) * self.camera.view()).inverse();
        let near = inv.project_point3(glam::Vec3::new(x, y, 0.0));
        let far = inv.project_point3(glam::Vec3::new(x, y, 1.0));
        mesh.raycast(near, (far - near).normalize())
    }

    /// Make the material under `pos` the one being painted, when the model has several.
    fn activate_set_at(&mut self, pos: Pos2, rect: Rect) -> Option<mesh::Hit> {
        let hit = self.pick(pos, rect);
        if let (Some(h), Some(mesh)) = (&hit, &self.mesh) {
            self.active_set = mesh.material_of(h.triangle).min(self.sets.len().saturating_sub(1));
        }
        hit
    }

    /// What the brush lays down, before the tool decides on mode and target.
    fn paint_values(&self) -> brush::Paint {
        let c = self.primary;
        let b = &self.brush;
        brush::Paint {
            mode: brush::Mode::Paint,
            colour: [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0],
            material: [b.rough, b.metal, b.height],
            channels: [b.paint_colour, b.paint_rough, b.paint_metal, b.paint_height],
            hardness: b.hardness,
            fill_opacity: b.opacity,
        }
    }

    /// Get the selected layer ready for the current tool, adding a paint layer or a mask when
    /// it needs one. Returns whether the stroke paints the mask, and what it lays down.
    fn prepare_layer(&mut self) -> Option<(bool, brush::Paint)> {
        let now = self.now;
        let base = self.paint_values();
        let set = self.sets.get_mut(self.active_set)?;
        let is_paint = set.selected_layer().is_some_and(|l| l.pixels.is_some());
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let ensure_mask = |set: &mut TextureSet| {
            let layer = set.layers.get_mut(set.selected)?;
            if layer.mask.is_none() {
                layer.mask = Some(Rc::new(Mask::white(device, queue)));
            }
            Some(())
        };
        match self.tool {
            Tool::Brush | Tool::Fill if !is_paint => {
                self.add_layer(false);
                let name = self.sets[self.active_set].selected_layer().map(|l| l.name.clone()).unwrap_or_default();
                self.toast = Some((format!("Added {name} to paint on, so the fill layer below stays as it is."), now));
                Some((false, base))
            }
            Tool::Brush | Tool::Fill => Some((false, base)),
            Tool::Eraser if is_paint => Some((false, brush::Paint { mode: brush::Mode::Erase, ..base })),
            Tool::Eraser => {
                ensure_mask(set)?;
                self.toast = Some(("Erasing a fill layer paints its mask, so the Mask tool can bring it back.".into(), now));
                Some((true, brush::Paint { colour: [0.0; 3], ..base }))
            }
            Tool::Mask => {
                ensure_mask(set)?;
                let grey = 0.2126 * base.colour[0] + 0.7152 * base.colour[1] + 0.0722 * base.colour[2];
                Some((true, brush::Paint { colour: [grey; 3], ..base }))
            }
            _ => None,
        }
    }

    /// Start a stroke on the selected layer of the material under `pos`.
    fn begin_stroke(&mut self, pos: Pos2, rect: Rect) -> bool {
        if self.sets.len() > 1 {
            self.activate_set_at(pos, rect);
        }
        // Anything changed before the stroke is its own undo step, and so is a layer or mask
        // the stroke had to add.
        self.history.record_stack_changes(&mut self.sets);
        let Some((mask, paint)) = self.prepare_layer() else { return false };
        self.history.record_stack_changes(&mut self.sets);

        let set = self.active_set;
        let Some(layer) = self.sets[set].selected_layer() else { return false };
        let target = match (mask, &layer.mask, &layer.pixels) {
            (true, Some(m), _) => brush::Target::Mask(m),
            (false, _, Some(p)) => brush::Target::Layer(p),
            _ => return false,
        };
        if !layer.visible {
            self.toast = Some((format!("{} is hidden, so you won't see this stroke until you show it.", layer.name), self.now));
        }
        self.brush_engine.begin(&self.rs.device, &self.rs.queue, target);
        let pressure = self.pressure();
        self.stroke = Some(StrokeState { set, mask, paint, last: pos, next: 0.0, pressure });
        true
    }

    /// Pen pressure through the curve, or full pressure for a mouse.
    fn pressure(&self) -> f32 {
        let pen = self.pen.status();
        if pen.active { self.brush.curve.apply(pen.sample.pressure) } else { 1.0 }
    }

    /// Lay dabs along the pointer's path since last frame.
    fn stroke_frame(&mut self, points: &[Pos2], rect: Rect, ppp: f32, cam: &brush::View) {
        let pen = self.pen.status();
        let (axis, squash) = self.tilt_shape(&pen);
        let p1 = self.pressure();
        let b = &self.brush;
        let Some(st) = &mut self.stroke else { return };
        let p0 = st.pressure;
        let size_k = |k: f32| if b.pen_size { k } else { 1.0 };
        let alpha_k = |k: f32| if b.pen_opacity { k } else { 1.0 };
        let spacing = |k: f32| (b.size * size_k(k) * SPACING).max(0.5);
        // Flow is what one pass of the brush over a texel lays down, so it's shared out over
        // the dabs that cover it. A texel near the middle of a stroke sits under about
        // 1 / SPACING dabs; giving each a big flat share would show every dab as a ring.
        let flow = 1.0 - (1.0 - b.flow.clamp(0.0, 1.0)).powf(SPACING * 2.0);

        let mut total = 0.0;
        let mut prev = st.last;
        for p in points {
            total += (*p - prev).length();
            prev = *p;
        }
        let mut dabs = Vec::new();
        let mut travelled = 0.0;
        let mut from = st.last;
        for &p in points {
            let seg = p - from;
            let len = seg.length();
            let mut d = st.next;
            while d <= len {
                let at = if len > 0.0 { from + seg * (d / len) } else { from };
                let k = if total > 0.0 { p0 + (p1 - p0) * ((travelled + d) / total) } else { p1 };
                dabs.push(Dab {
                    center: [(at.x - rect.left()) * ppp, (at.y - rect.top()) * ppp],
                    axis: [axis.x, axis.y],
                    radius: b.size * 0.5 * size_k(k) * ppp,
                    squash,
                    strength: b.opacity * alpha_k(k),
                    flow,
                });
                d += spacing(k);
            }
            st.next = d - len;
            travelled += len;
            from = p;
        }
        st.last = from;
        st.pressure = p1;
        if dabs.is_empty() {
            return;
        }
        let (set, paint) = (st.set, st.paint);
        if let Some(draw) = self.renderer.mesh_draw(set) {
            self.brush_engine.paint(&self.rs.device, &self.rs.queue, &draw, cam, &paint, &dabs);
            self.sets[set].dirty = true;
        }
    }

    /// Finish the stroke and keep it as one undo step.
    fn end_stroke(&mut self) {
        let Some(st) = self.stroke.take() else { return };
        let Some(tiles) = self.brush_engine.end(&self.rs.device, &self.rs.queue) else { return };
        self.history.push(Change::Pixels { set: st.set, tiles });
        if !st.mask && st.paint.mode == brush::Mode::Paint && st.paint.channels[0] {
            let set = &mut self.sets[st.set];
            if let Some(layer) = set.layers.get_mut(set.selected) {
                layer.swatch.get_or_insert(self.primary);
            }
        }
    }

    /// Fill the UV island under `pos` (or with `whole`, the whole material) on the selected
    /// paint layer.
    fn fill_at(&mut self, pos: Pos2, rect: Rect, whole: bool, cam: &brush::View) {
        let Some(hit) = self.activate_set_at(pos, rect) else { return };
        self.history.record_stack_changes(&mut self.sets);
        let Some((_, paint)) = self.prepare_layer() else { return };
        self.history.record_stack_changes(&mut self.sets);
        let set = self.active_set;
        let (Some(mesh), Some(draw)) = (&self.mesh, self.renderer.mesh_draw(set)) else { return };
        let (device, queue) = (&self.rs.device, &self.rs.queue);

        let island_buffer;
        let draw = if whole {
            draw
        } else {
            let islands = self.islands.get_or_insert_with(|| mesh.uv_islands());
            let island = islands[hit.triangle];
            let mut indices = Vec::new();
            for r in &draw.ranges {
                for tri in (r.start / 3)..(r.end / 3) {
                    if islands[tri as usize] == island {
                        indices.extend_from_slice(&mesh.indices[tri as usize * 3..tri as usize * 3 + 3]);
                    }
                }
            }
            island_buffer = eframe::egui_wgpu::wgpu::util::DeviceExt::create_buffer_init(
                device,
                &eframe::egui_wgpu::wgpu::util::BufferInitDescriptor {
                    label: Some("fill island"),
                    contents: bytemuck::cast_slice(&indices),
                    usage: eframe::egui_wgpu::wgpu::BufferUsages::INDEX,
                },
            );
            #[allow(clippy::single_range_in_vec_init)] // One range, in a list of them.
            brush::MeshDraw { vertices: draw.vertices, indices: &island_buffer, ranges: vec![0..indices.len() as u32] }
        };
        let Some(pixels) = self.sets[set].selected_layer().and_then(|l| l.pixels.as_deref()) else { return };
        self.brush_engine.begin(device, queue, brush::Target::Layer(pixels));
        self.brush_engine.paint(device, queue, &draw, cam, &brush::Paint { mode: brush::Mode::Fill, ..paint }, &[]);
        if let Some(tiles) = self.brush_engine.end(device, queue) {
            self.history.push(Change::Pixels { set, tiles });
        }
        self.sets[set].dirty = true;
        if paint.channels[0] {
            let s = &mut self.sets[set];
            if let Some(layer) = s.layers.get_mut(s.selected) {
                layer.swatch.get_or_insert(self.primary);
            }
        }
    }

    /// Pick up the colour and material under `pos`, as the stack shows them.
    fn pick_at(&mut self, pos: Pos2, rect: Rect) {
        let Some(hit) = self.activate_set_at(pos, rect) else { return };
        let (Some(mesh), Some(set)) = (&self.mesh, self.sets.get(self.active_set)) else { return };
        let uv = mesh.uv_at(&hit);
        let texel = |v: f32| ((v.rem_euclid(1.0) * texset::SIZE as f32) as u32).min(texset::SIZE - 1);
        let (x, y) = (texel(uv.x), texel(uv.y));
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let [color, mat, _] = set.output_textures();
        let c = texset::read_texel(device, queue, color, x, y, 4);
        let m = texset::read_texel(device, queue, mat, x, y, 8);
        self.primary = Color32::from_rgb(c[0], c[1], c[2]);
        self.brush.rough = texset::f16_to_f32(m[0], m[1]);
        self.brush.metal = texset::f16_to_f32(m[2], m[3]);
        self.brush.height = texset::f16_to_f32(m[4], m[5]);
    }

    // ---------------------------------------------------------------- floating panels

    fn tool_rail(&mut self, ctx: &egui::Context, vp: Rect) {
        let resp = egui::Area::new(egui::Id::unique("tool_rail")).order(Order::Foreground).fixed_pos(vp.min + Vec2::splat(16.0)).show(ctx, |ui| {
            theme::panel_frame().show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.vertical_centered(|ui| {
                    for tool in Tool::ALL {
                        let tip = format!("{}  ({})", tool.name(), tool.key().name());
                        if widgets::icon_button(ui, tool.icon(), self.tool == tool, 44.0, &tip).clicked() {
                            self.tool = tool;
                        }
                    }
                    ui.add_space(4.0);
                    let (line, _) = ui.allocate_exact_size(egui::vec2(32.0, 1.0), Sense::hover());
                    ui.painter().rect_filled(line, CornerRadius::ZERO, PANEL_LINE);
                    ui.add_space(4.0);
                    let (puck, resp) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::click());
                    let p = ui.painter();
                    let back = puck.max - Vec2::splat(15.0);
                    p.circle(back, 11.0, self.secondary, Stroke::new(2.0, PANEL));
                    let front = puck.min + Vec2::splat(17.0);
                    p.circle(front, 13.0, self.primary, Stroke::new(2.0, PANEL));
                    p.circle_stroke(front, 14.5, Stroke::new(1.0, Color32::from_rgb(0x54, 0x4a, 0x3f)));
                    if resp.on_hover_text("Colour (X swaps)").clicked() {
                        self.colour_open = !self.colour_open;
                    }
                });
            });
        });
        if self.colour_open {
            // Beside the rail, just under the brush bar (which can be two rows tall).
            let pos = egui::pos2(resp.response.rect.right() + 8.0, vp.top() + 120.0);
            egui::Area::new(egui::Id::unique("colour_popover")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    let w = 220.0;
                    ui.set_width(w);
                    // The picker sizes itself from the slider width.
                    ui.spacing_mut().slider_width = w;
                    ui.horizontal(|ui| {
                        widgets::caps_label(ui, "Colour");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, icons::X, false, 28.0, "Close").clicked() {
                                self.colour_open = false;
                            }
                        });
                    });
                    egui::color_picker::color_picker_color32(ui, &mut self.primary, egui::color_picker::Alpha::Opaque);
                    ui.add_space(6.0);
                    let b = &mut self.brush;
                    for (on, label, value, tip) in [
                        (b.paint_rough, "Roughness", &mut b.rough, "Turn on Rough in the brush bar to paint it"),
                        (b.paint_metal, "Metalness", &mut b.metal, "Turn on Metal in the brush bar to paint it"),
                        (b.paint_height, "Height", &mut b.height, "Turn on Height in the brush bar to paint it"),
                    ] {
                        ui.add_enabled_ui(on, |ui| {
                            let r = labelled_slider(ui, label, value, 0.0..=1.0, w, |v| format!("{v:.2}"));
                            r.on_disabled_hover_text(tip);
                        });
                    }
                });
            });
        }
    }

    fn brush_bar(&mut self, ctx: &egui::Context, vp: Rect) {
        // Fits between the tool rail and the layers panel.
        let max_w = (vp.width() - 92.0 - 352.0 - 16.0 - 28.0).max(240.0);
        egui::Area::new(egui::Id::unique("brush_bar")).order(Order::Foreground).fixed_pos(vp.min + egui::vec2(92.0, 16.0)).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                // One row when it fits, otherwise the pen and channel toggles drop to a second row.
                let one_row = max_w >= 1360.0;
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 8.0);
                let strokes = self.tool.strokes();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(self.tool.name()).font(theme::medium(13.0)));
                    widgets::vdivider(ui, 24.0);
                    match self.tool {
                        t if t.strokes() => {
                            slider_with_value(ui, "Size", &mut self.brush.size, 1.0..=500.0, 110.0, |v| format!("{v:.0}px"));
                            slider_with_value(ui, "Opacity", &mut self.brush.opacity, 0.0..=1.0, 80.0, pct);
                            slider_with_value(ui, "Flow", &mut self.brush.flow, 0.0..=1.0, 64.0, pct);
                            slider_with_value(ui, "Hardness", &mut self.brush.hardness, 0.0..=1.0, 64.0, pct);
                            if one_row {
                                widgets::vdivider(ui, 24.0);
                                self.brush_toggles(ui);
                            }
                        }
                        Tool::Fill => {
                            slider_with_value(ui, "Opacity", &mut self.brush.opacity, 0.0..=1.0, 80.0, pct);
                            widgets::vdivider(ui, 24.0);
                            self.channel_toggles(ui);
                            widgets::vdivider(ui, 24.0);
                            hint(ui, "Click fills a UV island · Shift+click fills the whole material");
                        }
                        Tool::Picker => hint(ui, "Click the model to pick up its colour and material"),
                        _ => hint(ui, "Comes later, with smart layers"),
                    }
                });
                if strokes && !one_row {
                    ui.horizontal(|ui| self.brush_toggles(ui));
                }
            });
        });
    }

    fn brush_toggles(&mut self, ui: &mut egui::Ui) {
        widgets::caps_label(ui, "Pen");
        let pen = self.pen.status();
        let live = (pen.active && pen.sample.touching).then_some(pen.sample.pressure);
        let thumb = widgets::pressure_curve(ui, egui::vec2(36.0, 28.0), &self.brush.curve, live);
        let tip = match (&pen.device, &pen.problem) {
            (Some(name), _) => format!("Pressure curve\n{name}"),
            (None, Some(problem)) => format!("Pressure curve\nNo pen: {problem}"),
            (None, None) => "Pressure curve".to_owned(),
        };
        let anchor = thumb.rect.left_bottom() + egui::vec2(-12.0, 18.0);
        if thumb.on_hover_text(tip).clicked() {
            self.pen_open = !self.pen_open;
            ui.ctx().request_repaint();
        }
        if self.pen_open {
            self.pen_popover(ui.ctx(), anchor);
        }
        widgets::chip(ui, "Size", &mut self.brush.pen_size).on_hover_text("Pen pressure changes brush size");
        widgets::chip(ui, "Opacity", &mut self.brush.pen_opacity).on_hover_text("Pen pressure changes opacity");
        widgets::chip(ui, "Tilt", &mut self.brush.pen_tilt).on_hover_text("Pen tilt flattens the brush");
        widgets::vdivider(ui, 24.0);
        if self.tool == Tool::Mask {
            hint(ui, "Dark hides the layer, light shows it");
        } else {
            self.channel_toggles(ui);
        }
    }

    /// Which channels the brush and fill paint.
    fn channel_toggles(&mut self, ui: &mut egui::Ui) {
        widgets::caps_label(ui, "Paints");
        widgets::chip(ui, "Colour", &mut self.brush.paint_colour).on_hover_text("This brush paints base colour");
        widgets::chip(ui, "Rough", &mut self.brush.paint_rough).on_hover_text("This brush paints roughness");
        widgets::chip(ui, "Metal", &mut self.brush.paint_metal).on_hover_text("This brush paints metalness");
        widgets::chip(ui, "Height", &mut self.brush.paint_height).on_hover_text("This brush paints height (bumps)");
    }

    /// Pressure curve settings, opened from the curve thumbnail in the brush bar.
    fn pen_popover(&mut self, ctx: &egui::Context, pos: Pos2) {
        let pen = self.pen.status();
        egui::Area::new(egui::Id::unique("pen_popover")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(240.0);
                ui.horizontal(|ui| {
                    widgets::caps_label(ui, "Pen pressure");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, icons::X, false, 28.0, "Close").clicked() {
                            self.pen_open = false;
                        }
                    });
                });
                let live = (pen.active && pen.sample.touching).then_some(pen.sample.pressure);
                widgets::pressure_curve(ui, egui::vec2(240.0, 140.0), &self.brush.curve, live);
                ui.add_space(6.0);
                labelled_slider(ui, "Softness", &mut self.brush.curve.softness, -1.0..=1.0, 240.0, |v| {
                    match v {
                        v if v < -0.05 => format!("Firm {:.0}", -v * 100.0),
                        v if v > 0.05 => format!("Soft {:.0}", v * 100.0),
                        _ => "Even".to_owned(),
                    }
                });
                labelled_slider(ui, "Lightest touch", &mut self.brush.curve.minimum, 0.0..=0.8, 240.0, pct);
                let status = match (&pen.device, &pen.problem) {
                    (Some(name), _) if pen.active => {
                        format!("{name}\n{} levels · pressure {:.0}%", pen.levels, pen.sample.pressure * 100.0)
                    }
                    (Some(name), _) => format!("{name}\n{} levels · touch the pen to the tablet", pen.levels),
                    (None, Some(problem)) => format!("No pen: {problem}"),
                    (None, None) => "Looking for a pen…".to_owned(),
                };
                ui.label(egui::RichText::new(status).font(theme::mono(11.0)).color(TEXT_3));
            });
        });
    }

    fn layers_panel(&mut self, ctx: &egui::Context, vp: Rect) {
        let width = 320.0;
        let pos = egui::pos2(vp.right() - 16.0 - width, vp.top() + 16.0);
        egui::Area::new(egui::Id::unique("layers")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.set_width(width - 26.0);
                self.layers_header(ui);
                ui.add_space(6.0);
                self.layer_list(ui);
                if self.sets.get(self.active_set).and_then(TextureSet::selected_layer).is_some() {
                    panel_divider(ui);
                    self.layer_properties(ui);
                }
                panel_divider(ui);

                widgets::caps_label(ui, "Lighting");
                ui.add_space(4.0);
                let w = ui.available_width();
                let mut deg = self.light_yaw.to_degrees().rem_euclid(360.0);
                if labelled_slider(ui, "Light rotation", &mut deg, 0.0..=360.0, w, |v| format!("{v:.0}°")).changed() {
                    self.light_yaw = deg.to_radians();
                }
                labelled_slider(ui, "Exposure", &mut self.exposure, 0.25..=3.0, w, |v| format!("{v:.2}"));
            });
        });
    }

    /// Title, the material being shown (a menu when the model has several) and the add button.
    fn layers_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(egui::RichText::new("Layers").font(theme::semibold(14.0)));
                let name = self.sets.get(self.active_set).map(|s| s.name.as_str()).unwrap_or("No material");
                let sub = format!("{name} · {} px", texset::SIZE);
                if self.sets.len() > 1 {
                    // One painted widget, so the whole line (text and chevron) takes the click.
                    let galley = ui.painter().layout_no_wrap(sub, egui::FontId::proportional(12.0), TEXT_3);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(galley.size().x + 18.0, galley.size().y.max(16.0)), Sense::click());
                    let colour = if resp.hovered() { TEXT } else { TEXT_3 };
                    ui.painter().galley(egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5), galley, colour);
                    ui.painter().text(
                        egui::pos2(rect.right() - 6.0, rect.center().y),
                        Align2::CENTER_CENTER,
                        icons::CHEVRON_DOWN,
                        theme::icon_font(12.0),
                        colour,
                    );
                    let resp = resp.on_hover_text("Switch material");
                    egui::Popup::menu(&resp).show(|ui| {
                        for (i, set) in self.sets.iter().enumerate() {
                            if ui.selectable_label(i == self.active_set, &set.name).clicked() {
                                self.active_set = i;
                            }
                        }
                    });
                } else {
                    ui.label(egui::RichText::new(sub).color(TEXT_3).size(12.0));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let resp = widgets::icon_action(ui, icons::PLUS, !self.sets.is_empty(), 34.0, "Add a layer");
                let mut add = None;
                egui::Popup::menu(&resp).show(|ui| {
                    if menu_item(ui, "Paint layer", "Empty, for the brush to paint on").clicked() {
                        add = Some(false);
                    }
                    if menu_item(ui, "Fill layer", "One colour and material everywhere").clicked() {
                        add = Some(true);
                    }
                });
                if let Some(fill) = add {
                    self.add_layer(fill);
                }
            });
        });
    }

    fn add_layer(&mut self, fill: bool) {
        let Some(set) = self.sets.get_mut(self.active_set) else { return };
        let count = set.layers.iter().filter(|l| matches!(l.kind, LayerKind::Fill(_)) == fill).count() + 1;
        let layer = if fill {
            Layer::fill(
                format!("Fill layer {count}"),
                Fill {
                    colour: self.primary,
                    rough: 0.5,
                    metal: 0.0,
                    height: 0.5,
                    channels: Channels { colour: true, rough: false, metal: false, height: false },
                },
            )
        } else {
            Layer::paint(&self.rs.device, format!("Paint layer {count}"))
        };
        let at = (set.selected + 1).min(set.layers.len());
        set.layers.insert(at, layer);
        set.selected = at;
    }

    /// The stack, top layer first.
    fn layer_list(&mut self, ui: &mut egui::Ui) {
        let Some(set) = self.sets.get_mut(self.active_set) else {
            ui.label(egui::RichText::new("Open a model to add layers.").color(TEXT_3));
            return;
        };
        let mut select = None;
        egui::ScrollArea::vertical().max_height(54.0 * 6.0 + 10.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for i in (0..set.layers.len()).rev() {
                if layer_row(ui, i, &mut set.layers[i], i == set.selected) {
                    select = Some(i);
                }
            }
        });
        if let Some(i) = select {
            set.selected = i;
        }
    }

    /// Settings for the selected layer: order, blend mode, opacity, and a fill layer's values.
    fn layer_properties(&mut self, ui: &mut egui::Ui) {
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let Some(set) = self.sets.get_mut(self.active_set) else { return };
        let (i, n) = (set.selected, set.layers.len());
        let mut action = None;
        ui.horizontal(|ui| {
            widgets::caps_label(ui, "Selected layer");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if widgets::icon_action(ui, icons::TRASH, true, 30.0, "Delete layer").clicked() {
                    action = Some(LayerAction::Delete);
                }
                if widgets::icon_action(ui, icons::COPY, true, 30.0, "Duplicate layer").clicked() {
                    action = Some(LayerAction::Duplicate);
                }
                if widgets::icon_action(ui, icons::ARROW_DOWN, i > 0, 30.0, "Move down").clicked() {
                    action = Some(LayerAction::Down);
                }
                if widgets::icon_action(ui, icons::ARROW_UP, i + 1 < n, 30.0, "Move up").clicked() {
                    action = Some(LayerAction::Up);
                }
            });
        });
        ui.add_space(2.0);
        let w = ui.available_width();
        let Some(layer) = set.layers.get_mut(i) else { return };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Blend").color(TEXT_2));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                egui::ComboBox::from_id_salt("blend_mode").selected_text(layer.blend.name()).width(150.0).show_ui(ui, |ui| {
                    for mode in BlendMode::ALL {
                        ui.selectable_value(&mut layer.blend, mode, mode.name());
                    }
                });
            });
        });
        ui.add_space(4.0);
        labelled_slider(ui, "Opacity", &mut layer.opacity, 0.0..=1.0, w, pct);
        let mut mask_tool = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Mask").color(TEXT_2));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if layer.mask.is_some() {
                    if widgets::icon_action(ui, icons::TRASH, true, 28.0, "Remove the mask").clicked() {
                        layer.mask = None;
                    }
                    widgets::chip(ui, "Invert", &mut layer.mask_invert).on_hover_text("Swap what the mask hides and shows");
                    if widgets::chip(ui, "Paint", &mut (self.tool == Tool::Mask)).on_hover_text("Paint the mask (M)").clicked() {
                        mask_tool = true;
                    }
                } else if widgets::button(ui, "Add mask", false, 28.0)
                    .on_hover_text("Hide parts of this layer by painting with the Mask tool (M)")
                    .clicked()
                {
                    layer.mask = Some(Rc::new(Mask::white(device, queue)));
                    mask_tool = true;
                }
            });
        });
        if mask_tool {
            self.tool = Tool::Mask;
        }
        ui.add_space(4.0);

        if let LayerKind::Fill(f) = &mut layer.kind {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                widgets::chip(ui, "Colour", &mut f.channels.colour).on_hover_text("This layer sets base colour");
                widgets::chip(ui, "Rough", &mut f.channels.rough).on_hover_text("This layer sets roughness");
                widgets::chip(ui, "Metal", &mut f.channels.metal).on_hover_text("This layer sets metalness");
                widgets::chip(ui, "Height", &mut f.channels.height).on_hover_text("This layer sets height (bumps)");
            });
            ui.add_space(6.0);
            if f.channels.colour {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Colour").color(TEXT_2));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        egui::color_picker::color_edit_button_srgba(ui, &mut f.colour, egui::color_picker::Alpha::Opaque);
                    });
                });
                ui.add_space(4.0);
            }
            if f.channels.rough {
                labelled_slider(ui, "Roughness", &mut f.rough, 0.0..=1.0, w, |v| format!("{v:.2}"));
            }
            if f.channels.metal {
                labelled_slider(ui, "Metalness", &mut f.metal, 0.0..=1.0, w, |v| format!("{v:.2}"));
            }
            if f.channels.height {
                labelled_slider(ui, "Height", &mut f.height, 0.0..=1.0, w, |v| {
                    if (v - 0.5).abs() < 0.005 { "Flat".to_owned() } else { format!("{:+.2}", v - 0.5) }
                });
            }
        }

        match action {
            Some(LayerAction::Up) if i + 1 < n => {
                set.layers.swap(i, i + 1);
                set.selected = i + 1;
            }
            Some(LayerAction::Down) if i > 0 => {
                set.layers.swap(i, i - 1);
                set.selected = i - 1;
            }
            Some(LayerAction::Duplicate) => {
                let copy = set.layers[i].duplicate(device, queue);
                set.layers.insert(i + 1, copy);
                set.selected = i + 1;
            }
            Some(LayerAction::Delete) => {
                set.layers.remove(i);
                set.selected = i.saturating_sub(1).min(set.layers.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- UV workspace

    /// UV health of the active material, and the buttons that fix what it finds.
    fn uv_panel(&mut self, ctx: &egui::Context, vp: Rect) {
        let width = 320.0;
        let pos = egui::pos2(vp.right() - 16.0 - width, vp.top() + 16.0);
        egui::Area::new(egui::Id::unique("uv panel")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                // On short screens the unwrap controls scroll into view rather than off the edge.
                let tallest = vp.height() - 56.0;
                ui.set_width(width - 26.0);
                // A bar of its own, not floating over the numbers on the right.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().max_height(tallest).min_scrolled_height(tallest).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new("UV health").font(theme::semibold(14.0)));
                    let Some(mesh) = &self.mesh else {
                        ui.label(egui::RichText::new("Open a model to check its UVs.").color(TEXT_3));
                        return;
                    };
                    let set = self.active_set.min(self.sets.len().saturating_sub(1));
                    if self.sets.len() > 1 {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                            for (i, s) in self.sets.iter().enumerate() {
                                let mut on = i == set;
                                if widgets::chip(ui, &s.name, &mut on).clicked() {
                                    self.active_set = i;
                                }
                            }
                        });
                    } else {
                        let name = self.sets.first().map_or("", |s| s.name.as_str());
                        ui.label(egui::RichText::new(format!("{name} · {} px", texset::SIZE)).color(TEXT_3).size(12.0));
                    }
                    ui.add_space(8.0);

                    if self.uv_health.as_ref().is_none_or(|(s, ..)| *s != set) {
                        let health = unwrap::health(mesh, set);
                        let texture = ctx.load_texture("uv coverage", coverage_image(&health), egui::TextureOptions::NEAREST);
                        self.uv_health = Some((set, health, texture));
                    }
                    let (_, health, texture) = self.uv_health.as_ref().expect("just filled");

                    // The layout itself: islands in brass, overlaps in red.
                    let side = ui.available_width();
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(rect, CornerRadius::same(6), SEG_BG);
                    for k in 1..4 {
                        let t = k as f32 / 4.0;
                        let line = Stroke::new(1.0, PANEL_LINE);
                        p.vline(rect.left() + rect.width() * t, rect.y_range(), line);
                        p.hline(rect.x_range(), rect.top() + rect.height() * t, line);
                    }
                    p.image(texture.id(), rect, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
                    p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, PANEL_LINE), egui::StrokeKind::Inside);
                    ui.add_space(8.0);

                    let none_or = |v: f32, cut: f32| if v < cut { "none".to_owned() } else { pct(v) };
                    let rows: [(&str, String, bool, &str); 6] = [
                        ("Texture used", pct(health.used), health.used < 0.3 && !health.unusable(), "Share of the texture square the islands cover"),
                        ("Overlap", none_or(health.overlap, 0.0005), health.overlap >= 0.005, "Texels more than one part of the model shares: paint there lands in both places"),
                        (
                            "Stretch",
                            if health.stretch < 1.01 { "none".into() } else { format!("{:.0}%", (health.stretch - 1.0) * 100.0) },
                            health.stretch > 1.25,
                            "How much a round dab gets squashed on average, by surface area",
                        ),
                        (
                            "Texel density",
                            format!("{} px/m", fmt_count(health.density.round() as usize)),
                            health.density_spread > 2.0,
                            "Texels per metre (per model unit) at the texture size",
                        ),
                        ("Outside 0–1", none_or(health.outside, 0.001), health.outside >= 0.001, "UV area past the edge of the texture, where it repeats"),
                        ("Islands", fmt_count(health.islands), false, "Separate pieces of the layout"),
                    ];
                    for (label, value, warn, tip) in rows {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(label).color(TEXT_2)).on_hover_text(tip);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(egui::RichText::new(value).font(theme::mono(12.0)).color(if warn { WARN } else { TEXT }));
                            });
                        });
                    }
                    panel_divider(ui);
                    for line in uv_advice(health) {
                        ui.label(egui::RichText::new(line).color(TEXT_2));
                        ui.add_space(2.0);
                    }
                    ui.add_space(8.0);

                    let busy = self.unwrap_job.is_some();
                    let unusable = health.unusable() || !mesh.has_uvs;
                    let mut start = None;
                    panel_divider(ui);
                    widgets::caps_label(ui, "Unwrap");
                    ui.add_space(6.0);
                    widgets::segmented_fill(ui, &mut self.unwrap_method, &UnwrapMethod::ALL, 34.0);
                    let axes: &[(UnwrapAxis, &str)] = match self.unwrap_method {
                        UnwrapMethod::Planar => &[(UnwrapAxis::View, "View"), (UnwrapAxis::X, "X"), (UnwrapAxis::Y, "Y"), (UnwrapAxis::Z, "Z")],
                        UnwrapMethod::Cylinder | UnwrapMethod::Sphere => &[(UnwrapAxis::X, "X"), (UnwrapAxis::Y, "Y"), (UnwrapAxis::Z, "Z")],
                        UnwrapMethod::Auto | UnwrapMethod::Box => &[],
                    };
                    if !axes.is_empty() {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let planar = self.unwrap_method == UnwrapMethod::Planar;
                            let (label, tip) = if planar {
                                ("From", "Which way to look at the model. View uses the camera as it is now.")
                            } else {
                                ("Around", "The axis the model is wrapped around: Y is up.")
                            };
                            ui.label(egui::RichText::new(label).color(TEXT_2)).on_hover_text(tip);
                            // Right-aligned, with the choices still reading left to right.
                            let labels: Vec<&str> = axes.iter().map(|a| a.1).collect();
                            ui.add_space(ui.available_width() - widgets::segmented_width(ui.ctx(), &labels));
                            let axis = if planar { &mut self.planar_axis } else { &mut self.round_axis };
                            widgets::segmented(ui, axis, axes, 30.0);
                        });
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(self.unwrap_method.about()).color(TEXT_3).size(12.0));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let unwrap_tip = if busy { "Already working on it" } else { "Lay the model out again from scratch, the way chosen above" };
                        let resp = ui.add_enabled_ui(!busy, |ui| widgets::button(ui, "Unwrap", unusable, 36.0)).inner;
                        if resp.on_hover_text(unwrap_tip).on_disabled_hover_text(unwrap_tip).clicked() {
                            start = Some(self.chosen_unwrap());
                        }
                        let repack_tip = if unusable {
                            "Needs UVs to repack. Unwrap makes them."
                        } else if busy {
                            "Already working on it"
                        } else {
                            "Keep every island's shape and lay them out tighter"
                        };
                        let resp = ui.add_enabled_ui(!busy && !unusable, |ui| widgets::button(ui, "Repack", !unusable && health.used < 0.3, 36.0)).inner;
                        if resp.on_hover_text(repack_tip).on_disabled_hover_text(repack_tip).clicked() {
                            start = Some(unwrap::Mode::Repack);
                        }
                    });
                    ui.add_space(6.0);
                    hint(ui, "Paint moves to the new layout, and Ctrl+Z undoes it. Seams, pins and straightening come later.");
                    if let Some(mode) = start {
                        self.start_unwrap(mode);
                    }
                });
            });
        });
    }

    /// The offer to unwrap a model that arrived without UVs, and progress while one runs.
    fn unwrap_card(&mut self, ctx: &egui::Context, vp: Rect) {
        let Some(mesh) = &self.mesh else { return };
        let job = self.unwrap_job.as_ref().map(|j| (j.mode, j.progress.clone()));
        if job.is_none() && (mesh.has_uvs || self.no_uv_offer_dismissed) {
            return;
        }
        let name = mesh.name.clone();
        egui::Area::new(egui::Id::unique("unwrap card"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_TOP)
            // Below the brush bar, centred in the free space like the view bar.
            .fixed_pos(egui::pos2((vp.left() + 92.0 + vp.right() - 352.0) * 0.5, vp.top() + 120.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::symmetric(18, 14)).show(ui, |ui| {
                    ui.set_width(380.0);
                    match job {
                        Some((mode, progress)) => {
                            let title = mode.doing();
                            ui.label(egui::RichText::new(format!("{title} {name}…")).font(theme::semibold(14.0)));
                            ui.add_space(8.0);
                            let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), Sense::hover());
                            ui.painter().rect_filled(bar, CornerRadius::same(3), TRACK);
                            let done = Rect::from_min_size(bar.min, egui::vec2(bar.width() * progress.fraction().clamp(0.02, 1.0), bar.height()));
                            ui.painter().rect_filled(done, CornerRadius::same(3), ACCENT);
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(pct(progress.fraction())).font(theme::mono(12.0)).color(TEXT_3));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if widgets::button(ui, "Cancel", false, 32.0).clicked() {
                                        progress.cancel();
                                    }
                                });
                            });
                        }
                        None => {
                            ui.label(egui::RichText::new(format!("{name} has no UVs")).font(theme::semibold(14.0)));
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new("Paint is stored in a texture, so every part of the model needs a place on it. Auto-unwrap lays it out for you.")
                                    .color(TEXT_2),
                            );
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if widgets::button(ui, "Auto-unwrap", true, 36.0).clicked() {
                                    self.start_unwrap(unwrap::Mode::Unwrap);
                                }
                                if widgets::button(ui, "More ways", false, 36.0)
                                    .on_hover_text("Planar, box, cylinder and sphere layouts, in the UV workspace")
                                    .clicked()
                                {
                                    self.workspace = Workspace::Uv;
                                    self.no_uv_offer_dismissed = true;
                                }
                                if widgets::button(ui, "Not now", false, 36.0).clicked() {
                                    self.no_uv_offer_dismissed = true;
                                }
                            });
                        }
                    }
                });
            });
    }

    fn view_bar(&mut self, ctx: &egui::Context, vp: Rect) {
        // Centre in the space left free between the tool rail and the layers panel.
        let free_center_x = (vp.left() + 92.0 + vp.right() - 352.0) * 0.5;
        egui::Area::new(egui::Id::unique("view_bar"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(egui::pos2(free_center_x, vp.bottom() - 16.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin { left: 16, right: 6, top: 5, bottom: 5 }).show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        let status = match &self.mesh {
                            Some(m) => format!("{} tris · {} verts", fmt_count(m.triangle_count()), fmt_count(m.vertices.len())),
                            None => "No model".into(),
                        };
                        ui.label(egui::RichText::new(status).font(theme::mono(12.0)).color(TEXT_3));
                        ui.add_space(6.0);
                        widgets::segmented(
                            ui,
                            &mut self.view_mode,
                            &[
                                (ViewMode::Lit, "Lit"),
                                (ViewMode::Colour, "Colour"),
                                (ViewMode::UvCheck, "UV check"),
                                (ViewMode::Normals, "Normals"),
                            ],
                            36.0,
                        );
                    });
                });
            });
    }

    fn welcome(&mut self, ctx: &egui::Context, vp: Rect, now: f64) {
        egui::Area::new(egui::Id::unique("welcome")).order(Order::Foreground).pivot(Align2::CENTER_CENTER).fixed_pos(vp.center()).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(28)).show(ui, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(icons::BOX).font(theme::icon_font(34.0)).color(ACCENT));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Open a model to start painting").font(theme::semibold(18.0)));
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Drop a .glb, .gltf or .obj file anywhere in this window.").color(TEXT_3));
                    ui.add_space(16.0);
                    if widgets::button(ui, "Open model…", true, 40.0).clicked() {
                        self.open_dialog(now);
                    }
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Ctrl+O").font(theme::mono(11.0)).color(TEXT_3));
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("or try a shape").color(TEXT_3).size(12.0));
                    ui.add_space(4.0);
                    // Measured, so the pair sits exactly on the card's centre line.
                    let gap = 8.0;
                    let w = widgets::button_width(ui.ctx(), "Sphere") + gap + widgets::button_width(ui.ctx(), "Cube");
                    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::hover());
                    let centred = Rect::from_center_size(row.center(), egui::vec2(w, 34.0));
                    ui.scope_builder(UiBuilder::new().max_rect(centred).layout(Layout::left_to_right(Align::Center)), |ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        if widgets::button(ui, "Sphere", false, 34.0).clicked() {
                            self.set_mesh(mesh::sphere(96, 48));
                        }
                        if widgets::button(ui, "Cube", false, 34.0).clicked() {
                            self.set_mesh(mesh::cube());
                        }
                    });
                });
            });
        });
    }

    fn workspace_placard(&mut self, ctx: &egui::Context, vp: Rect) {
        let (title, body) = match self.workspace {
            Workspace::Materials => ("Material graph", "The node-based PBR editor comes after the paint loop works end to end."),
            Workspace::Paint | Workspace::Uv | Workspace::Export => return,
        };
        egui::Area::new(egui::Id::unique("placard"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_TOP)
            .fixed_pos(egui::pos2(vp.center().x, vp.top() + 16.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::symmetric(18, 12)).show(ui, |ui| {
                    ui.set_max_width(460.0);
                    ui.label(egui::RichText::new(title).font(theme::semibold(14.0)));
                    ui.label(egui::RichText::new(body).color(TEXT_2));
                });
            });
    }

    fn toast(&mut self, ctx: &egui::Context, vp: Rect, now: f64) {
        let Some((msg, at)) = &self.toast else { return };
        if now - at > 6.0 {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        egui::Area::new(egui::Id::unique("toast"))
            .order(Order::Tooltip)
            // Centred just above the view bar, so it never covers it on narrow windows.
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(egui::pos2((vp.left() + 92.0 + vp.right() - 352.0) * 0.5, vp.bottom() - 16.0 - 48.0 - 10.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(egui::RichText::new(msg).color(TEXT));
                });
            });
    }
}

impl eframe::App for UmberApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        GROUND.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        self.now = now;
        self.handle_shortcuts(&ctx, now);
        self.poll_unwrap(&ctx);
        self.poll_export(&ctx);

        egui::Panel::top("top_bar")
            .exact_size(52.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(TOPBAR).inner_margin(Margin::symmetric(16, 0)))
            .show(ui, |ui| self.top_bar(ui, now));

        let vp = egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(VIEWPORT))
            .show(ui, |ui| self.viewport(ui))
            .inner;

        self.tool_rail(&ctx, vp);
        if self.workspace == Workspace::Paint {
            self.brush_bar(&ctx, vp);
        }
        if self.workspace == Workspace::Uv {
            self.uv_panel(&ctx, vp);
        } else if self.workspace == Workspace::Export {
            self.export_panel(&ctx, vp);
        } else {
            self.layers_panel(&ctx, vp);
        }
        self.view_bar(&ctx, vp);
        if self.mesh.is_none() {
            self.welcome(&ctx, vp, now);
        } else {
            self.workspace_placard(&ctx, vp);
            self.unwrap_card(&ctx, vp);
        }
        self.toast(&ctx, vp, now);

        // Anything the panels changed becomes one undo step once the pointer lets go.
        if !ctx.input(|i| i.pointer.any_down()) {
            self.history.record_stack_changes(&mut self.sets);
        }
    }
}

// ---------------------------------------------------------------- helpers

fn slider_with_value(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
    fmt: impl Fn(f32) -> String,
) {
    widgets::caps_label(ui, label);
    widgets::track_slider(ui, value, range, width);
    let text = egui::RichText::new(fmt(*value)).font(theme::mono(12.0));
    ui.add_sized([44.0, 20.0], egui::Label::new(text));
}

fn labelled_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
    fmt: impl Fn(f32) -> String,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(TEXT_2));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(egui::RichText::new(fmt(*value)).font(theme::mono(12.0)));
        });
    });
    let r = widgets::track_slider(ui, value, range, width);
    ui.add_space(4.0);
    r
}

/// A quiet line of help text in a bar.
fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(TEXT_3).size(12.0));
}

/// A path with the home folder shown as ~.
fn home_relative(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) && path != home => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(path).display())
        }
        _ => path.display().to_string(),
    }
}

/// "Just now", "2 min ago", for the line under a button that has done its thing.
fn ago(seconds: f64) -> String {
    match seconds {
        s if s < 60.0 => "Just now.".to_owned(),
        s if s < 3600.0 => format!("{} min ago.", (s / 60.0) as u32),
        s => format!("{} h ago.", (s / 3600.0) as u32),
    }
}

fn pct(v: f32) -> String {
    // Small shares keep a decimal, so 0.4 % doesn't read as none.
    if v > 0.0 && v < 0.0995 { format!("{:.1}%", v * 100.0) } else { format!("{:.0}%", v * 100.0) }
}

fn panel_divider(ui: &mut egui::Ui) {
    ui.add_space(8.0);
    let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(line, CornerRadius::ZERO, PANEL_LINE);
    ui.add_space(8.0);
}

/// A menu entry with a one-line explanation under it.
fn menu_item(ui: &mut egui::Ui, title: &str, detail: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(240.0, 44.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), RAISED);
    }
    p.text(rect.left_top() + egui::vec2(10.0, 13.0), Align2::LEFT_CENTER, title, theme::medium(13.0), TEXT);
    p.text(rect.left_top() + egui::vec2(10.0, 31.0), Align2::LEFT_CENTER, detail, egui::FontId::proportional(12.0), TEXT_3);
    resp
}

/// One row in the layer stack. Returns true when the row was clicked.
fn layer_row(ui: &mut egui::Ui, index: usize, layer: &mut Layer, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 54.0), Sense::click());
    let p = ui.painter_at(rect.expand(1.0));
    if selected {
        p.rect(rect, CornerRadius::same(10), RAISED, Stroke::new(1.0, Color32::from_rgb(0x5a, 0x4a, 0x33)), egui::StrokeKind::Inside);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(10), Color32::from_rgb(0x22, 0x1e, 0x19));
    }
    let thumb = Rect::from_min_size(rect.min + egui::vec2(8.0, 8.0), Vec2::splat(38.0));
    match layer.thumb_colour() {
        Some(c) => {
            p.rect_filled(thumb, CornerRadius::same(7), c);
        }
        None => empty_thumb(&p, thumb),
    }
    if let LayerKind::Fill(_) = layer.kind {
        let badge = Rect::from_min_size(thumb.right_bottom() - Vec2::splat(15.0), Vec2::splat(15.0));
        p.rect_filled(badge, CornerRadius::same(4), PANEL);
        p.text(badge.center(), Align2::CENTER_CENTER, icons::FILL, theme::icon_font(10.0), ICON);
    }
    let x = thumb.right() + 10.0;
    let name_color = if layer.visible { TEXT } else { TEXT_3 };
    let name_w = rect.right() - 92.0 - x;
    let name = ui.fonts_mut(|f| {
        let galley = f.layout_no_wrap(layer.name.clone(), theme::medium(13.0), name_color);
        if galley.size().x <= name_w { galley } else {
            let mut job = egui::text::LayoutJob::simple_singleline(layer.name.clone(), theme::medium(13.0), name_color);
            job.wrap = egui::text::TextWrapping::truncate_at_width(name_w);
            f.layout_job(job)
        }
    });
    p.galley(egui::pos2(x, rect.top() + 18.0 - name.size().y * 0.5), name, name_color);
    p.text(egui::pos2(x, rect.top() + 36.0), Align2::LEFT_CENTER, layer.kind_label(), egui::FontId::proportional(12.0), TEXT_3);

    let eye_rect = Rect::from_center_size(egui::pos2(rect.right() - 22.0, rect.center().y), Vec2::splat(32.0));
    let eye = ui.interact(eye_rect, ui.scope_id().with(("layer_eye", index)), Sense::click());
    let eye_icon = if layer.visible { icons::EYE } else { icons::EYE_OFF };
    p.text(eye_rect.center(), Align2::CENTER_CENTER, eye_icon, theme::icon_font(16.0), if eye.hovered() { TEXT } else { ICON });
    if eye.on_hover_text(if layer.visible { "Hide layer" } else { "Show layer" }).clicked() {
        layer.visible = !layer.visible;
    }
    p.text(
        egui::pos2(eye_rect.left() - 4.0, rect.center().y),
        Align2::RIGHT_CENTER,
        pct(layer.opacity),
        theme::mono(12.0),
        TEXT_2,
    );
    resp.clicked()
}

/// Checkerboard for a layer with nothing painted yet.
fn empty_thumb(p: &egui::Painter, r: Rect) {
    p.rect_filled(r, CornerRadius::same(7), Color32::from_rgb(0x2a, 0x25, 0x1f));
    let cell = r.width() / 4.0;
    for y in 0..4 {
        for x in 0..4 {
            if (x + y) % 2 == 0 {
                let c = Rect::from_min_size(r.min + egui::vec2(x as f32 * cell, y as f32 * cell), Vec2::splat(cell));
                p.rect_filled(c.intersect(r.shrink(0.5)), CornerRadius::ZERO, Color32::from_rgb(0x36, 0x2f, 0x28));
            }
        }
    }
    p.rect_stroke(r, CornerRadius::same(7), Stroke::new(1.0, PANEL_LINE), egui::StrokeKind::Inside);
}

fn draw_logo_u(p: &egui::Painter, r: Rect) {
    let c = r.center();
    let pts: Vec<Pos2> = (0..=16)
        .map(|i| {
            let t = std::f32::consts::PI * i as f32 / 16.0;
            egui::pos2(c.x - 5.0 * t.cos(), c.y + 1.0 + 5.0 * t.sin())
        })
        .collect();
    let mut path = vec![egui::pos2(c.x - 5.0, c.y - 6.0)];
    path.extend(pts);
    path.push(egui::pos2(c.x + 5.0, c.y - 6.0));
    p.add(egui::Shape::line(path, Stroke::new(2.2, ON_ACCENT)));
}

/// The coverage map as an image: islands in brass, overlaps in red, the rest clear.
fn coverage_image(health: &Health) -> egui::ColorImage {
    let island = Color32::from_rgba_unmultiplied(0xc9, 0xa0, 0x63, 150);
    let pixels = health
        .map
        .iter()
        .map(|&c| match c {
            0 => Color32::TRANSPARENT,
            1 => island,
            _ => WARN,
        })
        .collect();
    egui::ColorImage::new([unwrap::MAP, unwrap::MAP], pixels)
}

/// What the numbers mean, in a sentence each, worst first.
fn uv_advice(h: &Health) -> Vec<String> {
    if h.unusable() {
        return vec!["No usable UVs yet. Unwrap lays the model out so it can be painted.".into()];
    }
    let mut out = Vec::new();
    if h.collapsed > 0 {
        out.push(format!("{} triangles have no room in the texture, so paint can't land on them.", fmt_count(h.collapsed)));
    }
    if h.overlap >= 0.005 {
        out.push(format!("{} of the layout overlaps, so paint there lands twice. Fine for mirrored parts; otherwise repack.", pct(h.overlap)));
    }
    if h.outside >= 0.001 {
        out.push(format!("{} of the layout sits outside the texture, where it repeats.", pct(h.outside)));
    }
    if h.stretch > 1.25 {
        out.push(format!("Dabs come out squashed on {} of the surface. Unwrapping with Auto cuts it into flatter pieces.", pct(h.badly_stretched)));
    }
    if h.density_spread > 2.0 {
        out.push(format!("Detail varies {:.1}× across the model, so some parts will look blurrier than others.", h.density_spread));
    }
    if h.used < 0.3 {
        out.push(format!("Only {} of the texture is used. Repack lays the islands out tighter, so paint gets more texels.", pct(h.used)));
    }
    if out.is_empty() {
        out.push("Healthy layout. Nothing needs fixing.".into());
    }
    out
}

fn fmt_count(n: usize) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f32 / 1000.0),
        _ => format!("{:.2}M", n as f32 / 1_000_000.0),
    }
}
