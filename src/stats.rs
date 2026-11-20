//! The F3 overlay: how long frames take, where the time goes on the CPU and the GPU, what the
//! viewport draws, how much texture memory the work holds, and which GPU does it.
//!
//! GPU time comes from timestamp queries, when the adapter has them. Any pass worth timing asks
//! [`timestamps`] for a slot; the timer is global so it needn't be threaded through every call,
//! and it only hands out slots while the overlay is open.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui_wgpu::wgpu;
use egui::{Align2, Color32, CornerRadius, Margin, Order, Rect, Sense, Stroke};

use crate::theme::{self, ACCENT, PANEL_LINE, TEXT, TEXT_2, TEXT_3};

/// Frames kept for the graph and the averages.
const HISTORY: usize = 240;
/// Frames averaged for the numbers, so they're steady enough to read.
const AVERAGE: usize = 30;
/// Timed passes per frame; any past this go untimed.
const MAX_PASSES: u32 = 64;
/// Frames of timestamps in flight back from the GPU at once.
const READBACKS: usize = 3;

const FREE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;

static GPU: Mutex<Option<GpuTimer>> = Mutex::new(None);

/// The device features Umber asks for on `adapter`: timestamp queries when it has them.
pub fn wanted_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    adapter.features() & wgpu::Features::TIMESTAMP_QUERY
}

/// Set up GPU timing, if the device can do it.
pub fn install_gpu_timer(device: &wgpu::Device, queue: &wgpu::Queue) {
    if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
        return;
    }
    let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("pass timestamps"),
        ty: wgpu::QueryType::Timestamp,
        count: MAX_PASSES * 2,
    });
    let size = (MAX_PASSES * 2) as u64 * 8;
    let resolve = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timestamp resolve"),
        size,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = (0..READBACKS)
        .map(|_| Readback {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("timestamp readback"),
                size,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            labels: Vec::new(),
            state: Arc::new(AtomicU8::new(FREE)),
        })
        .collect();
    *GPU.lock().unwrap_or_else(|e| e.into_inner()) = Some(GpuTimer {
        queries,
        resolve,
        readback,
        labels: Vec::new(),
        on: false,
        period_ns: queue.get_timestamp_period() as f64,
        latest: Vec::new(),
    });
}

/// Whether this device can time GPU passes at all.
pub fn gpu_timing_available() -> bool {
    GPU.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn set_gpu_timing(on: bool) {
    if let Some(t) = GPU.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        t.on = on;
        if !on {
            t.labels.clear();
            t.latest.clear();
        }
    }
}

/// A pair of timestamp slots for one pass.
pub struct Slot {
    set: wgpu::QuerySet,
    begin: u32,
}

impl Slot {
    pub fn render(&self) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(self.begin),
            end_of_pass_write_index: Some(self.begin + 1),
        }
    }

    pub fn compute(&self) -> wgpu::ComputePassTimestampWrites<'_> {
        wgpu::ComputePassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(self.begin),
            end_of_pass_write_index: Some(self.begin + 1),
        }
    }
}

/// Timestamp slots for a pass called `label`, while the overlay is open and the GPU can time
/// passes. Passes with the same label add up. Call on the UI thread only.
pub fn timestamps(label: &'static str) -> Option<Slot> {
    let mut gpu = GPU.lock().unwrap_or_else(|e| e.into_inner());
    let t = gpu.as_mut().filter(|t| t.on)?;
    let n = t.labels.len() as u32;
    if n >= MAX_PASSES {
        return None;
    }
    t.labels.push(label);
    Some(Slot { set: t.queries.clone(), begin: n * 2 })
}

struct Readback {
    buffer: wgpu::Buffer,
    labels: Vec<&'static str>,
    state: Arc<AtomicU8>,
}

