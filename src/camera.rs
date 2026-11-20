//! Orbit camera: turntable rotation around a target, pan in the view plane, dolly zoom.

use glam::{Mat4, Vec3};

pub struct OrbitCamera {
    pub target: Vec3,
    pub distance: f32,
    /// Radians around the vertical axis.
    pub yaw: f32,
    /// Radians above the horizon, clamped short of the poles.
    pub pitch: f32,
    pub fov_y: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self { target: Vec3::ZERO, distance: 4.0, yaw: 0.65, pitch: 0.38, fov_y: 35f32.to_radians() }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        let dir = Vec3::new(self.pitch.cos() * self.yaw.sin(), self.pitch.sin(), self.pitch.cos() * self.yaw.cos());
        self.target + dir * self.distance
    }

    pub fn view(&self) -> Mat4 {
        Mat4::look_at_rh(self.eye(), self.target, Vec3::Y)
    }

    pub fn projection(&self, aspect: f32) -> Mat4 {
        let near = (self.distance * 0.01).max(1e-4);
        let far = self.distance * 100.0 + 10.0;
        Mat4::perspective_rh(self.fov_y, aspect.max(1e-3), near, far)
    }

    /// Frame a bounding sphere so it fills most of the view.
    pub fn frame(&mut self, center: Vec3, radius: f32) {
        self.target = center;
        self.distance = radius / (self.fov_y * 0.5).sin() * 1.15;
    }

    /// `delta` in points; a full viewport-width drag turns about 270°.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.008;
        self.pitch = (self.pitch + dy * 0.008).clamp(-1.54, 1.54);
    }

    /// Pan so the point under the cursor follows it. `viewport_h` in points.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_h: f32) {
        let world_per_point = 2.0 * self.distance * (self.fov_y * 0.5).tan() / viewport_h.max(1.0);
        let view = self.view();
        let right = Vec3::new(view.x_axis.x, view.y_axis.x, view.z_axis.x);
        let up = Vec3::new(view.x_axis.y, view.y_axis.y, view.z_axis.y);
        self.target += (-right * dx + up * dy) * world_per_point;
    }

    /// Positive `steps` zoom in.
    pub fn zoom(&mut self, steps: f32) {
        self.distance = (self.distance * 0.88f32.powf(steps)).clamp(1e-3, 1e5);
    }
}
