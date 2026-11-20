//! Pen pressure and tilt on X11 through XInput2 raw events.
//!
//! winit gives no pen pressure on X11, so a background thread opens its own X connection,
//! finds every pointer device with an "Abs Pressure" valuator, and listens for raw events
//! on the root window. Raw events reach every client that asks for them, whatever window
//! the pointer is over and whoever holds a grab, so they never fight winit's own input.
//! Position still comes from egui; this only adds pressure, tilt and contact.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _, DeviceClassData, DeviceType, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{Atom, ConnectionExt as _};

/// One reading from the pen.
#[derive(Clone, Copy, Debug, Default)]
pub struct PenSample {
    /// 0..1, straight from the device (no curve applied).
    pub pressure: f32,
    /// -1..1 on each axis; 1 means tilted fully to the right / towards the user.
    pub tilt: [f32; 2],
    /// The tip is on the surface.
    pub touching: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PenStatus {
    /// Name of the last pen that sent anything, or the first one found.
    pub device: Option<String>,
    /// Pressure levels the device reports.
    pub levels: u32,
    /// The pen, not a mouse, moved the pointer last.
    pub active: bool,
    pub sample: PenSample,
    /// Why there is no pen input, when there is none.
    pub problem: Option<String>,
}

/// Handle to the pen thread. Cheap to read every frame.
pub struct PenInput {
    shared: Arc<Mutex<PenStatus>>,
}

impl PenInput {
    /// Starts listening. Never fails: if X11 or XInput2 is missing, `status().problem` says so.
    pub fn start(ctx: egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(PenStatus::default()));
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new().name("pen-xi2".into()).spawn(move || {
            if let Err(e) = run(&thread_shared, &ctx) {
                log::warn!("pen input stopped: {e:#}");
                thread_shared.lock().unwrap().problem = Some(format!("{e:#}"));
            }
        });
        if let Err(e) = spawned {
            shared.lock().unwrap().problem = Some(format!("could not start the pen thread: {e}"));
        }
        Self { shared }
    }

    pub fn status(&self) -> PenStatus {
        self.shared.lock().unwrap().clone()
    }
}

/// How pressure maps to brush strength. Shared by every pressure-driven brush setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureCurve {
    /// -1 (soft: a light touch already paints strongly) .. 1 (firm: needs a hard press).
    pub softness: f32,
    /// Output at the lightest touch, 0..1. Stops strokes thinning to nothing.
    pub minimum: f32,
}

impl Default for PressureCurve {
    fn default() -> Self {
        Self { softness: 0.0, minimum: 0.0 }
    }
}

impl PressureCurve {
    pub fn apply(&self, pressure: f32) -> f32 {
        // Exponent 0.33..3: soft bends the curve up, firm bends it down.
        let exponent = 3f32.powf(-self.softness);
        let p = pressure.clamp(0.0, 1.0).powf(exponent);
        self.minimum + (1.0 - self.minimum) * p
    }
}

// ---------------------------------------------------------------- the X thread

#[derive(Clone, Copy, Default)]
struct Axis {
    number: u16,
    min: f64,
    max: f64,
}

impl Axis {
    /// Maps a raw value to 0..1.
    fn unit(&self, v: f64) -> f32 {
        let span = self.max - self.min;
        if span <= 0.0 { 0.0 } else { ((v - self.min) / span).clamp(0.0, 1.0) as f32 }
    }
}

#[derive(Clone)]
struct PenDevice {
    name: String,
    pressure: Axis,
    tilt: [Option<Axis>; 2],
    /// Last value of every valuator; raw events only carry the ones that changed.
    values: HashMap<u16, f64>,
}

#[derive(Default)]
struct Devices {
    pens: HashMap<u16, PenDevice>,
    /// Devices that move by relative motion: real mice and touchpads.
    mice: Vec<u16>,
}

struct Atoms {
    pressure: Atom,
    tilt_x: Atom,
    tilt_y: Atom,
    rel_x: Atom,
}

fn fp(v: Fp3232) -> f64 {
    v.integral as f64 + v.frac as f64 / 4_294_967_296.0
}

fn run(shared: &Mutex<PenStatus>, ctx: &egui::Context) -> anyhow::Result<()> {
    if std::env::var_os("DISPLAY").is_none() {
        anyhow::bail!("no X11 display; pen pressure needs X11 for now");
    }
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let ver = conn.xinput_xi_query_version(2, 2)?.reply()?;
    if (ver.major_version, ver.minor_version) < (2, 1) {
        anyhow::bail!("X server has XInput {}.{}; raw events need 2.1", ver.major_version, ver.minor_version);
    }

    let intern = |name: &str| -> anyhow::Result<Atom> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
    let atoms = Atoms {
        pressure: intern("Abs Pressure")?,
        tilt_x: intern("Abs Tilt X")?,
        tilt_y: intern("Abs Tilt Y")?,
        rel_x: intern("Rel X")?,
    };

    let mask = XIEventMask::RAW_MOTION | XIEventMask::RAW_BUTTON_PRESS | XIEventMask::RAW_BUTTON_RELEASE | XIEventMask::HIERARCHY;
    conn.xinput_xi_select_events(root, &[xinput::EventMask { deviceid: 0, mask: vec![mask] }])?.check()?;

    let mut devices = scan(&conn, &atoms)?;
    publish_devices(shared, &devices);

    loop {
        let event = conn.wait_for_event()?;
        let changed = match event {
            Event::XinputRawMotion(e) => on_raw(shared, &mut devices, e.sourceid, &e.valuator_mask, &e.axisvalues, None),
            Event::XinputRawButtonPress(e) if e.detail == 1 => {
                on_raw(shared, &mut devices, e.sourceid, &e.valuator_mask, &e.axisvalues, Some(true))
            }
            Event::XinputRawButtonRelease(e) if e.detail == 1 => {
                on_raw(shared, &mut devices, e.sourceid, &e.valuator_mask, &e.axisvalues, Some(false))
            }
            Event::XinputHierarchy(_) => {
                devices = scan(&conn, &atoms)?;
                publish_devices(shared, &devices);
                true
            }
            _ => false,
        };
        if changed {
            ctx.request_repaint();
        }
    }
}