struct GpuTimer {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: Vec<Readback>,
    /// The label of each pass timed this frame, in slot order.
    labels: Vec<&'static str>,
    on: bool,
    period_ns: f64,
    /// Milliseconds per label, from the newest frame the GPU has finished.
    latest: Vec<(&'static str, f64)>,
}

impl GpuTimer {
    /// Send this frame's timestamps back, and read any that have arrived.
    fn end_frame(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let n = self.labels.len() as u32;
        if n > 0 {
            if let Some(rb) = self.readback.iter_mut().find(|r| r.state.load(Ordering::Acquire) == FREE) {
                let bytes = (n * 2) as u64 * 8;
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("timestamps") });
                enc.resolve_query_set(&self.queries, 0..n * 2, &self.resolve, 0);
                enc.copy_buffer_to_buffer(&self.resolve, 0, &rb.buffer, 0, bytes);
                queue.submit([enc.finish()]);
                rb.labels = std::mem::take(&mut self.labels);
                rb.state.store(MAPPING, Ordering::Release);
                let state = rb.state.clone();
                rb.buffer.slice(..bytes).map_async(wgpu::MapMode::Read, move |r| {
                    state.store(if r.is_ok() { MAPPED } else { FREE }, Ordering::Release);
                });
            }
            // With every readback still in flight this frame goes untimed.
            self.labels.clear();
        }
        let _ = device.poll(wgpu::PollType::Poll);
        for rb in &mut self.readback {
            if rb.state.load(Ordering::Acquire) != MAPPED {
                continue;
            }
            let bytes = rb.labels.len() as u64 * 16;
            let mut times: Vec<(&'static str, f64)> = Vec::new();
            if let Ok(view) = rb.buffer.slice(..bytes).get_mapped_range() {
                let stamps: &[u64] = bytemuck::cast_slice(&view);
                for (label, pair) in rb.labels.iter().zip(stamps.chunks_exact(2)) {
                    let ms = pair[1].saturating_sub(pair[0]) as f64 * self.period_ns / 1e6;
                    match times.iter_mut().find(|(l, _)| l == label) {
                        Some((_, t)) => *t += ms,
                        None => times.push((label, ms)),
                    }
                }
            }
            rb.buffer.unmap();
            rb.state.store(FREE, Ordering::Release);
            if self.on {
                self.latest = times;
            }
        }
    }
}

/// What the viewport drew last frame.
#[derive(Clone, Copy, Default)]
pub struct DrawCounts {
    pub triangles: u64,
    pub draw_calls: u32,
}

/// Texture memory the open work holds, in bytes.
#[derive(Clone, Copy, Default)]
pub struct Memory {
    pub layers: u64,
    pub maps: u64,
    pub undo: u64,
}

/// GPU memory a texture takes, with every mip level (block-compressed and depth formats
/// aside, which Umber doesn't keep around).
pub fn texture_bytes(t: &wgpu::Texture) -> u64 {
    let bpp = t.format().block_copy_size(None).unwrap_or(4) as u64;
    let (w, h, layers) = (t.width() as u64, t.height() as u64, t.depth_or_array_layers() as u64);
    (0..t.mip_level_count()).map(|l| (w >> l).max(1) * (h >> l).max(1)).sum::<u64>() * bpp * layers * t.sample_count() as u64
}

