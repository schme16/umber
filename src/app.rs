//! Application state and the Graphite shell: top bar, full-bleed viewport, floating panels.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};

use eframe::egui_wgpu::{self, wgpu};
use eframe::egui_wgpu::wgpu::util::DeviceExt;
use egui::{Align, Align2, Color32, CornerRadius, Layout, Margin, Order, Pos2, Rect, Sense, Stroke, UiBuilder, Vec2};

use crate::brush::{self, BrushEngine, Dab};
use crate::brush_texture::{self, Projection, Source};
use crate::camera::{CameraPrefs, OrbitCamera, Pivot, Turning};
use crate::commands::{self, Cmd};
use crate::export;
use crate::help::{self, Tip};
use glam::Vec3;
use crate::history::{Applied, Change, History};
use crate::mesh::{self, Mesh};
use crate::pen::{PenInput, PressureCurve};
use crate::project;
use crate::stats::{self, Stats};
use crate::renderer::{FrameParams, ViewMode, ViewportRenderer};
use crate::texset::{self, BlendMode, Channels, Compositor, Fill, Layer, LayerKind, Mask, TextureSet};
use crate::theme::{self, *};
use crate::unwrap::{self, Health};
use crate::{icons, widgets};

/// Overlapping texels on the UV coverage map, and numbers worth a second look.
const WARN: Color32 = Color32::from_rgb(0xe0, 0x7d, 0x62);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Workspace {
    Paint,
    Uv,
    Materials,
    Export,
}

/// How the UV panel lays a model out. Each maps to an [`unwrap::Mode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum UnwrapMethod {
    Auto,
    Planar,
    Box,
    Cylinder,
    Sphere,
}

impl UnwrapMethod {
    pub(crate) const ALL: [(UnwrapMethod, &'static str); 5] = [
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tool {
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
    pub(crate) const ALL: [Tool; 8] =
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

    /// From the command registry, so the rail, the brush bar and the palette agree.
    fn name(self) -> &'static str {
        commands::get(Cmd::Tool(self)).name
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

/// What a stroke or fill can reach.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) enum Reach {
    /// Everything under the brush (for fills, the UV island).
    #[default]
    All,
    /// Only the face the stroke starts on: triangles that bend less than the edge angle from
    /// their neighbours.
    Face,
}

/// Dabs sit this fraction of the brush's diameter apart.
const SPACING: f32 = 0.1;
/// Seconds of no editing before unsaved work is copied to the recovery folder...
const AUTOSAVE_PAUSE: f64 = 2.0;
/// ...and the least time between copies.
const AUTOSAVE_EVERY: f64 = 30.0;

/// Saved in project files, all but the pen curve (that belongs to the tablet, not the work).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    #[serde(skip)]
    curve: PressureCurve,
    paint_colour: bool,
    paint_rough: bool,
    paint_metal: bool,
    paint_height: bool,
    /// Paint with `texture` rather than the colour.
    use_texture: bool,
    /// The last texture chosen, kept while painting with colour so switching back is one tap.
    texture: Option<Source>,
    projection: Projection,
    /// Times the projection's own size; bigger is a bigger pattern.
    texture_scale: f32,
    /// Degrees.
    texture_turn: f32,
    stroke_reach: Reach,
    fill_reach: Reach,
    /// Neighbouring triangles that bend more than this many degrees are different faces.
    edge_angle: f32,
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
            use_texture: false,
            texture: None,
            projection: Projection::Surface,
            texture_scale: 1.0,
            texture_turn: 0.0,
            stroke_reach: Reach::All,
            fill_reach: Reach::All,
            edge_angle: 40.0,
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
    /// The triangles this stroke may paint, when it keeps to one face.
    only: Option<(wgpu::Buffer, u32)>,
    /// Keeping to one face, but the stroke hasn't touched the model yet.
    finding_face: bool,
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

/// A project save running on a worker thread.
struct SaveJob {
    path: PathBuf,
    /// The history state it holds.
    state: u64,
    /// The work it belongs to (see [`UmberApp::document`]).
    document: u64,
    done: Arc<AtomicU32>,
    steps: u32,
    result: mpsc::Receiver<anyhow::Result<()>>,
}

/// Something that replaces the open work, held while "Save changes?" is asked.
enum Pending {
    Open(PathBuf),
    Close,
}

enum LayerAction {
    AddMask,
    Up,
    Down,
    Duplicate,
    Delete,
}

/// How far back a release looks to see whether the drag was a flick, in seconds.
const FLICK_WINDOW: f64 = 0.08;
/// How quickly a flicked model slows down: its speed falls by e every 1/SPIN_DECAY seconds.
const SPIN_DECAY: f32 = 6.5;

/// A camera drag in progress, and the spin a flick leaves after it.
#[derive(Default)]
struct Nav {
    dragging: bool,
    /// What the model turns about, chosen as the drag starts.
    pivot: Vec3,
    /// How deep the grabbed point is, so a pan keeps it under the cursor.
    depth: f32,
    /// Recent turning moves as (time, points), to tell a flick from a stop.
    recent: Vec<(f64, Vec2)>,
    /// Points per second the model keeps turning after a flick.
    spin: Vec2,
    /// Where the scroll wheel last zoomed: the cursor, and the point under it.
    zoom_at: Option<(Pos2, Vec3)>,
}

/// The speed a drag let go at, in points per second, if it was still moving fast: a drag that
/// stopped before letting go doesn't spin.
fn flick(recent: &[(f64, Vec2)], now: f64) -> Vec2 {
    let window = 0.05;
    let moves: Vec<_> = recent.iter().filter(|(t, d)| now - t < window && *d != Vec2::ZERO).collect();
    if moves.len() < 2 {
        return Vec2::ZERO;
    }
    let span = (now - moves[0].0).max(1.0 / 120.0) as f32;
    let v = moves.iter().fold(Vec2::ZERO, |a, (_, d)| a + *d) / span;
    let speed = v.length();
    if speed > 300.0 { v * (speed.min(4000.0) / speed) } else { Vec2::ZERO }
}

pub struct UmberApp {
    ctx: egui::Context,
    rs: egui_wgpu::RenderState,
    renderer: ViewportRenderer,
    camera: OrbitCamera,
    /// How dragging and scrolling move the camera, remembered between sessions.
    cam_prefs: CameraPrefs,
    /// A turn or pan in progress, or the spin a flick left behind.
    nav: Nav,
    camera_open: bool,
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
    /// The brush texture on the GPU, what it was made from, and its thumbnail.
    brush_texture: Option<(Source, egui::TextureHandle, u64)>,
    /// Thumbnails for the texture picker, made the first time it shows each one, and the mesh
    /// generation the model's own textures were taken from.
    texture_thumbs: Vec<(Source, egui::TextureHandle)>,
    thumbs_generation: u64,
    stroke: Option<StrokeState>,
    /// UV island of every triangle, worked out the first time the fill tool needs it.
    islands: Option<Vec<u32>>,
    /// Face of every triangle, for the edge angle it was worked out at.
    faces: Option<(f32, Vec<u32>)>,
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

    /// Where Ctrl+S writes; `None` until the work is saved once.
    project_path: Option<PathBuf>,
    /// The history state as last saved (or opened). Anything else is unsaved.
    saved_state: u64,
    /// Bumped whenever different work is opened, so a save finishing late knows it's stale.
    document: u64,
    save_job: Option<SaveJob>,
    /// Unsaved work is copied here now and then, for the welcome card to offer after a crash.
    recovery_path: Option<PathBuf>,
    /// The history state the recovery copy holds, if one has been written.
    recovery_state: Option<u64>,
    autosave_job: Option<SaveJob>,
    /// When the history state last changed (state, time), so autosave waits for a pause.
    last_change: (u64, f64),
    last_autosave: f64,
    /// Unsaved work an earlier Umber left behind, offered on the welcome card.
    recoverable: Vec<project::Recoverable>,
    /// Waiting on "Save changes?"; the flag is set once Save is chosen and the save runs.
    pending: Option<(Pending, bool)>,
    /// The close was agreed to; let it through.
    closing: bool,
    /// The window title as last set.
    title: String,
    /// Umber's own window, so file pickers open on top of it rather than behind.
    parent: Option<Parent>,

    palette: help::Palette,
    shortcuts_open: bool,
    /// Space is down: a left drag turns the model.
    space_held: bool,
    tips: help::Tips,
    /// The F3 overlay and the numbers behind it.
    stats: Stats,
}

impl UmberApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        theme::install(&cc.egui_ctx);
        let rs = cc.wgpu_render_state.clone().expect("Umber needs the wgpu renderer");
        let info = rs.adapter.get_info();
        log::info!("GPU: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
        stats::install_gpu_timer(&rs.device, &rs.queue);
        let tips = help::Tips::load();
        let stats = Stats::new(tips.stats_open(), &info);
        let renderer = ViewportRenderer::new(&rs);
        let compositor = Compositor::new(&rs.device, &rs.queue);
        let brush_engine = BrushEngine::new(&rs.device, &rs.queue);
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            rs,
            renderer,
            camera: OrbitCamera::default(),
            cam_prefs: tips.camera(),
            nav: Nav::default(),
            camera_open: false,
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
            brush_texture: None,
            texture_thumbs: Vec::new(),
            thumbs_generation: 0,
            stroke: None,
            islands: None,
            faces: None,
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
            project_path: None,
            saved_state: 0,
            document: 0,
            save_job: None,
            recovery_path: None,
            recovery_state: None,
            autosave_job: None,
            last_change: (0, 0.0),
            last_autosave: f64::NEG_INFINITY,
            recoverable: project::recoverable(),
            pending: None,
            closing: false,
            title: String::new(),
            parent: None,
            palette: help::Palette::default(),
            shortcuts_open: false,
            space_held: false,
            tips,
            stats,
        };
        project::sweep_recovery_dir();
        if let Some(path) = initial_file {
            app.open_now(&path);
        }
        app
    }

    fn set_mesh(&mut self, mesh: Mesh) {
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let sets = mesh.materials.iter().map(|m| self.compositor.new_set(device, queue, m)).collect();
        self.install(mesh, sets);
    }

    /// Make this the open work: a model with its texture sets, nothing to undo, nothing unsaved.
    fn install(&mut self, mesh: Mesh, sets: Vec<TextureSet>) {
        if self.stroke.take().is_some() {
            self.brush_engine.end(&self.rs.device, &self.rs.queue);
        }
        if let Some(job) = self.unwrap_job.take() {
            job.progress.cancel();
        }
        self.sets = sets;
        self.active_set = 0;
        self.history.clear();
        self.saved_state = self.history.state();
        self.last_change = (self.saved_state, self.now);
        self.document += 1;
        self.project_path = None;
        self.recovery_path = None;
        self.recovery_state = None;
        self.model_path = None;
        self.last_export = None;
        let (center, radius) = mesh.bounding_sphere();
        self.camera = OrbitCamera::default();
        self.camera.frame(center, radius);
        self.nav = Nav::default();
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
        self.faces = None;
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
        let mut dialog = self.dialog().set_title("Export to which folder? One inside your Godot project works best");
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

    // ---------------------------------------------------------------- open and save

    /// Open a model or a project, asking about unsaved changes first.
    fn open_path(&mut self, path: &Path) {
        self.tips.remember_folder(path);
        self.replace_work(Pending::Open(path.to_owned()));
    }

    fn open_dialog(&mut self) {
        let mut extensions = vec![project::EXTENSION];
        extensions.extend_from_slice(mesh::SUPPORTED_EXTENSIONS);
        let mut dialog = self.dialog().set_title("Open a model or project").add_filter("Models and Umber projects", &extensions);
        if let Some(dir) = self.start_folder() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_path(&path);
        }
    }

    /// Do `then` now if nothing is unsaved, otherwise ask first.
    fn replace_work(&mut self, then: Pending) {
        if self.unsaved() {
            self.pending = Some((then, false));
        } else {
            self.carry_out(then);
        }
    }

    /// The open work is saved or let go: do what was waiting.
    fn carry_out(&mut self, then: Pending) {
        self.pending = None;
        self.discard_recovery();
        match then {
            Pending::Open(path) => self.open_now(&path),
            Pending::Close => {
                self.closing = true;
                // A copy half written now would come back as unsaved work next time.
                if let Some(job) = self.autosave_job.take() {
                    let _ = job.result.recv();
                    let _ = std::fs::remove_file(&job.path);
                }
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn open_now(&mut self, path: &Path) {
        if project::is_project(path) {
            self.open_project(path, false);
            return;
        }
        match mesh::load(path) {
            Ok(mesh) => {
                // A model without UVs gets the unwrap card instead.
                let note = mesh.has_uvs.then(|| format!("Opened {} · {} triangles", mesh.name, fmt_count(mesh.triangle_count())));
                self.set_mesh(mesh);
                self.model_path = Some(path.to_owned());
                self.toast = note.map(|n| (n, self.now));
            }
            Err(e) => {
                log::error!("{e:#}");
                self.toast = Some((format!("{e:#}"), self.now));
            }
        }
    }

    /// Open a project file, or (`recovered`) unsaved work from the recovery folder.
    fn open_project(&mut self, path: &Path, recovered: bool) {
        let started = std::time::Instant::now();
        let loaded = project::read(path).map(|l| l.into_sets(&self.rs.device, &self.rs.queue, &self.compositor));
        let (mesh, settings, sets) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                log::error!("{e:#}");
                self.toast = Some((format!("{e:#}"), self.now));
                return;
            }
        };
        log::info!("opened {} in {:.0?}", path.display(), started.elapsed());
        self.install(mesh, sets);
        let saved_as = settings.project_path.clone();
        self.apply_settings(settings);
        self.recoverable.retain(|r| r.path != path);
        if recovered {
            // Unsaved until it is saved somewhere real; autosave carries on into the same copy.
            self.project_path = saved_as;
            self.saved_state = u64::MAX;
            self.recovery_path = Some(path.to_owned());
            self.recovery_state = Some(self.history.state());
            self.toast = Some(("Recovered your unsaved work. Save it to keep it (Ctrl+S).".into(), self.now));
        } else {
            self.project_path = Some(path.to_owned());
            self.toast = Some((format!("Opened {}", project::display_name(path)), self.now));
        }
    }

    /// The camera, brush, colours, view and export choices a project file brings back.
    fn apply_settings(&mut self, s: project::Settings) {
        if let Some(c) = s.camera {
            self.camera.target = Vec3::from(c.target);
            self.camera.distance = c.distance;
            self.camera.rot = match c.rot {
                Some(q) => glam::Quat::from_array(q).normalize(),
                None => OrbitCamera::level(c.yaw, c.pitch),
            };
            self.nav = Nav::default();
        }
        if let Ok(mut brush) = serde_json::from_value::<BrushSettings>(s.brush) {
            brush.curve = self.brush.curve;
            self.brush = brush;
        }
        let colour = |c: [u8; 4]| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]);
        if let Some(c) = s.primary {
            self.primary = colour(c);
        }
        if let Some(c) = s.secondary {
            self.secondary = colour(c);
        }
        self.active_set = s.active_set.min(self.sets.len().saturating_sub(1));
        let modes = [ViewMode::Lit, ViewMode::Colour, ViewMode::UvCheck, ViewMode::Normals];
        if let Some(mode) = modes.into_iter().find(|m| format!("{m:?}") == s.view_mode) {
            self.view_mode = mode;
        }
        self.light_yaw = s.light_yaw.unwrap_or(self.light_yaw);
        self.exposure = s.exposure.unwrap_or(self.exposure);
        self.model_path = s.model_path;
        if s.export_dir.is_some() {
            self.export_dir = s.export_dir;
        }
        if let Some(size) = s.export_size.filter(|s| export::SIZES.contains(s)) {
            self.export_size = size;
        }
    }