fn scan(conn: &impl Connection, atoms: &Atoms) -> anyhow::Result<Devices> {
    let reply = conn.xinput_xi_query_device(0u16)?.reply()?;
    let mut out = Devices::default();
    for info in reply.infos {
        if info.type_ != DeviceType::SLAVE_POINTER || !info.enabled {
            continue;
        }
        let mut pressure = None;
        let mut tilt = [None, None];
        let mut relative = false;
        for class in &info.classes {
            if let DeviceClassData::Valuator(v) = &class.data {
                let axis = Axis { number: v.number, min: fp(v.min), max: fp(v.max) };
                match v.label {
                    l if l == atoms.pressure => pressure = Some(axis),
                    l if l == atoms.tilt_x => tilt[0] = Some(axis),
                    l if l == atoms.tilt_y => tilt[1] = Some(axis),
                    l if l == atoms.rel_x => relative = true,
                    _ => {}
                }
            }
        }
        let name = String::from_utf8_lossy(&info.name).into_owned();
        if let Some(pressure) = pressure {
            log::info!(
                "pen: {name} (id {}), {} pressure levels, tilt {}",
                info.deviceid,
                (pressure.max - pressure.min) as u32 + 1,
                if tilt[0].is_some() { "yes" } else { "no" }
            );
            out.pens.insert(info.deviceid, PenDevice { name, pressure, tilt, values: HashMap::new() });
        } else if relative {
            out.mice.push(info.deviceid);
        }
    }
    Ok(out)
}

fn publish_devices(shared: &Mutex<PenStatus>, devices: &Devices) {
    let mut s = shared.lock().unwrap();
    // Keep the current pen if it is still there; otherwise name any pen we have.
    let still_there = s.device.as_ref().is_some_and(|n| devices.pens.values().any(|p| &p.name == n));
    if !still_there {
        match devices.pens.values().next() {
            Some(p) => {
                s.device = Some(p.name.clone());
                s.levels = (p.pressure.max - p.pressure.min) as u32 + 1;
            }
            None => {
                s.device = None;
                s.levels = 0;
                s.active = false;
                s.sample = PenSample::default();
            }
        }
    }
    s.problem = if devices.pens.is_empty() { Some("no pen tablet found".into()) } else { None };
}

/// Returns true when anything the UI shows changed.
fn on_raw(
    shared: &Mutex<PenStatus>,
    devices: &mut Devices,
    source: u16,
    mask: &[u32],
    values: &[Fp3232],
    tip: Option<bool>,
) -> bool {
    let Some(pen) = devices.pens.get_mut(&source) else {
        // A mouse moved: pressure no longer applies to the pointer.
        if devices.mice.contains(&source) {
            let mut s = shared.lock().unwrap();
            if s.active {
                s.active = false;
                s.sample.touching = false;
                return true;
            }
        }
        return false;
    };

    // The mask is a bitset of valuator numbers; values come in the same order.
    let mut it = values.iter();
    for (word_i, word) in mask.iter().enumerate() {
        for bit in 0..32 {
            if word & (1 << bit) != 0
                && let Some(v) = it.next()
            {
                pen.values.insert((word_i * 32 + bit) as u16, fp(*v));
            }
        }
    }

    if tip == Some(false) {
        pen.values.insert(pen.pressure.number, pen.pressure.min);
    }

    let get = |axis: &Axis| pen.values.get(&axis.number).map(|v| axis.unit(*v));
    let pressure = get(&pen.pressure).unwrap_or(0.0);
    let tilt = pen.tilt.map(|a| a.and_then(|a| get(&a)).map_or(0.0, |u| u * 2.0 - 1.0));

    let mut s = shared.lock().unwrap();
    if s.device.as_deref() != Some(pen.name.as_str()) {
        s.device = Some(pen.name.clone());
        s.levels = (pen.pressure.max - pen.pressure.min) as u32 + 1;
    }
    s.active = true;
    s.sample.tilt = tilt;
    match tip {
        // Some drivers leave the last pressure in place after lift-off.
        Some(false) => {
            s.sample.touching = false;
            s.sample.pressure = 0.0;
        }
        Some(true) => {
            s.sample.touching = true;
            s.sample.pressure = pressure;
        }
        // Pressure can arrive a moment before the tip-down button, so it counts as contact too.
        None => {
            s.sample.pressure = pressure;
            s.sample.touching |= pressure > 0.0;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_ends_and_bend() {
        let linear = PressureCurve::default();
        assert!((linear.apply(0.5) - 0.5).abs() < 1e-6);
        let soft = PressureCurve { softness: 1.0, minimum: 0.0 };
        assert!(soft.apply(0.25) > 0.5);
        let firm = PressureCurve { softness: -1.0, minimum: 0.0 };
        assert!(firm.apply(0.5) < 0.2);
        let floored = PressureCurve { softness: 0.0, minimum: 0.2 };
        assert!((floored.apply(0.0) - 0.2).abs() < 1e-6);
        assert!((floored.apply(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fixed_point() {
        assert_eq!(fp(Fp3232 { integral: 3, frac: 1 << 31 }), 3.5);
        assert_eq!(fp(Fp3232 { integral: -64, frac: 0 }), -64.0);
    }
}