/// Everything the overlay shows, collected as frames go by.
pub struct Stats {
    pub open: bool,
    last_frame: Option<Instant>,
    frame_start: Instant,
    /// Milliseconds between frames.
    frames: VecDeque<f32>,
    /// Milliseconds of CPU on each named part of a frame, newest frame last.
    cpu: Vec<(&'static str, VecDeque<f32>)>,
    /// Running totals for the frame being built.
    cpu_now: Vec<(&'static str, Duration)>,
    /// eframe's own figure for the previous frame: Umber's code plus drawing the UI.
    whole_cpu: VecDeque<f32>,
    gpu: Vec<(&'static str, VecDeque<f32>)>,
    pub draws: DrawCounts,
    pub memory: Memory,
    adapter: String,
}

impl Stats {
    pub fn new(open: bool, adapter: &wgpu::AdapterInfo) -> Self {
        let backend = match adapter.backend {
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Dx12 => "DirectX 12",
            wgpu::Backend::Gl => "OpenGL",
            wgpu::Backend::BrowserWebGpu => "WebGPU",
            wgpu::Backend::Noop => "none",
        };
        let driver = [adapter.driver.as_str(), adapter.driver_info.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let adapter = if driver.is_empty() {
            format!("{} · {backend}", adapter.name)
        } else {
            format!("{} · {backend} · {driver}", adapter.name)
        };
        set_gpu_timing(open);
        Self {
            open,
            last_frame: None,
            frame_start: Instant::now(),
            frames: VecDeque::new(),
            cpu: Vec::new(),
            cpu_now: Vec::new(),
            whole_cpu: VecDeque::new(),
            gpu: Vec::new(),
            draws: DrawCounts::default(),
            memory: Memory::default(),
            adapter,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        set_gpu_timing(self.open);
        // Old numbers would be from before the gap.
        self.last_frame = None;
        self.frames.clear();
        self.cpu.clear();
        self.whole_cpu.clear();
        self.gpu.clear();
    }

    /// Call first thing in a frame. `whole_cpu` is eframe's CPU time for the previous frame.
    pub fn begin_frame(&mut self, whole_cpu: Option<f32>) {
        let now = Instant::now();
        self.frame_start = now;
        self.cpu_now.clear();
        if !self.open {
            return;
        }
        if let Some(last) = self.last_frame.replace(now) {
            push(&mut self.frames, (now - last).as_secs_f32() * 1000.0);
        }
        if let Some(s) = whole_cpu {
            push(&mut self.whole_cpu, s * 1000.0);
        }
    }

    /// Add `took` to the CPU time of `part` this frame.
    pub fn cpu(&mut self, part: &'static str, took: Duration) {
        match self.cpu_now.iter_mut().find(|(p, _)| *p == part) {
            Some((_, d)) => *d += took,
            None => self.cpu_now.push((part, took)),
        }
    }

    /// Call last thing in a frame, once every pass has been submitted.
    pub fn end_frame(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if !self.open {
            return;
        }
        let ui = self.frame_start.elapsed();
        let named: Duration = self.cpu_now.iter().map(|(_, d)| *d).sum();
        self.cpu("Panels and input", ui.saturating_sub(named));
        let parts = std::mem::take(&mut self.cpu_now);
        record(&mut self.cpu, parts.iter().map(|(p, d)| (*p, d.as_secs_f32() * 1000.0)));

        let mut gpu = GPU.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = gpu.as_mut() {
            t.end_frame(device, queue);
            let latest: Vec<_> = t.latest.iter().map(|(l, ms)| (*l, *ms as f32)).collect();
            record(&mut self.gpu, latest.into_iter());
        }
    }

    /// The overlay, at the bottom left of the viewport beside the tool rail and above the view
    /// bar, clear of the brush bar and the panels.
    pub fn show(&self, ctx: &egui::Context, vp: Rect) {
        if !self.open {
            return;
        }
        // Numbers that only change while something happens would read as frozen.
        ctx.request_repaint();
        egui::Area::new(egui::Id::unique("stats overlay"))
            .order(Order::Foreground)
            .interactable(false)
            .pivot(Align2::LEFT_BOTTOM)
            .fixed_pos(egui::pos2(vp.left() + 92.0, vp.bottom() - 72.0))
            .show(ctx, |ui| {
                theme::panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    ui.set_width(280.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    self.contents(ui);
                });
            });
    }

    fn contents(&self, ui: &mut egui::Ui) {
        let frame = average(&self.frames);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{:.0}", if frame > 0.0 { 1000.0 / frame } else { 0.0 })).font(theme::semibold(22.0)).color(TEXT));
            ui.label(egui::RichText::new("fps").font(theme::mono(12.0)).color(TEXT_3));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(format!("{frame:.1} ms a frame")).font(theme::mono(12.0)).color(TEXT_2));
            });
        });
        self.graph(ui);

        heading(ui, "CPU");
        let whole = average(&self.whole_cpu);
        if whole > 0.0 {
            row(ui, "Each frame, all told", &ms(whole));
        }
        for (part, samples) in &self.cpu {
            row(ui, part, &ms(average(samples)));
        }

        heading(ui, "GPU");
        if !gpu_timing_available() {
            hint(ui, "This GPU can't time its passes.");
        } else if self.gpu.is_empty() {
            hint(ui, "Waiting for the first timings…");
        } else {
            let total: f32 = self.gpu.iter().map(|(_, s)| average(s)).sum();
            row(ui, "Timed passes, all told", &ms(total));
            for (part, samples) in &self.gpu {
                row(ui, part, &ms(average(samples)));
            }
        }

        heading(ui, "Viewport");
        row(ui, "Triangles", &count(self.draws.triangles));
        row(ui, "Draw calls", &count(self.draws.draw_calls as u64));