    fn settings(&self) -> project::Settings {
        let c = &self.camera;
        project::Settings {
            camera: Some({
                let (yaw, pitch) = c.yaw_pitch();
                project::CameraInfo { target: c.target.into(), distance: c.distance, yaw, pitch, rot: Some(c.rot.to_array()) }
            }),
            brush: serde_json::to_value(&self.brush).unwrap_or_default(),
            primary: Some(self.primary.to_array()),
            secondary: Some(self.secondary.to_array()),
            active_set: self.active_set,
            view_mode: format!("{:?}", self.view_mode),
            light_yaw: Some(self.light_yaw),
            exposure: Some(self.exposure),
            model_path: self.model_path.clone(),
            export_dir: self.export_dir.clone(),
            export_size: Some(self.export_size),
            project_path: None,
        }
    }

    /// Texture memory the open work holds, for the F3 overlay. Layers shared between the
    /// stack and its undo steps count once, under layers.
    fn memory(&self) -> stats::Memory {
        let mut seen = std::collections::HashSet::new();
        let mut layers = 0;
        for layer in self.sets.iter().flat_map(|s| &s.layers) {
            if let Some(p) = &layer.pixels {
                if seen.insert(Rc::as_ptr(p) as usize) {
                    layers += p.bytes();
                }
            }
            if let Some(m) = &layer.mask {
                if seen.insert(Rc::as_ptr(m) as usize) {
                    layers += stats::texture_bytes(&m.texture);
                }
            }
        }
        stats::Memory { layers, maps: self.sets.iter().map(TextureSet::map_bytes).sum(), undo: self.history.bytes() }
    }

    fn unsaved(&self) -> bool {
        self.mesh.is_some() && self.history.state() != self.saved_state
    }

    /// The project's file name once saved, the model's name before that.
    fn work_name(&self) -> String {
        match (&self.project_path, &self.mesh) {
            (Some(p), _) => p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            (None, Some(m)) => m.name.clone(),
            (None, None) => String::new(),
        }
    }

    /// Ctrl+S, or with `choose` Ctrl+Shift+S. Returns whether a save is now running.
    fn save(&mut self, choose: bool) -> bool {
        if self.mesh.is_none() {
            self.toast = Some(("Open a model first; then there's something to save.".into(), self.now));
            return false;
        }
        if self.save_job.is_some() {
            return true;
        }
        let path = match (&self.project_path, choose) {
            (Some(p), false) => p.clone(),
            _ => match self.pick_save_path() {
                Some(p) => {
                    self.tips.remember_folder(&p);
                    p
                }
                None => return false,
            },
        };
        self.start_save(path, false);
        true
    }

    /// Where Open and Save as start: the folder last opened from or saved to, else the open
    /// work's own folder.
    fn start_folder(&self) -> Option<PathBuf> {
        let own = self.project_path.as_ref().or(self.model_path.as_ref()).and_then(|p| p.parent());
        self.tips.last_folder().or(own).map(Path::to_owned)
    }

    fn pick_save_path(&self) -> Option<PathBuf> {
        // Scripted captures set this, since a save dialog would open on the real desktop.
        if let Some(path) = std::env::var_os("UMBER_SAVE_PATH") {
            return Some(PathBuf::from(path));
        }
        let name = match &self.project_path {
            Some(p) => project::display_name(p),
            None => format!("{}.{}", self.work_name(), project::EXTENSION),
        };
        let dir = self.start_folder();
        let mut dialog = self.dialog().set_title("Save project").add_filter("Umber project", &[project::EXTENSION]).set_file_name(name);
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        let path = dialog.save_file()?;
        if project::is_project(&path) {
            return Some(path);
        }
        let mut with_ext = path.into_os_string();
        with_ext.push(format!(".{}", project::EXTENSION));
        Some(PathBuf::from(with_ext))
    }

