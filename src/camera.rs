//! The viewport camera. By default turning the model feels like holding it: it turns about
//! the point you grab, up stays up, the scroll wheel zooms towards the cursor, and a flick
//! keeps it turning for a moment. Turning about the model's centre and free (trackball)
//! turning are options, in [`CameraPrefs`].
//!
//! Turning about any point keeps that point still on screen, so whatever was under the cursor
//! when the drag began stays under it.

use glam::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

/// Radians a point of drag turns the model: a full viewport-width drag turns about 270°.
const TURN_PER_POINT: f32 = 0.008;
/// How far turntable turning can look up or down: just short of straight.
const MAX_PITCH: f32 = 1.54;

/// What the model turns about.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Pivot {
    /// The point on the model under the cursor when the drag starts (the model's centre when
    /// the cursor is off it).
    #[default]
    Cursor,
    /// The middle of the model's bounds, wherever the view has moved.
    Centre,
}

/// How a drag turns the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Turning {
    /// Sideways turns about the vertical, up and down tilts; up always stays up.
    #[default]
    Turntable,
    /// The model rolls the way the drag goes, like a ball under the hand; it can end up at
    /// any angle.
    Free,
}

/// How the camera moves, remembered between sessions.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraPrefs {
    pub pivot: Pivot,
    pub turning: Turning,
    /// The scroll wheel zooms towards the point under the cursor, not the middle of the view.
    pub zoom_to_cursor: bool,
    /// A flick keeps the model turning for a moment after the drag lets go.
    pub momentum: bool,
}

impl Default for CameraPrefs {
    fn default() -> Self {
        Self { pivot: Pivot::Cursor, turning: Turning::Turntable, zoom_to_cursor: true, momentum: true }
    }
}

pub struct OrbitCamera {
    /// The point the camera looks at, `distance` in front of it.
    pub target: Vec3,
    pub distance: f32,
    /// Camera to world: the camera looks down its -Z with +Y up.
    pub rot: Quat,
    pub fov_y: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self { target: Vec3::ZERO, distance: 4.0, rot: Self::level(0.65, 0.38), fov_y: 35f32.to_radians() }
    }
}