        heading(ui, "Texture memory");
        row(ui, "Layers", &bytes(self.memory.layers));
        row(ui, "Material maps", &bytes(self.memory.maps));
        row(ui, "Undo history", &bytes(self.memory.undo));

        ui.add_space(6.0);
        ui.label(egui::RichText::new(&self.adapter).font(theme::mono(11.0)).color(TEXT_3));
    }

    /// Frame times, newest on the right, against lines at 60 and 30 fps.
    fn graph(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::same(4), theme::SEG_BG);
        let top_ms = self.frames.iter().copied().fold(50.0f32, f32::max).min(200.0);
        let y = |ms: f32| rect.bottom() - (ms / top_ms).min(1.0) * rect.height();
        for (ms, label) in [(1000.0 / 60.0, "60"), (1000.0 / 30.0, "30")] {
            let yy = y(ms);
            p.line_segment([egui::pos2(rect.left(), yy), egui::pos2(rect.right(), yy)], Stroke::new(1.0, PANEL_LINE));
            p.text(egui::pos2(rect.right() - 3.0, yy - 1.0), Align2::RIGHT_BOTTOM, label, theme::mono(9.0), TEXT_3);
        }
        let bar = rect.width() / HISTORY as f32;
        let start = HISTORY - self.frames.len();
        for (i, &ms) in self.frames.iter().enumerate() {
            let x = rect.left() + (start + i) as f32 * bar;
            // Frames that miss 30 fps stand out.
            let colour = if ms > 1000.0 / 30.0 + 1.0 { Color32::from_rgb(0xd0, 0x6a, 0x4f) } else { ACCENT };
            p.rect_filled(Rect::from_min_max(egui::pos2(x, y(ms)), egui::pos2(x + bar.max(1.0), rect.bottom())), CornerRadius::ZERO, colour);
        }
    }
}

fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}

/// Add one frame's figures; parts missing this frame count as zero, and parts that have
/// been zero for the whole history drop off.
fn record(into: &mut Vec<(&'static str, VecDeque<f32>)>, frame: impl Iterator<Item = (&'static str, f32)>) {
    let frame: Vec<_> = frame.collect();
    for (part, _) in &frame {
        if !into.iter().any(|(p, _)| p == part) {
            into.push((part, VecDeque::new()));
        }
    }
    for (part, q) in into.iter_mut() {
        push(q, frame.iter().find(|(p, _)| p == part).map_or(0.0, |(_, v)| *v));
    }
    into.retain(|(_, q)| q.iter().any(|v| *v > 0.0));
}

fn average(q: &VecDeque<f32>) -> f32 {
    let n = q.len().min(AVERAGE);
    if n == 0 {
        return 0.0;
    }
    q.iter().rev().take(n).sum::<f32>() / n as f32
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(5.0);
    ui.label(egui::RichText::new(text).font(theme::medium(12.0)).color(TEXT_2));
}

/// A name on the left and its value on the right, one compact line.
fn row(ui: &mut egui::Ui, name: &str, value: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 15.0), Sense::hover());
    let p = ui.painter();
    p.text(rect.left_center(), Align2::LEFT_CENTER, name, theme::mono(11.5), TEXT_3);
    p.text(rect.right_center(), Align2::RIGHT_CENTER, value, theme::mono(11.5), TEXT);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(theme::mono(11.5)).color(TEXT_3));
}

fn ms(v: f32) -> String {
    if v < 0.1 { format!("{:.0} µs", v * 1000.0) } else { format!("{v:.1} ms") }
}

fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn bytes(b: u64) -> String {
    let mb = b as f64 / (1u64 << 20) as f64;
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_commas() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1234567), "1,234,567");
    }

    #[test]
    fn parts_that_stop_drop_off() {
        let mut parts = Vec::new();
        record(&mut parts, [("Painting", 2.0)].into_iter());
        assert_eq!(parts.len(), 1);
        for _ in 0..HISTORY {
            record(&mut parts, std::iter::empty());
        }
        assert!(parts.is_empty());
    }

    #[test]
    fn average_uses_the_newest_frames() {
        let mut q = VecDeque::new();
        for _ in 0..100 {
            push(&mut q, 50.0);
        }
        for _ in 0..AVERAGE {
            push(&mut q, 10.0);
        }
        assert_eq!(average(&q), 10.0);
    }
}