    /// Copy the work on the GPU now, and compress and write it on a worker.
    fn start_save(&mut self, path: PathBuf, recovery: bool) {
        self.end_stroke();
        self.history.record_stack_changes(&mut self.sets);
        let Some(mesh) = &self.mesh else { return };
        let mut settings = self.settings();
        if recovery {
            settings.project_path = self.project_path.clone();
        }
        let snapshot = project::Snapshot::take(&self.rs.device, &self.rs.queue, mesh, &self.sets, settings);
        let (done, steps, result) = project::save_in_background(snapshot, path.clone());
        let job = SaveJob { path, state: self.history.state(), document: self.document, done, steps, result };
        if recovery {
            self.autosave_job = Some(job);
        } else {
            self.save_job = Some(job);
        }
    }

    fn poll_saves(&mut self, ctx: &egui::Context) {
        if let Some((job, outcome)) = finished(&mut self.save_job, ctx) {
            match outcome {
                Ok(()) if job.document == self.document => {
                    self.saved_state = job.state;
                    self.project_path = Some(job.path.clone());
                    self.toast = Some((format!("Saved {}", home_relative(&job.path)), self.now));
                    if !self.unsaved() {
                        self.discard_recovery();
                    }
                    if let Some((then, true)) = self.pending.take() {
                        self.carry_out(then);
                    }
                }
                // Work that has since been closed; nothing here to update.
                Ok(()) => {}
                Err(e) => {
                    log::error!("{e:#}");
                    self.toast = Some((format!("Couldn't save: {e:#}"), self.now));
                    // Ask again rather than carry on as if it had saved.
                    if let Some((_, waiting)) = &mut self.pending {
                        *waiting = false;
                    }
                }
            }
        }
        if let Some((job, outcome)) = finished(&mut self.autosave_job, ctx) {
            match outcome {
                Ok(()) if job.document == self.document && self.unsaved() => self.recovery_state = Some(job.state),
                // Saved or let go while it was writing: the copy isn't needed.
                Ok(()) => {
                    let _ = std::fs::remove_file(&job.path);
                }
                Err(e) => log::warn!("couldn't keep a recovery copy: {e:#}"),
            }
        }
    }

