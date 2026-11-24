//! Orbit camera around a target point, Z up as in FreeCAD.

use glam::{Mat4, Vec2, Vec3, Vec4};

pub struct Camera {
    pub target: Vec3,
    pub distance: f32,
    /// Around Z, radians.
    pub yaw: f32,
    /// Above the XY plane, radians.
    pub pitch: f32,
    pub ortho: bool,
    /// Radius of the scene, for fitting and zoom limits.
    pub radius: f32,
    /// Width / height of the view, kept from the last frame for `fit`.
    pub aspect: f32,
}

const FOV_Y: f32 = 35.0 * std::f32::consts::PI / 180.0;
/// Just short of straight up or down, where the view's up direction is lost.
const MAX_PITCH: f32 = std::f32::consts::FRAC_PI_2 - 1e-4;

impl Default for Camera {
    fn default() -> Self {
        let mut c = Camera { target: Vec3::ZERO, distance: 100.0, yaw: 0.0, pitch: 0.0, ortho: true, radius: 50.0, aspect: 1.5 };
        c.set_view(View::Iso);
        c
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Iso,
    Front,
    Top,
    Right,
}

impl Camera {
    pub fn eye(&self) -> Vec3 {
        self.target + self.forward() * -self.distance
    }

    /// Direction the camera looks in.
    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        -Vec3::new(cp * cy, cp * sy, sp)
    }

    fn basis(&self) -> (Vec3, Vec3) {
        let f = self.forward();
        let right = f.cross(Vec3::Z).try_normalize().unwrap_or(Vec3::X);
        let up = right.cross(f);
        (right, up)
    }

    /// `drawn` bounds everything the view draws (the scene, the ground grid). The clip planes
    /// sit just outside it, so nothing in it is cut however the camera is panned or zoomed.
    pub fn view_proj(&self, aspect: f32, drawn: (Vec3, Vec3)) -> Mat4 {
        let (_, up) = self.basis();
        let eye = self.eye();
        let view = look_at_rh(eye, self.target, up);
        // Depths of the box's corners along the view direction.
        let (lo, hi) = drawn;
        let forward = self.forward();
        let (mut nearest, mut farthest) = (f32::MAX, f32::MIN);
        for i in 0..8 {
            let corner = Vec3::new(
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            );
            let d = (corner - eye).dot(forward);
            nearest = nearest.min(d);
            farthest = farthest.max(d);
        }
        let pad = (hi - lo).length() * 0.01 + 1e-3;
        let proj = if self.ortho {
            let h = self.distance * (FOV_Y * 0.5).tan();
            let w = h * aspect;
            // The eye may sit inside the scene after zooming; ortho can look behind it safely.
            let near = nearest - pad;
            orthographic_rh(-w, w, -h, h, near, (farthest + pad).max(near + pad))
        } else {
            let near = (nearest - pad).max(self.distance * 0.01).max(1e-3);
            perspective_rh(FOV_Y, aspect, near, (farthest + pad).max(near * 2.0))
        };
        proj * view
    }

    pub fn fit(&mut self, lo: Vec3, hi: Vec3) {
        self.target = (lo + hi) * 0.5;
        self.radius = ((hi - lo).length() * 0.5).max(1e-3);
        // The narrower of the two fields of view decides, so tall views do not crop the model.
        let half_x = ((FOV_Y * 0.5).tan() * self.aspect.max(0.05)).atan();
        let half = (FOV_Y * 0.5).min(half_x);
        self.distance = self.radius / half.sin() * 1.05;
    }

    pub fn set_view(&mut self, v: View) {
        use std::f32::consts::FRAC_PI_2;
        let (yaw, pitch) = match v {
            View::Iso => (-45f32.to_radians(), 35.264f32.to_radians()), // from the front-right
            View::Front => (-FRAC_PI_2, 0.0),
            View::Top => (-FRAC_PI_2, FRAC_PI_2 - 1e-4),
            View::Right => (0.0, 0.0),
        };
        self.yaw = yaw;
        self.pitch = pitch;
    }