impl OrbitCamera {
    /// An upright camera `yaw` radians round the vertical and `pitch` above the horizon.
    pub fn level(yaw: f32, pitch: f32) -> Quat {
        Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-pitch)
    }

    /// The yaw and pitch of the way the camera faces, ignoring any roll.
    pub fn yaw_pitch(&self) -> (f32, f32) {
        let back = self.rot * Vec3::Z;
        let pitch = back.y.clamp(-1.0, 1.0).asin();
        let yaw = if back.y.abs() < 0.99 {
            back.x.atan2(back.z)
        } else {
            // Looking straight up or down: the camera's right says which way it faces.
            let right = self.rot * Vec3::X;
            (-right.z).atan2(right.x)
        };
        (yaw, pitch)
    }

    pub fn eye(&self) -> Vec3 {
        self.target + self.rot * Vec3::Z * self.distance
    }

    pub fn forward(&self) -> Vec3 {
        self.rot * Vec3::NEG_Z
    }

    pub fn view(&self) -> Mat4 {
        Mat4::from_rotation_translation(self.rot, self.eye()).inverse()
    }

    pub fn projection(&self, aspect: f32) -> Mat4 {
        let near = (self.distance * 0.01).max(1e-4);
        let far = self.distance * 100.0 + 10.0;
        Mat4::perspective_rh(self.fov_y, aspect.max(1e-3), near, far)
    }

    /// Frame a bounding sphere so it fills most of the view, facing the way it faces now.
    pub fn frame(&mut self, center: Vec3, radius: f32) {
        self.target = center;
        self.distance = radius / (self.fov_y * 0.5).sin() * 1.15;
    }

    /// Stand the camera upright again, about the middle of the view (after free turning).
    pub fn make_level(&mut self) {
        let (yaw, pitch) = self.yaw_pitch();
        self.rot = Self::level(yaw, pitch.clamp(-MAX_PITCH, MAX_PITCH));
    }

    /// Turn the model by a drag of `dx`, `dy` points about `pivot`, which stays where it is on
    /// screen. The model follows the drag.
    pub fn turn(&mut self, dx: f32, dy: f32, pivot: Vec3, turning: Turning) {
        let old = self.rot;
        let new = match turning {
            Turning::Turntable => {
                let (yaw, pitch) = self.yaw_pitch();
                Self::level(yaw - dx * TURN_PER_POINT, (pitch + dy * TURN_PER_POINT).clamp(-MAX_PITCH, MAX_PITCH))
            }
            Turning::Free => {
                let axis = old * Vec3::Y * dx + old * Vec3::X * dy;
                let len = axis.length();
                if len < 1e-6 {
                    return;
                }
                (Quat::from_axis_angle(axis / len, -len * TURN_PER_POINT) * old).normalize()
            }
        };
        // The camera swings round the pivot by however much it turned.
        let q = new * old.inverse();
        self.target = pivot + q * (self.target - pivot);
        self.rot = new;
    }

    /// Pan so a point `depth` in front of the camera follows the cursor exactly.
    /// `viewport_h` in points.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_h: f32, depth: f32) {
        let world_per_point = 2.0 * depth.max(1e-4) * (self.fov_y * 0.5).tan() / viewport_h.max(1.0);
        let (right, up) = (self.rot * Vec3::X, self.rot * Vec3::Y);
        self.target += (-right * dx + up * dy) * world_per_point;
    }

    /// How far in front of the camera a point is, along the view direction.
    pub fn depth_of(&self, point: Vec3) -> f32 {
        (point - self.eye()).dot(self.forward())
    }

    /// Positive `steps` zoom in, towards `towards` when given (which then stays put on screen),
    /// else towards the middle of the view.
    pub fn zoom(&mut self, steps: f32, towards: Option<Vec3>) {
        let factor = 0.88f32.powf(steps);
        let distance = (self.distance * factor).clamp(1e-3, 1e5);
        let factor = distance / self.distance;
        if let Some(p) = towards {
            // Slide the eye along its line to `p`, so `p` stays under the cursor.
            let eye = p + (self.eye() - p) * factor;
            self.target = eye + self.forward() * distance;
        }
        self.distance = distance;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(cam: &OrbitCamera, p: Vec3) -> Vec3 {
        (cam.projection(1.5) * cam.view()).project_point3(p)
    }

    #[test]
    fn level_matches_the_old_yaw_pitch_camera() {
        let cam = OrbitCamera::default();
        let (yaw, pitch) = (0.65f32, 0.38f32);
        let dir = Vec3::new(pitch.cos() * yaw.sin(), pitch.sin(), pitch.cos() * yaw.cos());
        assert!((cam.eye() - (cam.target + dir * cam.distance)).length() < 1e-5);
        let old = Mat4::look_at_rh(cam.eye(), cam.target, Vec3::Y);
        assert!(cam.view().abs_diff_eq(old, 1e-5));
        let (y, p) = cam.yaw_pitch();
        assert!((y - yaw).abs() < 1e-5 && (p - pitch).abs() < 1e-5);
    }

    #[test]
    fn the_pivot_stays_put_on_screen() {
        for turning in [Turning::Turntable, Turning::Free] {
            let mut cam = OrbitCamera::default();
            let pivot = Vec3::new(0.6, -0.3, 0.4);
            let before = screen(&cam, pivot);
            cam.turn(37.0, -21.0, pivot, turning);
            cam.turn(-5.0, 60.0, pivot, turning);
            let after = screen(&cam, pivot);
            assert!((before - after).truncate().length() < 1e-4, "{turning:?}: {before} -> {after}");
        }
    }

    #[test]
    fn turntable_keeps_up_up() {
        let mut cam = OrbitCamera::default();
        cam.turn(120.0, 300.0, Vec3::ZERO, Turning::Turntable);
        let right = cam.rot * Vec3::X;
        assert!(right.y.abs() < 1e-5);
        assert!(cam.yaw_pitch().1 <= MAX_PITCH + 1e-5);
    }

    #[test]
    fn zoom_towards_a_point_keeps_it_under_the_cursor() {
        let mut cam = OrbitCamera::default();
        let p = Vec3::new(0.7, 0.2, -0.1);
        let before = screen(&cam, p);
        cam.zoom(3.0, Some(p));
        let after = screen(&cam, p);
        assert!((before - after).truncate().length() < 1e-4);
        assert!(cam.distance < 4.0);
    }

    #[test]
    fn pan_moves_a_point_at_its_depth_with_the_cursor() {
        let mut cam = OrbitCamera::default();
        let p = Vec3::new(0.3, 0.1, 0.2);
        let h = 800.0;
        let before = screen(&cam, p);
        cam.pan(40.0, -25.0, h, cam.depth_of(p));
        let after = screen(&cam, p);
        // NDC spans 2 over the viewport height.
        let moved = (after - before).truncate() * h / 2.0;
        assert!((moved.y - 25.0).abs() < 1e-2 && (moved.x * 1.5 - 40.0).abs() < 1e-2, "{moved}");
    }
}

#[cfg(test)]
mod soak {
    use super::*;

    #[test]
    fn free_turning_stays_finite() {
        let mut cam = OrbitCamera::default();
        cam.frame(Vec3::new(0.0, 0.9, 0.0), 1.0);
        let pivot = Vec3::new(-0.4, 1.1, 0.1);
        for i in 0..2000 {
            let (dx, dy) = ((i as f32 * 0.37).sin() * 3.0, (i as f32 * 0.21).cos() * 2.0);
            cam.turn(dx, dy, pivot, Turning::Free);
            assert!(cam.rot.is_finite() && cam.target.is_finite(), "step {i}: {:?} {:?}", cam.rot, cam.target);
        }
        for i in 0..200 {
            cam.turn(1e-3 * i as f32, 0.0, pivot, Turning::Free);
            cam.turn(0.0, 1e-7, pivot, Turning::Free);
            assert!(cam.rot.is_finite() && cam.target.is_finite(), "tiny {i}");
        }
    }
}