    /// Keep a recovery copy of unsaved work: after a short pause in editing, at most every
    /// [`AUTOSAVE_EVERY`] seconds, never mid-stroke.
    fn autosave(&mut self, ctx: &egui::Context) {
        let state = self.history.state();
        if state != self.last_change.0 {
            self.last_change = (state, self.now);
        }
        if !self.unsaved() || self.recovery_state == Some(state) || self.autosave_job.is_some() || self.stroke.is_some() || self.closing {
            return;
        }
        if ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        let wait = (AUTOSAVE_PAUSE - (self.now - self.last_change.1)).max(AUTOSAVE_EVERY - (self.now - self.last_autosave));
        if wait > 0.0 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(wait));
            return;
        }
        let Some(path) = self.recovery_path.clone().or_else(|| project::new_recovery_path(&self.work_name())) else { return };
        self.recovery_path = Some(path.clone());
        self.last_autosave = self.now;
        self.start_save(path, true);
    }

    /// The work is saved or let go: its recovery copy goes.
    fn discard_recovery(&mut self) {
        if self.recovery_state.take().is_some() {
            if let Some(path) = &self.recovery_path {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// "Save changes?" while something waits to replace the open work.
    fn save_changes_modal(&mut self, ctx: &egui::Context) {
        let Some((then, waiting)) = &self.pending else { return };
        let waiting = *waiting;
        let doing = match then {
            Pending::Open(_) => "Opening another file closes this one.",
            Pending::Close => "Umber is closing.",
        };
        let name = self.work_name();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("save-changes")).frame(theme::panel_frame().inner_margin(Margin::same(24))).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(egui::RichText::new(format!("Save changes to {name}?")).font(theme::semibold(17.0)));
            ui.add_space(6.0);
            let note = if waiting { "Saving…".to_owned() } else { format!("{doing} Changes since the last save are lost if you don't save them.") };
            ui.label(egui::RichText::new(note).color(TEXT_2));
            ui.add_space(20.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if widgets::button(ui, if waiting { "Saving…" } else { "Save" }, true, 36.0).clicked() && !waiting {
                    choice = Some(0);
                }
                if widgets::button(ui, "Don't save", false, 36.0).clicked() {
                    choice = Some(1);
                }
                if widgets::button(ui, "Cancel", false, 36.0).clicked() {
                    choice = Some(2);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(2);
        }
        match choice {
            Some(0) => {
                if self.save(false) {
                    if let Some((_, waiting)) = &mut self.pending {
                        *waiting = true;
                    }
                }
            }
            Some(1) => {
                if let Some((then, _)) = self.pending.take() {
                    self.carry_out(then);
                }
            }
            Some(_) => self.pending = None,
            None => {}
        }
    }

    /// The window close button: ask about unsaved work, otherwise tidy up and go.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.closing || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.unsaved() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.pending.is_none() {
                self.pending = Some((Pending::Close, false));
            }
        } else {
            self.carry_out(Pending::Close);
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match &self.mesh {
            None => "Umber".to_owned(),
            Some(_) if self.unsaved() => format!("● {} · Umber", self.work_name()),
            Some(_) => format!("{} · Umber", self.work_name()),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| Some(f.path().to_path_buf())).collect());
        if let Some(path) = dropped.first() {
            self.open_path(path);
        }
        // Every key comes from the command registry. Only typing in a text field holds back the
        // plain keys and undo (the field has its own); a clicked button or slider also holds
        // egui focus, and must not swallow them.
        let typing = ctx.text_edit_focused();
        // Space is held for navigation, never sent on to the focused button (egui presses it).
        self.space_held = !typing && ctx.input_mut(|i| {
            while i.consume_key(egui::Modifiers::NONE, egui::Key::Space) {}
            i.key_down(egui::Key::Space)
        });
        let pressed: Vec<(egui::Key, egui::Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, mods) in pressed {
            let Some(c) = commands::COMMANDS.iter().find(|c| c.keys.iter().any(|k| k.matches(key, mods))) else { continue };
            let k = c.keys.iter().find(|k| k.matches(key, mods)).expect("just matched");
            if typing && (!k.ctrl || matches!(c.cmd, Cmd::Undo | Cmd::Redo)) {
                continue;
            }
            // While the palette or the sheet is up, only the keys with Ctrl (and ? to close the sheet) work.
            if (self.palette.open || self.shortcuts_open) && !k.ctrl && c.cmd != Cmd::Shortcuts {
                continue;
            }
            match self.unavailable(c.cmd) {
                None => self.run(c.cmd),
                Some(why) => self.toast = Some((format!("{}: {why}.", c.name.trim_end_matches('…')), self.now)),
            }
        }
    }

    // ---------------------------------------------------------------- commands

    /// Why `cmd` can't run right now, in a few plain words, or `None` if it can.
    fn unavailable(&self, cmd: Cmd) -> Option<String> {
        let model = self.mesh.as_ref();
        let no_model = || Some("Open a model first".to_owned());
        let set = self.sets.get(self.active_set);
        let layer = set.and_then(|s| s.layers.get(s.selected));
        match cmd {
            Cmd::Save | Cmd::SaveAs if model.is_none() => no_model(),
            Cmd::Save | Cmd::SaveAs if self.save_job.is_some() => Some("Already saving".into()),
            Cmd::Export if model.is_none() => no_model(),
            Cmd::Export if !model.is_some_and(|m| m.has_uvs) => Some("Unwrap it first, so its maps line up".into()),
            Cmd::Export if self.export_job.is_some() => Some("Already exporting".into()),
            Cmd::Undo if !self.history.can_undo() => Some("Nothing to undo".into()),
            Cmd::Redo if !self.history.can_redo() => Some("Nothing to redo".into()),
            Cmd::Frame if model.is_none() => no_model(),
            Cmd::NewPaintLayer | Cmd::NewFillLayer if set.is_none() => no_model(),
            Cmd::AddMask | Cmd::DuplicateLayer | Cmd::DeleteLayer | Cmd::LayerUp | Cmd::LayerDown if set.is_none() => no_model(),
            Cmd::AddMask | Cmd::DuplicateLayer | Cmd::DeleteLayer | Cmd::LayerUp | Cmd::LayerDown if layer.is_none() => {
                Some("Select a layer first".into())
            }
            Cmd::AddMask if layer.is_some_and(|l| l.mask.is_some()) => Some("This layer has a mask already".into()),
            Cmd::LayerUp if set.is_some_and(|s| s.selected + 1 >= s.layers.len()) => Some("It's the top layer already".into()),
            Cmd::LayerDown if set.is_some_and(|s| s.selected == 0) => Some("It's the bottom layer already".into()),
            Cmd::Unwrap(_) | Cmd::Repack if model.is_none() => no_model(),
            Cmd::Unwrap(_) | Cmd::Repack if self.unwrap_job.is_some() => Some("Already working on it".into()),
            Cmd::Repack if !model.is_some_and(|m| m.has_uvs) => Some("Needs UVs to repack. Unwrap makes them".into()),
            Cmd::Repack if self.uv_health.as_ref().is_some_and(|(s, h, _)| *s == self.active_set && h.unusable()) => {
                Some("Needs UVs to repack. Unwrap makes them".into())
            }
            Cmd::ResetTips if !self.tips.any_hidden() => Some("Every tip is still on".into()),
            _ => None,
        }
    }

    /// For commands that choose something: whether it's the current choice.
    fn cmd_on(&self, cmd: Cmd) -> Option<bool> {
        let b = &self.brush;
        Some(match cmd {
            Cmd::Tool(t) => self.tool == t,
            Cmd::View(v) => self.view_mode == v,
            Cmd::Workspace(w) => self.workspace == w,
            Cmd::PaintColour => b.paint_colour,
            Cmd::PaintRough => b.paint_rough,
            Cmd::PaintMetal => b.paint_metal,
            Cmd::PaintHeight => b.paint_height,
            Cmd::BrushTexture => b.use_texture,
            Cmd::ReachAll => b.stroke_reach == Reach::All,
            Cmd::ReachFace => b.stroke_reach == Reach::Face,
            Cmd::Stats => self.stats.open,
            Cmd::TurnAbout(p) => self.cam_prefs.pivot == p,
            Cmd::Turning(t) => self.cam_prefs.turning == t,
            Cmd::ZoomToCursor => self.cam_prefs.zoom_to_cursor,
            Cmd::SpinOnFlick => self.cam_prefs.momentum,
            _ => return None,
        })
    }

    fn run(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Open => self.open_dialog(),
            Cmd::Save => {
                self.save(false);
            }
            Cmd::SaveAs => {
                self.save(true);
            }
            Cmd::Export => self.export_to_godot(),
            Cmd::ExportFolder => {
                self.pick_export_dir();
            }
            // Goes through handle_close like the window's close button.
            Cmd::Quit => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Cmd::Undo => self.undo(),
            Cmd::Redo => self.redo(),
            Cmd::Tool(t) => self.tool = t,
            Cmd::BrushSmaller => self.brush.size = (self.brush.size / 1.15).max(1.0),
            Cmd::BrushBigger => self.brush.size = (self.brush.size * 1.15).min(1000.0),
            Cmd::SwapColours => std::mem::swap(&mut self.primary, &mut self.secondary),
            Cmd::ColourPanel => {
                self.workspace = Workspace::Paint;
                self.colour_open = true;
            }
            Cmd::BrushTexture => {
                self.workspace = Workspace::Paint;
                self.colour_open = true;
                self.brush.use_texture = true;
                self.sync_brush_texture();
            }
            Cmd::OpenBrushTexture => self.open_brush_texture(),
            Cmd::ReachAll => self.brush.stroke_reach = Reach::All,
            Cmd::ReachFace => self.brush.stroke_reach = Reach::Face,
            Cmd::PenSettings => {
                // The curve lives in the brush bar, which shows for the painting tools.
                self.workspace = Workspace::Paint;
                if !self.tool.strokes() {
                    self.tool = Tool::Brush;
                }
                self.pen_open = true;
            }
            Cmd::PaintColour => self.brush.paint_colour = !self.brush.paint_colour,
            Cmd::PaintRough => self.brush.paint_rough = !self.brush.paint_rough,
            Cmd::PaintMetal => self.brush.paint_metal = !self.brush.paint_metal,
            Cmd::PaintHeight => self.brush.paint_height = !self.brush.paint_height,
            Cmd::NewPaintLayer => self.add_layer(false),
            Cmd::NewFillLayer => self.add_layer(true),
            Cmd::AddMask => self.layer_action(LayerAction::AddMask),
            Cmd::DuplicateLayer => self.layer_action(LayerAction::Duplicate),
            Cmd::DeleteLayer => self.layer_action(LayerAction::Delete),
            Cmd::LayerUp => self.layer_action(LayerAction::Up),
            Cmd::LayerDown => self.layer_action(LayerAction::Down),
            Cmd::Frame => {
                if let Some(m) = &self.mesh {
                    let (c, r) = m.bounding_sphere();
                    self.camera.frame(c, r);
                    self.nav = Nav::default();
                }
            }
            Cmd::CameraSettings => self.camera_open = true,
            Cmd::TurnAbout(p) => self.set_cam_prefs(CameraPrefs { pivot: p, ..self.cam_prefs }),
            Cmd::Turning(t) => self.set_cam_prefs(CameraPrefs { turning: t, ..self.cam_prefs }),
            Cmd::ZoomToCursor => self.set_cam_prefs(CameraPrefs { zoom_to_cursor: !self.cam_prefs.zoom_to_cursor, ..self.cam_prefs }),
            Cmd::SpinOnFlick => self.set_cam_prefs(CameraPrefs { momentum: !self.cam_prefs.momentum, ..self.cam_prefs }),
            Cmd::View(v) => {
                self.view_mode = v;
                // The UV workspace always shows the checker, so go where the view applies.
                if self.workspace == Workspace::Uv {
                    self.workspace = Workspace::Paint;
                }
            }
            Cmd::Workspace(w) => self.workspace = w,
            Cmd::Unwrap(method) => {
                self.unwrap_method = method;
                self.workspace = Workspace::Uv;
                self.start_unwrap(self.chosen_unwrap());
            }
            Cmd::Repack => {
                self.workspace = Workspace::Uv;
                self.start_unwrap(unwrap::Mode::Repack);
            }
            Cmd::Palette => {
                self.shortcuts_open = false;
                self.palette.toggle();
            }
            Cmd::Shortcuts => {
                self.palette.open = false;
                self.shortcuts_open = !self.shortcuts_open;
            }
            Cmd::Stats => {
                self.stats.toggle();
                self.tips.set_stats_open(self.stats.open);
            }
            Cmd::ResetTips => {
                self.tips.reset();
                self.toast = Some(("Tips are back on. Each shows once, the first time it applies.".into(), self.now));
            }
        }
    }

    /// The tooltip for a button that runs `cmd`: its name and key, and why not if it can't run.
    fn tip(&self, cmd: Cmd) -> String {
        match self.unavailable(cmd) {
            Some(why) => format!("{}\n{why}", commands::tip(cmd)),
            None => commands::tip(cmd),
        }
    }

    fn layer_action(&mut self, action: LayerAction) {
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let Some(set) = self.sets.get_mut(self.active_set) else { return };
        let (i, n) = (set.selected, set.layers.len());
        if i >= n {
            return;
        }
        match action {
            LayerAction::Up if i + 1 < n => {
                set.layers.swap(i, i + 1);
                set.selected = i + 1;
            }
            LayerAction::Down if i > 0 => {
                set.layers.swap(i, i - 1);
                set.selected = i - 1;
            }
            LayerAction::Duplicate => {
                let copy = set.layers[i].duplicate(device, queue);
                set.layers.insert(i + 1, copy);
                set.selected = i + 1;
            }
            LayerAction::Delete => {
                set.layers.remove(i);
                set.selected = i.saturating_sub(1).min(set.layers.len().saturating_sub(1));
            }
            LayerAction::AddMask => {
                let layer = &mut set.layers[i];
                if layer.mask.is_none() {
                    layer.mask = Some(Rc::new(Mask::white(device, queue)));
                }
                self.tool = Tool::Mask;
            }
            _ => {}
        }
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

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, TOPBAR_LINE));

        ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)), |ui| {
            let (mark, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
            ui.painter().rect_filled(mark, CornerRadius::same(7), ACCENT);
            draw_logo_u(ui.painter(), mark);
            ui.label(egui::RichText::new("Umber").font(theme::semibold(14.0)));
            widgets::vdivider(ui, 18.0);
            if self.mesh.is_none() {
                ui.label(egui::RichText::new("No model open").color(TEXT_2));
                return;
            }
            let name = ui.label(egui::RichText::new(self.work_name()).color(TEXT_2));
            if let Some(path) = &self.project_path {
                name.on_hover_text(home_relative(path));
            }
            if let Some(job) = &self.save_job {
                let f = job.done.load(Ordering::Relaxed) as f32 / job.steps.max(1) as f32;
                ui.label(egui::RichText::new(format!("Saving {}", pct(f))).color(TEXT_3));
            } else if self.unsaved() {
                ui.label(egui::RichText::new("●").size(10.0).color(ACCENT)).on_hover_text(format!("Unsaved changes ({} saves)", commands::key_label(Cmd::Save).unwrap_or_default()));
            }
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
            let key = commands::key_label(Cmd::Export).unwrap_or_default();
            let tip = match (self.unavailable(Cmd::Export), &self.export_dir) {
                (Some(why), _) if exporting.is_none() => format!("{why}.\nThen this writes a .glb, a Godot material and its maps ({key})"),
                (_, Some(dir)) => format!("Export again to {} ({key})", home_relative(dir)),
                (_, None) => format!("Write a .glb, a Godot material and its maps ({key})"),
            };
            if widgets::button(ui, &label, exporting.is_none(), 34.0).on_hover_text(tip).clicked() && exporting.is_none() {
                self.export_to_godot();
            }
            ui.add_space(4.0);
            if widgets::icon_action(ui, icons::REDO, self.history.can_redo(), 36.0, &self.tip(Cmd::Redo)).clicked() {
                self.redo();
            }
            if widgets::icon_action(ui, icons::UNDO, self.history.can_undo(), 36.0, &self.tip(Cmd::Undo)).clicked() {
                self.undo();
            }
            let key = commands::key_label(Cmd::Save).unwrap_or_default();
            let save_tip = match (self.unavailable(Cmd::Save), &self.project_path) {
                (Some(why), _) => format!("Save ({key})\n{why}"),
                (None, Some(p)) => format!("Save to {} ({key})", home_relative(p)),
                (None, None) => format!("Save as a project, layers and all ({key})"),
            };
            let more = save_menu_arrow(ui, self.mesh.is_some());
            let mut chosen = None;
            egui::Popup::menu(&more).show(|ui| {
                if menu_item(ui, "Save", &commands::key_label(Cmd::Save).unwrap_or_default()).clicked() {
                    chosen = Some(Cmd::Save);
                }
                let save_as = format!("To a new file · {}", commands::key_label(Cmd::SaveAs).unwrap_or_default());
                if menu_item(ui, "Save as…", &save_as).clicked() {
                    chosen = Some(Cmd::SaveAs);
                }
            });
            // The arrow sits against Save, as one split button.
            let gap = std::mem::replace(&mut ui.spacing_mut().item_spacing.x, 0.0);
            if widgets::icon_action(ui, icons::SAVE, self.mesh.is_some(), 36.0, &save_tip).clicked() {
                chosen = Some(Cmd::Save);
            }
            ui.spacing_mut().item_spacing.x = gap;
            if let Some(cmd) = chosen {
                self.run(cmd);
            }
            if widgets::icon_button(ui, icons::FOLDER_OPEN, false, 36.0, &format!("Open a model or project ({})", commands::key_label(Cmd::Open).unwrap_or_default())).clicked() {
                self.open_dialog();
            }
            widgets::vdivider(ui, 18.0);
            let search = format!("Search commands ({})\nType what you want to do; every command is there", commands::key_label(Cmd::Palette).unwrap_or_default());
            if widgets::icon_button(ui, icons::SEARCH, self.palette.open, 36.0, &search).clicked() {
                self.run(Cmd::Palette);
            }
        });
    }

    // ---------------------------------------------------------------- viewport

    fn viewport(&mut self, ui: &mut egui::Ui) -> Rect {
        let rect = ui.max_rect();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();

        // Navigation: right, middle or a pen's extra button drag turns the model, Shift pans. For
        // pen-only use, holding Alt or Space turns a left drag into one too (Huion's driver can
        // send Space from a pen button).
        let (shift, alt, space) = ui.input(|i| (i.modifiers.shift, i.modifiers.alt, self.space_held));
        let hand = alt || space;
        let delta = response.drag_delta();
        let nav_drag = response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
            || response.dragged_by(egui::PointerButton::Extra1)
            || response.dragged_by(egui::PointerButton::Extra2)
            || (hand && self.stroke.is_none() && response.dragged_by(egui::PointerButton::Primary));
        let (now, dt, hover, pressed_any) =
            ui.input(|i| (i.time, i.stable_dt.min(0.05), i.pointer.hover_pos(), i.pointer.any_pressed()));
        if pressed_any && response.hovered() {
            // Touching the view again catches a spinning model.
            self.nav.spin = Vec2::ZERO;
        }
        if nav_drag {
            if !self.nav.dragging {
                let origin = ui.input(|i| i.pointer.press_origin()).or(hover);
                self.start_nav(origin, rect);
            }
            if delta != Vec2::ZERO {
                if shift {
                    self.camera.pan(delta.x, delta.y, rect.height(), self.nav.depth);
                } else {
                    self.camera.turn(delta.x, delta.y, self.nav.pivot, self.cam_prefs.turning);
                }
                self.nav.zoom_at = None;
            }
            self.nav.recent.push((now, if shift { Vec2::ZERO } else { delta }));
            self.nav.recent.retain(|(t, _)| now - t < FLICK_WINDOW);
        } else if self.nav.dragging {
            self.nav.dragging = false;
            if self.cam_prefs.momentum {
                self.nav.spin = flick(&self.nav.recent, now);
            }
            self.nav.recent.clear();
        }
        if !self.nav.dragging && self.nav.spin != Vec2::ZERO {
            let step = self.nav.spin * dt;
            self.camera.turn(step.x, step.y, self.nav.pivot, self.cam_prefs.turning);
            self.nav.spin *= (-dt * SPIN_DECAY).exp();
            if self.nav.spin.length() < 12.0 {
                self.nav.spin = Vec2::ZERO;
            }
            self.nav.zoom_at = None;
            ui.ctx().request_repaint();
        }
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let towards = if self.cam_prefs.zoom_to_cursor { hover.map(|p| self.zoom_anchor(p, rect)) } else { None };
                self.camera.zoom(scroll / 50.0, towards);
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
        let painting = std::time::Instant::now();
        if in_paint && pressed && !hand && response.hovered() && self.stroke.is_none() && !has_uvs {
            self.no_uv_offer_dismissed = false;
            self.toast = Some(("This model has no UVs yet, so there's nowhere for paint to go. Auto-unwrap it first.".into(), self.now));
        } else if in_paint && pressed && !hand && response.hovered() && self.stroke.is_none() && !self.colour_open {
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
        self.stats.cpu("Painting", painting.elapsed());

        let compositing = std::time::Instant::now();
        for set in &mut self.sets {
            self.compositor.update(&self.rs.device, &self.rs.queue, set);
        }
        self.stats.cpu("Compositing layers", compositing.elapsed());
        let params = FrameParams {
            view,
            proj,
            eye: self.camera.eye(),
            light_yaw: self.light_yaw,
            view_mode: if self.workspace == Workspace::Uv { ViewMode::UvCheck } else { self.view_mode },
            exposure: self.exposure,
        };
        let drawing = std::time::Instant::now();
        let tex = self.renderer.render(&self.rs, size, &params);
        self.stats.cpu("Drawing the model", drawing.elapsed());
        self.stats.draws = self.renderer.draw_counts();
        ui.painter().image(tex, rect, Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);

        // Brush cursor: outer ring is the full size, inner ring shows the hardness falloff.
        let painting = self.workspace == Workspace::Paint && self.mesh.is_some() && self.tool.shows_brush_cursor();
        if hand && response.hovered() {
            ui.ctx().set_cursor_icon(if nav_drag { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        } else if painting && !nav_drag {
            if let Some(pos) = response.hover_pos() {
                self.brush_cursor(ui.painter(), pos);
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
            }
        } else if self.workspace == Workspace::Paint && self.mesh.is_some() && !nav_drag && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }

        // While turning, a small brass dot marks what the model turns about.
        if self.nav.dragging && !shift && self.mesh.is_some() {
            let clip = proj * view * self.nav.pivot.extend(1.0);
            if clip.w > 0.0 {
                let ndc = clip.truncate() / clip.w;
                let at = egui::pos2(rect.left() + (ndc.x + 1.0) * 0.5 * rect.width(), rect.top() + (1.0 - ndc.y) * 0.5 * rect.height());
                let p = ui.painter();
                p.circle_filled(at, 5.0, Color32::from_black_alpha(140));
                p.circle_filled(at, 3.5, ACCENT);
            }
        }

        if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
            ui.painter().rect_stroke(rect.shrink(8.0), CornerRadius::same(16), Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
        }
        rect
    }

    /// Change how the camera moves, and remember it. Going back to turntable turning stands
    /// a tipped model upright again.
    fn set_cam_prefs(&mut self, prefs: CameraPrefs) {
        if prefs.turning == Turning::Turntable && self.cam_prefs.turning == Turning::Free {
            self.camera.make_level();
        }
        if !prefs.momentum {
            self.nav.spin = Vec2::ZERO;
        }
        self.cam_prefs = prefs;
        self.tips.set_camera(prefs);
    }

    /// A turn or pan begins at `at`: choose what it turns about and how deep the grabbed point is.
    fn start_nav(&mut self, at: Option<Pos2>, rect: Rect) {
        let centre = self.mesh.as_ref().map_or(self.camera.target, |m| m.bounding_sphere().0);
        let grabbed = at.and_then(|p| {
            let (origin, dir) = self.ray(p, rect);
            self.mesh.as_ref()?.raycast(origin, dir).map(|h| origin + dir * h.t)
        });
        self.nav.pivot = match self.cam_prefs.pivot {
            Pivot::Cursor => grabbed.unwrap_or(centre),
            Pivot::Centre => centre,
        };
        self.nav.depth = grabbed.map(|g| self.camera.depth_of(g)).filter(|d| *d > 0.0).unwrap_or(self.camera.distance);
        self.nav.dragging = true;
        self.nav.spin = Vec2::ZERO;
        self.nav.recent.clear();
    }

    /// What the scroll wheel zooms towards at `at`: the model under the cursor, or failing that
    /// a point as deep as the middle of the view. Zooming keeps that point under the cursor, so
    /// the answer holds until the cursor or the camera moves.
    fn zoom_anchor(&mut self, at: Pos2, rect: Rect) -> Vec3 {
        if let Some((p, w)) = self.nav.zoom_at {
            if p.distance(at) < 0.5 {
                return w;
            }
        }
        let (origin, dir) = self.ray(at, rect);
        let hit = self.mesh.as_ref().and_then(|m| m.raycast(origin, dir)).map(|h| origin + dir * h.t);
        let eye = self.camera.eye();
        let w = hit.unwrap_or_else(|| eye + dir * (self.camera.distance / dir.dot(self.camera.forward()).max(1e-3)));
        self.nav.zoom_at = Some((at, w));
        w
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

    /// The ray from the camera through a point of the viewport: where it starts on the near
    /// plane, and its direction.
    fn ray(&self, pos: Pos2, rect: Rect) -> (Vec3, Vec3) {
        let x = (pos.x - rect.left()) / rect.width() * 2.0 - 1.0;
        let y = 1.0 - (pos.y - rect.top()) / rect.height() * 2.0;
        let inv = (self.camera.projection(rect.width() / rect.height().max(1.0)) * self.camera.view()).inverse();
        let near = inv.project_point3(glam::Vec3::new(x, y, 0.0));
        let far = inv.project_point3(glam::Vec3::new(x, y, 1.0));
        (near, (far - near).normalize())
    }

    /// The mesh under a point of the viewport.
    fn pick(&self, pos: Pos2, rect: Rect) -> Option<mesh::Hit> {
        let (origin, dir) = self.ray(pos, rect);
        self.mesh.as_ref()?.raycast(origin, dir)
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
            texture: self.texture_paint(),
        }
    }

    /// How the brush texture is laid on, when the brush paints with one.
    fn texture_paint(&self) -> Option<brush::TexturePaint> {
        let b = &self.brush;
        let loaded = self.brush_texture.as_ref().is_some_and(|(s, _, _)| Some(s) == b.texture.as_ref());
        if !b.use_texture || !loaded {
            return None;
        }
        let size = self.mesh.as_ref().map_or(1.0, |m| {
            let (lo, hi) = m.bounds();
            (hi - lo).length()
        });
        Some(brush::TexturePaint {
            projection: b.projection,
            tile: b.projection.tile(size, self.ctx.pixels_per_point()) * b.texture_scale,
            rotation: b.texture_turn.to_radians(),
        })
    }

    /// The pixels of a brush texture source.
    fn texture_pixels(&self, source: &Source) -> anyhow::Result<brush_texture::Pixels> {
        match source {
            Source::Pattern(p) => Ok(brush_texture::pattern(*p)),
            Source::Model(i) => {
                let img = self.mesh.as_ref().and_then(|m| m.materials.get(*i)).and_then(|m| m.base_color_texture.clone());
                img.map(brush_texture::Pixels::from_image).ok_or_else(|| anyhow::anyhow!("the model has no texture there"))
            }
            Source::File(path) => brush_texture::load_file(path),
        }
    }

    /// Paint with `source` from now on.
    fn choose_texture(&mut self, source: Source) {
        match self.texture_pixels(&source) {
            Ok(px) => {
                let gpu = brush_texture::GpuTexture::upload(&self.rs.device, &self.rs.queue, &px);
                self.brush_engine.set_texture(&gpu);
                let thumb = self.ctx.load_texture("brush texture", brush_texture::thumbnail(&px.colour, 64), egui::TextureOptions::LINEAR);
                if let Source::File(path) = &source {
                    self.tips.remember_texture(path, true);
                }
                self.brush_texture = Some((source.clone(), thumb, self.mesh_generation));
                self.brush.texture = Some(source);
                self.brush.use_texture = true;
            }
            Err(e) => {
                let name = source.name(self.mesh.as_ref().map_or(&[], |m| &m.materials));
                if let Source::File(path) = &source {
                    self.tips.remember_texture(path, path.exists());
                }
                self.toast = Some((format!("Couldn't paint with {name}: {e}"), self.now));
                if self.brush.texture.as_ref() == Some(&source) {
                    self.brush.texture = None;
                    self.brush.use_texture = false;
                }
            }
        }
    }

    /// Load the brush texture the settings ask for, if it isn't already (after opening a
    /// project, say).
    fn sync_brush_texture(&mut self) {
        if !self.brush.use_texture {
            return;
        }
        match self.brush.texture.clone() {
            None => self.choose_texture(Source::Pattern(brush_texture::Pattern::Bricks)),
            // The model's own textures change with the model.
            Some(src)
                if self.brush_texture.as_ref().is_none_or(|(s, _, generation)| {
                    *s != src || (matches!(s, Source::Model(_)) && *generation != self.mesh_generation)
                }) =>
            {
                self.choose_texture(src)
            }
            Some(_) => {}
        }
    }

    fn open_brush_texture(&mut self) {
        let start = self.tips.recent_textures().first().and_then(|p| p.parent()).map(Path::to_owned);
        let mut dialog = self.dialog().set_title("Paint with an image").add_filter("Images", &["png", "jpg", "jpeg"]);
        if let Some(dir) = start {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.choose_texture(Source::File(path));
            self.workspace = Workspace::Paint;
            self.colour_open = true;
        }
    }

    /// Every texture the picker offers: patterns, the model's own, then recent images.
    fn texture_choices(&self) -> Vec<Source> {
        let mut out: Vec<Source> = brush_texture::Pattern::ALL.into_iter().map(Source::Pattern).collect();
        if let Some(mesh) = &self.mesh {
            out.extend(mesh.materials.iter().enumerate().filter(|(_, m)| m.base_color_texture.is_some()).map(|(i, _)| Source::Model(i)));
        }
        out.extend(self.tips.recent_textures().iter().cloned().map(Source::File));
        out
    }

    /// The picker's thumbnail for `source`, made the first time it's asked for.
    fn texture_thumb(&mut self, source: &Source) -> Option<egui::TextureHandle> {
        if self.thumbs_generation != self.mesh_generation {
            self.texture_thumbs.retain(|(s, _)| !matches!(s, Source::Model(_)));
            self.thumbs_generation = self.mesh_generation;
        }
        if let Some((_, t)) = self.texture_thumbs.iter().find(|(s, _)| s == source) {
            return Some(t.clone());
        }
        let px = self.texture_pixels(source).ok()?;
        let t = self.ctx.load_texture("texture thumb", brush_texture::thumbnail(&px.colour, 96), egui::TextureOptions::LINEAR);
        self.texture_thumbs.push((source.clone(), t.clone()));
        Some(t)
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
            Tool::Eraser if is_paint => Some((false, brush::Paint { mode: brush::Mode::Erase, texture: None, ..base })),
            Tool::Eraser => {
                ensure_mask(set)?;
                self.toast = Some(("Erasing a fill layer paints its mask, so the Mask tool can bring it back.".into(), now));
                Some((true, brush::Paint { colour: [0.0; 3], texture: None, ..base }))
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
        let one_face = self.brush.stroke_reach == Reach::Face;
        let only = if one_face { self.pick(pos, rect).and_then(|hit| self.face_triangles(hit.triangle, set)) } else { None };
        let finding_face = one_face && only.is_none();
        self.stroke = Some(StrokeState { set, mask, paint, last: pos, next: 0.0, pressure, only, finding_face });
        true
    }

    /// An index buffer of the triangles in `set` on the same face as `triangle`, and its
    /// index count.
    fn face_triangles(&mut self, triangle: usize, set: usize) -> Option<(wgpu::Buffer, u32)> {
        let mesh = self.mesh.as_ref()?;
        let draw = self.renderer.mesh_draw(set)?;
        Self::face_index_buffer(mesh, &mut self.faces, self.brush.edge_angle, triangle, &draw.ranges, &self.rs.device)
    }

    /// [`Self::face_triangles`] for callers already holding the mesh and its draw ranges.
    fn face_index_buffer(
        mesh: &Mesh,
        cache: &mut Option<(f32, Vec<u32>)>,
        angle: f32,
        triangle: usize,
        ranges: &[std::ops::Range<u32>],
        device: &wgpu::Device,
    ) -> Option<(wgpu::Buffer, u32)> {
        if cache.as_ref().is_none_or(|(a, _)| *a != angle) {
            *cache = Some((angle, mesh.faces(angle)));
        }
        let faces = &cache.as_ref()?.1;
        let face = *faces.get(triangle)?;
        let mut indices = Vec::new();
        for r in ranges {
            for tri in (r.start / 3)..(r.end / 3) {
                if faces[tri as usize] == face {
                    indices.extend_from_slice(&mesh.indices[tri as usize * 3..tri as usize * 3 + 3]);
                }
            }
        }
        if indices.is_empty() {
            return None;
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("one face"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        Some((buffer, indices.len() as u32))
    }

    /// Pen pressure through the curve, or full pressure for a mouse.
    fn pressure(&self) -> f32 {
        let pen = self.pen.status();
        if pen.active { self.brush.curve.apply(pen.sample.pressure) } else { 1.0 }
    }

    /// Lay dabs along the pointer's path since last frame.
    fn stroke_frame(&mut self, points: &[Pos2], rect: Rect, ppp: f32, cam: &brush::View) {
        // Keeping to one face: the first point that lands on the model chooses it, and until
        // then nothing is painted.
        if let Some((true, set)) = self.stroke.as_ref().map(|st| (st.finding_face, st.set)) {
            let found = points.iter().find_map(|p| self.pick(*p, rect)).and_then(|hit| self.face_triangles(hit.triangle, set));
            if let (Some(only), Some(st)) = (found, self.stroke.as_mut()) {
                st.only = Some(only);
                st.finding_face = false;
            }
        }
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
        if dabs.is_empty() || st.finding_face {
            return;
        }
        let (set, paint) = (st.set, st.paint);
        let only = st.only.as_ref().map(|(b, n)| (b, *n));
        if let Some(mut draw) = self.renderer.mesh_draw(set) {
            draw.only = only;
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
        let face_buffer;
        let draw = if whole {
            draw
        } else if self.brush.fill_reach == Reach::Face {
            let mut draw = draw;
            face_buffer = Self::face_index_buffer(mesh, &mut self.faces, self.brush.edge_angle, hit.triangle, &draw.ranges, device);
            draw.only = face_buffer.as_ref().map(|(b, n)| (b, *n));
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
            brush::MeshDraw { vertices: draw.vertices, indices: &island_buffer, ranges: vec![0..indices.len() as u32], only: None }
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
        // Picking up a colour means painting with it.
        self.brush.use_texture = false;
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
                        let tip = format!("{}\n{}", commands::tip(Cmd::Tool(tool)), commands::get(Cmd::Tool(tool)).about);
                        // On press, not release: a pen tap that slides a little still lands.
                        if widgets::tapped(&widgets::icon_button(ui, tool.icon(), self.tool == tool, 44.0, &tip)) {
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
                    if let (true, Some((_, thumb, _))) = (self.brush.use_texture, &self.brush_texture) {
                        let r = Rect::from_center_size(front, Vec2::splat(26.0));
                        egui::Image::new((thumb.id(), r.size())).corner_radius(13).paint_at(ui, r);
                    }
                    p.circle_stroke(front, 14.5, Stroke::new(1.0, Color32::from_rgb(0x54, 0x4a, 0x3f)));
                    if resp.on_hover_text("Colour").clicked() {
                        self.colour_open = !self.colour_open;
                    }
                    let tip = format!("{}\n{}", commands::tip(Cmd::SwapColours), commands::get(Cmd::SwapColours).about);
                    if widgets::tapped(&widgets::icon_button(ui, icons::SWAP, false, 28.0, &tip)) {
                        self.run(Cmd::SwapColours);
                    }
                });
            });
        });
        if self.colour_open {
            // Beside the rail, just under the brush bar (which can be two rows tall).
            let pos = egui::pos2(resp.response.rect.right() + 8.0, vp.top() + 120.0);
            let popover = egui::Area::new(egui::Id::unique("colour_popover")).order(Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    let w = 220.0;
                    ui.set_width(w);
                    // The picker sizes itself from the slider width.
                    ui.spacing_mut().slider_width = w;
                    ui.horizontal(|ui| {
                        widgets::caps_label(ui, "Brush paints");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, icons::X, false, 28.0, "Close").clicked() {
                                self.colour_open = false;
                            }
                        });
                    });
                    let mut textured = self.brush.use_texture;
                    if widgets::segmented_fill(ui, &mut textured, &[(false, "Colour"), (true, "Texture")], 30.0) {
                        self.brush.use_texture = textured;
                        self.sync_brush_texture();
                    }
                    ui.add_space(8.0);
                    if self.brush.use_texture {
                        self.texture_picker(ui, w);
                    } else {
                        widgets::colour_picker(ui, &mut self.primary, w);
                    }
                    ui.add_space(6.0);
                    let height_tip = if self.brush.use_texture {
                        "How deep the texture's bumps go; 0.50 is flat. Turn on Height in the brush bar to paint it"
                    } else {
                        "Turn on Height in the brush bar to paint it"
                    };

                    let b = &mut self.brush;
                    for (on, label, value, tip) in [
                        (b.paint_rough, "Roughness", &mut b.rough, "Turn on Rough in the brush bar to paint it"),
                        (b.paint_metal, "Metalness", &mut b.metal, "Turn on Metal in the brush bar to paint it"),
                        (b.paint_height, "Height", &mut b.height, height_tip),
                    ] {
                        ui.add_enabled_ui(on, |ui| {
                            let r = labelled_slider(ui, label, value, 0.0..=1.0, w, |v| format!("{v:.2}"));
                            r.on_disabled_hover_text(tip);
                        });
                    }
                });
            });
            // A press anywhere else closes it (the rail is left alone, so the colour circles
            // still toggle it). The viewport doesn't paint with that press.
            let press = ctx.input(|i| i.pointer.primary_pressed().then(|| i.pointer.interact_pos()).flatten());
            if press.is_some_and(|p| !popover.response.rect.contains(p) && !resp.response.rect.contains(p)) {
                self.colour_open = false;
            }
        }
    }

    /// The Texture side of the colour popover: what to paint with, and how it lies on.
    fn texture_picker(&mut self, ui: &mut egui::Ui, w: f32) {
        let materials: Vec<mesh::Material> = self.mesh.as_ref().map(|m| m.materials.clone()).unwrap_or_default();
        let cols = 4;
        let gap = 6.0;
        let side = ((w - gap * (cols as f32 - 1.0)) / cols as f32).floor();
        let mut choices: Vec<Option<Source>> = self.texture_choices().into_iter().map(Some).collect();
        // The last tile opens an image file.
        choices.push(None);
        let mut chosen = None;
        let mut open = false;
        for row in choices.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for choice in row {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
                    let p = ui.painter_at(rect.expand(2.0));
                    match choice {
                        Some(src) => {
                            match self.texture_thumb(src) {
                                Some(t) => {
                                    egui::Image::new((t.id(), rect.size())).corner_radius(8).paint_at(ui, rect);
                                }
                                None => {
                                    p.rect_filled(rect, CornerRadius::same(8), RAISED);
                                }
                            }
                            let on = self.brush.texture.as_ref() == Some(src);
                            if on {
                                p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
                            } else if resp.hovered() {
                                p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, TEXT_3), egui::StrokeKind::Inside);
                            }
                            if widgets::tapped(&resp.on_hover_text(src.name(&materials))) {
                                chosen = Some(src.clone());
                            }
                        }
                        None => {
                            let fill = if resp.hovered() { RAISED } else { SEG_BG };
                            p.rect_filled(rect, CornerRadius::same(8), fill);
                            p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, PANEL_LINE), egui::StrokeKind::Inside);
                            p.text(rect.center(), Align2::CENTER_CENTER, icons::IMAGE_PLUS, theme::icon_font(20.0), ICON);
                            let tip = format!("{}\n{}", commands::get(Cmd::OpenBrushTexture).name, commands::get(Cmd::OpenBrushTexture).about);
                            if resp.on_hover_text(tip).clicked() {
                                open = true;
                            }
                        }
                    }
                }
            });
            ui.add_space(gap - ui.spacing().item_spacing.y);
        }
        if let Some(src) = chosen {
            self.choose_texture(src);
        }
        if open {
            self.open_brush_texture();
        }
        if let Some(src) = &self.brush.texture {
            ui.label(egui::RichText::new(src.name(&materials)).color(TEXT_2));
        }
        ui.add_space(8.0);

        widgets::caps_label(ui, "Lay it on");
        let options: Vec<(Projection, &str)> = Projection::ALL.iter().map(|p| (*p, p.name())).collect();
        widgets::segmented_fill(ui, &mut self.brush.projection, &options, 28.0);
        ui.add(egui::Label::new(egui::RichText::new(self.brush.projection.about()).color(TEXT_3).size(12.0)).wrap());
        ui.add_space(6.0);
        // Scale moves in doublings, so small and large are equally easy to reach.
        let mut doublings = self.brush.texture_scale.max(1e-3).log2();
        labelled_slider(ui, "Scale", &mut doublings, -3.0..=3.0, w, |v| format!("×{:.2}", v.exp2()));
        self.brush.texture_scale = doublings.exp2();
        labelled_slider(ui, "Turn", &mut self.brush.texture_turn, -180.0..=180.0, w, |v| format!("{v:.0}°"));
    }

    fn brush_bar(&mut self, ctx: &egui::Context, vp: Rect) {
        // Fits between the tool rail and the layers panel.
        let max_w = (vp.width() - 92.0 - 352.0 - 16.0 - 28.0).max(240.0);
        egui::Area::new(egui::Id::unique("brush_bar")).order(Order::Foreground).fixed_pos(vp.min + egui::vec2(92.0, 16.0)).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                // One row when it fits, otherwise the pen and channel toggles drop to a second row.
                let one_row = max_w >= 1640.0;
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
                            self.reach_controls(ui, true);
                            widgets::vdivider(ui, 24.0);
                            hint(ui, "Shift+click fills the whole material");
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
        widgets::vdivider(ui, 24.0);
        self.reach_controls(ui, false);
    }

    /// Whether strokes (or fills) reach everything under them or keep to one face.
    fn reach_controls(&mut self, ui: &mut egui::Ui, fill: bool) {
        widgets::caps_label(ui, if fill { "Fills" } else { "Paints on" });
        let b = &mut self.brush;
        let (reach, options) = if fill {
            (&mut b.fill_reach, [(Reach::All, "UV island"), (Reach::Face, "One face")])
        } else {
            (&mut b.stroke_reach, [(Reach::All, "Everything"), (Reach::Face, "One face")])
        };
        widgets::segmented(ui, reach, &options, 30.0);
        if *reach == Reach::Face {
            ui.scope(|ui| slider_with_value(ui, "Edge", &mut b.edge_angle, 5.0..=90.0, 64.0, |v| format!("{v:.0}°")))
                .response
                .on_hover_text("Where one face ends: a bend sharper than this. Lower keeps to flatter parts; higher follows rounder ones");
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
        let mut action = None;
        ui.horizontal(|ui| {
            widgets::caps_label(ui, "Selected layer");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (icon, cmd) in [
                    (icons::TRASH, Cmd::DeleteLayer),
                    (icons::COPY, Cmd::DuplicateLayer),
                    (icons::ARROW_DOWN, Cmd::LayerDown),
                    (icons::ARROW_UP, Cmd::LayerUp),
                ] {
                    let ok = self.unavailable(cmd).is_none();
                    if widgets::icon_action(ui, icon, ok, 30.0, &self.tip(cmd)).clicked() {
                        action = Some(cmd);
                    }
                }
            });
        });
        if let Some(cmd) = action {
            self.run(cmd);
            return;
        }
        let (device, queue) = (&self.rs.device, &self.rs.queue);
        let Some(set) = self.sets.get_mut(self.active_set) else { return };
        let i = set.selected;
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
                    if widgets::chip(ui, "Paint", &mut (self.tool == Tool::Mask)).on_hover_text(format!("Paint the mask ({})", commands::key_label(Cmd::Tool(Tool::Mask)).unwrap_or_default())).clicked() {
                        mask_tool = true;
                    }
                } else if widgets::button(ui, "Add mask", false, 28.0)
                    .on_hover_text(format!("Hide parts of this layer by painting with the Mask tool ({})", commands::key_label(Cmd::Tool(Tool::Mask)).unwrap_or_default()))
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
                        widgets::colour_button(ui, &mut f.colour);
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
        let mut camera_button = None;
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
                        widgets::vdivider(ui, 18.0);
                        let tip = "Camera: how dragging turns the model and how scrolling zooms";
                        let b = widgets::icon_button(ui, icons::ROTATE_3D, self.camera_open, 36.0, tip);
                        if b.clicked() {
                            self.camera_open = !self.camera_open;
                        }
                        camera_button = Some(b.rect);
                    });
                });
            });
        if let (true, Some(button)) = (self.camera_open, camera_button) {
            self.camera_popover(ctx, button);
        }
    }

    /// How the camera moves, opened from the view bar.
    fn camera_popover(&mut self, ctx: &egui::Context, button: Rect) {
        let mut prefs = self.cam_prefs;
        let popover = egui::Area::new(egui::Id::unique("camera_popover"))
            .order(Order::Foreground)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_pos(button.center_top() - egui::vec2(0.0, 18.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    let w = 260.0;
                    ui.set_width(w);
                    ui.horizontal(|ui| {
                        widgets::caps_label(ui, "Camera");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, icons::X, false, 28.0, "Close").clicked() {
                                self.camera_open = false;
                            }
                        });
                    });
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Turns about").color(TEXT_2).size(12.0));
                    widgets::segmented_fill(ui, &mut prefs.pivot, &[(Pivot::Cursor, "What you grab"), (Pivot::Centre, "Model centre")], 30.0);
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Turning").color(TEXT_2).size(12.0));
                    widgets::segmented_fill(ui, &mut prefs.turning, &[(Turning::Turntable, "Up stays up"), (Turning::Free, "Free")], 30.0);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        widgets::chip(ui, "Zoom to cursor", &mut prefs.zoom_to_cursor)
                            .on_hover_text("Scrolling zooms towards what's under the cursor");
                        widgets::chip(ui, "Spin on a flick", &mut prefs.momentum)
                            .on_hover_text("Let go mid-drag and the model turns on for a moment, then settles");
                    });
                    ui.add_space(6.0);
                    let how = "Right-drag, or Space+drag, turns. Add Shift to move. Scroll zooms.";
                    ui.label(egui::RichText::new(how).color(TEXT_3).size(11.5));
                });
            });
        if prefs != self.cam_prefs {
            self.set_cam_prefs(prefs);
        }
        let press = ctx.input(|i| if i.pointer.any_pressed() { i.pointer.press_origin() } else { None });
        if press.is_some_and(|p| !popover.response.rect.contains(p) && !button.contains(p)) {
            self.camera_open = false;
        }
    }

    fn welcome(&mut self, ctx: &egui::Context, vp: Rect) {
        egui::Area::new(egui::Id::unique("welcome")).order(Order::Foreground).pivot(Align2::CENTER_CENTER).fixed_pos(vp.center()).show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin::same(28)).show(ui, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(icons::BOX).font(theme::icon_font(34.0)).color(ACCENT));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Open a model to start painting").font(theme::semibold(18.0)));
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Drop a model or an .umber project anywhere in this window.").color(TEXT_3));
                    ui.add_space(16.0);
                    if widgets::button(ui, "Open model…", true, 40.0).clicked() {
                        self.open_dialog();
                    }
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(commands::key_label(Cmd::Open).unwrap_or_default()).font(theme::mono(11.0)).color(TEXT_3));
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
                    self.recovery_list(ui);
                });
            });
        });
    }

    /// Unsaved work an earlier Umber left behind (it closed without saving, or crashed).
    fn recovery_list(&mut self, ui: &mut egui::Ui) {
        if self.recoverable.is_empty() {
            return;
        }
        ui.add_space(10.0);
        panel_divider(ui);
        ui.label(egui::RichText::new("Unsaved work from last time").color(TEXT_3).size(12.0));
        ui.add_space(4.0);
        let (mut recover, mut discard) = (None, None);
        for (i, r) in self.recoverable.iter().take(3).enumerate() {
            let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), Sense::hover());
            ui.scope_builder(UiBuilder::new().max_rect(row).layout(Layout::left_to_right(Align::Center)), |ui| {
                ui.label(egui::RichText::new(icons::HISTORY).font(theme::icon_font(16.0)).color(ACCENT));
                ui.add_space(2.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(egui::Label::new(egui::RichText::new(&r.name).color(TEXT)).truncate());
                    let age = r.modified.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
                    ui.label(egui::RichText::new(when(age)).color(TEXT_3).size(11.5));
                });
            });
            ui.scope_builder(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)), |ui| {
                if widgets::icon_action(ui, icons::X, true, 30.0, "Throw this away").clicked() {
                    discard = Some(i);
                }
                if widgets::button(ui, "Recover", false, 30.0).on_hover_text(home_relative(&r.path)).clicked() {
                    recover = Some(r.path.clone());
                }
            });
        }
        if let Some(i) = discard {
            let gone = self.recoverable.remove(i);
            let _ = std::fs::remove_file(&gone.path);
        }
        if let Some(path) = recover {
            self.open_project(&path, true);
        }
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

    /// The one tip that applies now and hasn't been seen, below the brush bar.
    fn tips(&mut self, ctx: &egui::Context, vp: Rect) {
        let Some(mesh) = &self.mesh else { return };
        // The unwrap offer and modal surfaces come first.
        if self.unwrap_job.is_some() || (!mesh.has_uvs && !self.no_uv_offer_dismissed) || self.palette.open || self.shortcuts_open || self.pending.is_some() {
            return;
        }
        let on_fill = self.sets.get(self.active_set).and_then(|s| s.selected_layer()).is_some_and(|l| l.pixels.is_none());
        let tip = match self.workspace {
            Workspace::Paint if self.tips.wants(Tip::Start) => Tip::Start,
            Workspace::Paint if self.tool == Tool::Eraser && on_fill => Tip::EraseFill,
            Workspace::Paint if self.tool == Tool::Mask => Tip::Mask,
            Workspace::Uv => Tip::Uv,
            Workspace::Export => Tip::Export,
            _ => return,
        };
        if !self.tips.wants(tip) {
            return;
        }
        let x = match self.workspace {
            // Centred in the space the panels leave: the UV and export panels are narrower.
            Workspace::Uv | Workspace::Export => (vp.left() + 92.0 + vp.right() - 336.0) * 0.5,
            _ => (vp.left() + 92.0 + vp.right() - 352.0) * 0.5,
        };
        let top = if self.workspace == Workspace::Paint { vp.top() + 120.0 } else { vp.top() + 16.0 };
        help::tip_card(ctx, egui::pos2(x, top), tip, &mut self.tips);
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

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
        if self.parent.is_none() {
            if let (Ok(w), Ok(d)) = (frame.window_handle(), frame.display_handle()) {
                self.parent = Some(Parent { window: w.as_raw(), display: d.as_raw() });
            }
        }
        self.stats.begin_frame(frame.info().cpu_usage);
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        self.now = now;
        self.handle_close(&ctx);
        if self.pending.is_none() {
            self.handle_shortcuts(&ctx);
        }
        self.poll_unwrap(&ctx);
        self.poll_export(&ctx);
        self.poll_saves(&ctx);

        egui::Panel::top("top_bar")
            .exact_size(52.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(TOPBAR).inner_margin(Margin::symmetric(16, 0)))
            .show(ui, |ui| self.top_bar(ui));

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
            self.welcome(&ctx, vp);
        } else {
            self.workspace_placard(&ctx, vp);
            self.unwrap_card(&ctx, vp);
        }
        self.stats.show(&ctx, vp);
        self.tips(&ctx, vp);
        self.toast(&ctx, vp, now);
        let chosen = {
            let (mut palette, mut sheet) = (std::mem::take(&mut self.palette), self.shortcuts_open);
            let state = |cmd| help::CmdState { reason: self.unavailable(cmd), on: self.cmd_on(cmd) };
            let from_palette = palette.show(&ctx, &state);
            let from_sheet = help::shortcut_sheet(&ctx, &mut sheet);
            (self.palette, self.shortcuts_open) = (palette, sheet);
            from_palette.or(from_sheet)
        };
        if let Some(cmd) = chosen {
            if self.unavailable(cmd).is_none() {
                self.run(cmd);
            }
        }
        self.save_changes_modal(&ctx);

        // Anything the panels changed becomes one undo step once the pointer lets go.
        if !ctx.input(|i| i.pointer.any_down()) {
            self.history.record_stack_changes(&mut self.sets);
        }
        self.autosave(&ctx);
        self.update_title(&ctx);
        if self.stats.open {
            self.stats.memory = self.memory();
        }
        self.stats.end_frame(&self.rs.device, &self.rs.queue);
    }
}