    /// Mouse drag in pixels.
    pub fn orbit(&mut self, delta: Vec2) {
        self.yaw -= delta.x * 0.008;
        self.pitch = (self.pitch + delta.y * 0.008).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Turn the camera about its own eye, as looking around: the eye stays put and the point
    /// orbited about moves with the view. Mouse drag in pixels; right turns right, up looks up.
    pub fn look(&mut self, delta: Vec2) {
        let eye = self.eye();
        self.yaw -= delta.x * 0.004;
        self.pitch = (self.pitch + delta.y * 0.004).clamp(-MAX_PITCH, MAX_PITCH);
        self.target = eye + self.forward() * self.distance;
    }

    /// Mouse drag in pixels, viewport height in pixels.
    pub fn pan(&mut self, delta: Vec2, viewport_h: f32) {
        let (right, up) = self.basis();
        let world_per_px = 2.0 * self.distance * (FOV_Y * 0.5).tan() / viewport_h.max(1.0);
        self.target += (-right * delta.x + up * delta.y) * world_per_px;
    }

    /// Pixels per document unit at the target, in a view `height_px` high.
    pub fn px_per_unit(&self, height_px: f32) -> f32 {
        height_px / (2.0 * self.distance * (FOV_Y * 0.5).tan())
    }

    /// Scroll in points; positive zooms in.
    pub fn zoom(&mut self, steps: f32) {
        self.distance = (self.distance * (-steps * 0.0015).exp()).clamp(self.radius * 1e-3, self.radius * 1e3);
    }
}

/// The six planes of a view-projection's clip volume (wgpu's: -w..w in x and y, 0..w in z),
/// facing inwards. Works for orthographic and perspective projections alike.
pub struct Frustum {
    planes: [Vec4; 6],
}

impl Frustum {
    pub fn new(view_proj: Mat4) -> Self {
        let r = |i| view_proj.row(i);
        let (x, y, z, w) = (r(0), r(1), r(2), r(3));
        Frustum { planes: [w + x, w - x, w + y, w - y, z, w - z] }
    }

    /// False only when the box is wholly outside one plane, so it cannot show.
    pub fn intersects(&self, lo: Vec3, hi: Vec3) -> bool {
        self.planes.iter().all(|p| {
            // the corner furthest along the plane's normal
            let c = Vec3::new(if p.x >= 0.0 { hi.x } else { lo.x }, if p.y >= 0.0 { hi.y } else { lo.y }, if p.z >= 0.0 { hi.z } else { lo.z });
            p.truncate().dot(c) + p.w >= 0.0
        })
    }
}

// Right-handed view and projection matrices with wgpu's 0..1 depth range.

fn look_at_rh(eye: Vec3, center: Vec3, up: Vec3) -> Mat4 {
    let f = (center - eye).normalize();
    let s = f.cross(up).normalize();
    let u = s.cross(f);
    Mat4::from_cols(
        Vec4::new(s.x, u.x, -f.x, 0.0),
        Vec4::new(s.y, u.y, -f.y, 0.0),
        Vec4::new(s.z, u.z, -f.z, 0.0),
        Vec4::new(-s.dot(eye), -u.dot(eye), f.dot(eye), 1.0),
    )
}

fn perspective_rh(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y * 0.5).tan();
    let r = far / (near - far);
    Mat4::from_cols(
        Vec4::new(f / aspect, 0.0, 0.0, 0.0),
        Vec4::new(0.0, f, 0.0, 0.0),
        Vec4::new(0.0, 0.0, r, -1.0),
        Vec4::new(0.0, 0.0, r * near, 0.0),
    )
}

fn orthographic_rh(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Mat4 {
    let (rw, rh, r) = (1.0 / (right - left), 1.0 / (top - bottom), 1.0 / (near - far));
    Mat4::from_cols(
        Vec4::new(2.0 * rw, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 2.0 * rh, 0.0, 0.0),
        Vec4::new(0.0, 0.0, r, 0.0),
        Vec4::new(-(left + right) * rw, -(top + bottom) * rh, r * near, 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_puts_the_box_in_front_of_the_camera() {
        for ortho in [true, false] {
        for aspect in [0.4f32, 1.5, 3.0] {
            let mut c = Camera { ortho, aspect, ..Camera::default() };
            c.fit(Vec3::new(0.0, 0.0, 0.0), Vec3::new(80.0, 50.0, 6.0));
            let vp = c.view_proj(aspect, (Vec3::ZERO, Vec3::new(80.0, 50.0, 6.0)));
            for p in [Vec3::new(0.0, 0.0, 0.0), Vec3::new(80.0, 50.0, 6.0), Vec3::new(40.0, 25.0, 3.0)] {
                let q = vp.project_point3(p);
                assert!(q.x.abs() <= 1.0 && q.y.abs() <= 1.0 && (0.0..=1.0).contains(&q.z), "{ortho} {aspect} {p} -> {q}");
            }
        }
        }
    }

    #[test]
    fn frustum_keeps_what_is_in_view_and_drops_the_rest() {
        for ortho in [true, false] {
            let mut c = Camera { ortho, aspect: 1.5, ..Camera::default() };
            c.fit(Vec3::ZERO, Vec3::splat(10.0));
            let f = Frustum::new(c.view_proj(1.5, (Vec3::ZERO, Vec3::splat(10.0))));
            let unit = |p: Vec3| (p, p + Vec3::ONE);
            // the fitted box, a box poking out of one side, one far to the side, one behind the eye
            assert!(f.intersects(Vec3::ZERO, Vec3::splat(10.0)), "{ortho}");
            let (lo, hi) = unit(Vec3::new(-0.5, 5.0, 5.0));
            assert!(f.intersects(lo, hi), "{ortho}");
            let side = c.target + c.basis().0 * c.distance * 10.0;
            let (lo, hi) = unit(side);
            assert!(!f.intersects(lo, hi), "{ortho}");
            let behind = c.eye() - c.forward() * (c.distance + c.radius * 4.0);
            let (lo, hi) = unit(behind);
            assert!(!f.intersects(lo, hi), "{ortho}");
            // zoomed in, the far corner of the box leaves the view
            c.distance *= 0.05;
            let f = Frustum::new(c.view_proj(1.5, (Vec3::ZERO, Vec3::splat(10.0))));
            let (lo, hi) = unit(Vec3::ZERO);
            assert!(!f.intersects(lo, hi), "{ortho}");
            let (lo, hi) = unit(c.target - Vec3::splat(0.5));
            assert!(f.intersects(lo, hi), "{ortho}");
        }
    }

    #[test]
    fn clip_planes_keep_everything_drawn_however_the_camera_moves() {
        // A small part far from the origin, and the ground grid reaching back to the origin.
        let (lo, hi) = (Vec3::new(100.0, 100.0, 0.0), Vec3::new(110.0, 110.0, 5.0));
        let drawn = (Vec3::new(-130.0, -130.0, 0.0), hi.max(Vec3::new(130.0, 130.0, 0.0)));
        for ortho in [true, false] {
            let mut c = Camera { ortho, aspect: 1.5, ..Camera::default() };
            c.fit(lo, hi);
            c.yaw = 45f32.to_radians(); // looking back toward the origin
            c.pitch = 10f32.to_radians();
            c.zoom(800.0);
            let vp = c.view_proj(1.5, drawn);
            for p in [Vec3::new(-130.0, -130.0, 0.0), Vec3::new(-130.0, 130.0, 0.0), Vec3::new(130.0, -130.0, 0.0)] {
                let q = vp * p.extend(1.0);
                if q.w > 0.0 {
                    assert!(q.z / q.w <= 1.0, "{ortho} {p} beyond the far plane: {}", q.z / q.w);
                }
            }
        }
    }

    #[test]
    fn looking_around_keeps_the_eye_in_place() {
        let mut c = Camera::default();
        c.fit(Vec3::ZERO, Vec3::splat(10.0));
        let eye = c.eye();
        c.look(Vec2::new(120.0, -40.0));
        assert!((c.eye() - eye).length() < 1e-3 * c.distance, "{} vs {eye}", c.eye());
        let before = c.forward();
        // dragging right turns the view right: the new forward is clockwise from above
        c.look(Vec2::new(50.0, 0.0));
        assert!(before.cross(c.forward()).z < 0.0);
        // dragging up looks up
        let z = c.forward().z;
        c.look(Vec2::new(0.0, -50.0));
        assert!(c.forward().z > z);
    }

    #[test]
    fn top_view_looks_down() {
        let mut c = Camera::default();
        c.set_view(View::Top);
        assert!(c.forward().z < -0.999);
    }
}