// ---------------------------------------------------------------- helpers

/// Umber's window, handed to file pickers so the desktop keeps them above it.
#[derive(Clone, Copy)]
struct Parent {
    window: raw_window_handle::RawWindowHandle,
    display: raw_window_handle::RawDisplayHandle,
}

impl raw_window_handle::HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        // Safety: the window lives as long as the app, which owns this.
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.window) })
    }
}

impl raw_window_handle::HasDisplayHandle for Parent {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        // Safety: as above.
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.display) })
    }
}

impl UmberApp {
    /// A file dialog that opens over Umber's window.
    fn dialog(&self) -> rfd::FileDialog {
        let dialog = rfd::FileDialog::new();
        match &self.parent {
            Some(p) => dialog.set_parent(p),
            None => dialog,
        }
    }
}

/// Take a save job off its slot once it has finished.
fn finished(slot: &mut Option<SaveJob>, ctx: &egui::Context) -> Option<(SaveJob, anyhow::Result<()>)> {
    let job = slot.as_ref()?;
    let outcome = match job.result.try_recv() {
        Ok(outcome) => outcome,
        Err(mpsc::TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return None;
        }
        Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("the save stopped unexpectedly")),
    };
    slot.take().map(|job| (job, outcome))
}

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

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn when(seconds: f64) -> String {
    match seconds {
        s if s < 60.0 => "just now".to_owned(),
        s if s < 3600.0 => format!("{} min ago", (s / 60.0) as u32),
        s if s < 86400.0 => format!("{} h ago", (s / 3600.0) as u32),
        s if s < 172800.0 => "yesterday".to_owned(),
        s => format!("{} days ago", (s / 86400.0) as u32),
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
/// The narrow arrow beside Save that opens Save and Save as.
fn save_menu_arrow(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 36.0), if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(8), RAISED);
    }
    let fg = match (enabled, hovered) {
        (false, _) => ICON_OFF,
        (true, true) => TEXT,
        (true, false) => ICON,
    };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::CHEVRON_DOWN, theme::icon_font(13.0), fg);
    resp.on_hover_text(format!("Save as… ({})", commands::key_label(Cmd::SaveAs).unwrap_or_default()))
}

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
