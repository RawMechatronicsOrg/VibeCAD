//! What the view needs to pick edges and to draw drag handles: the B-rep edges and faces the
//! worker sends on demand (`edges`, `face`), and the screen geometry of picking an edge near
//! the cursor and of dragging a handle along a direction in the model.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2, pos2};
use glam::{Mat4, Vec3};
use serde_json::Value;

/// A B-rep edge of an object, in document coordinates.
#[derive(Clone, Debug)]
pub struct EdgeInfo {
    /// "Edge<index>".
    pub index: u32,
    /// The faces it bounds, 0-based ("Face1" is 0).
    pub faces: Vec<u32>,
    pub points: Vec<Vec3>,
    /// Its middle, and the direction out of the corner there (zero for a lone edge).
    pub mid: Vec3,
    pub out: Vec3,
}

/// A face as handles and previews use it, in document coordinates.
#[derive(Clone, Debug)]
pub struct FaceInfo {
    pub obj: String,
    pub sub: String,
    pub planar: bool,
    pub center: Vec3,
    /// Outward.
    pub normal: Vec3,
    pub points: Vec<Vec3>,
    pub triangles: Vec<[u32; 3]>,
    /// Triangle sides on the face's border: the outline of a preview.
    pub border: Vec<[u32; 2]>,
}

fn vec3(v: &Value) -> Option<Vec3> {
    Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32))
}

pub fn parse_edges(v: &Value) -> Vec<EdgeInfo> {
    v["edges"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(EdgeInfo {
                        index: e["i"].as_u64()? as u32,
                        faces: e["faces"].as_array()?.iter().filter_map(|f| f.as_u64().map(|f| f as u32)).collect(),
                        points: e["points"].as_array()?.iter().filter_map(vec3).collect(),
                        mid: vec3(&e["mid"])?,
                        out: vec3(&e["out"])?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_face(v: &Value) -> Option<FaceInfo> {
    let points: Vec<Vec3> = v["points"].as_array()?.iter().filter_map(vec3).collect();
    let n = points.len() as u64;
    let triangles: Vec<[u32; 3]> = v["triangles"]
        .as_array()?
        .iter()
        .filter_map(|t| {
            let c = [t[0].as_u64()?, t[1].as_u64()?, t[2].as_u64()?];
            c.iter().all(|&i| i < n).then(|| c.map(|i| i as u32))
        })
        .collect();
    Some(FaceInfo {
        obj: v["obj"].as_str()?.to_string(),
        sub: v["sub"].as_str()?.to_string(),
        planar: v["planar"].as_bool().unwrap_or(false),
        center: vec3(&v["center"])?,
        normal: vec3(&v["normal"])?,
        border: border(&triangles),
        points,
        triangles,
    })
}

/// The triangle sides used once: the border of a triangulated face, holes included.
pub fn border(triangles: &[[u32; 3]]) -> Vec<[u32; 2]> {
    let mut count: std::collections::HashMap<(u32, u32), (u32, [u32; 2])> = std::collections::HashMap::new();
    for t in triangles {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            count.entry((a.min(b), a.max(b))).or_insert((0, [a, b])).0 += 1;
        }
    }
    let mut out: Vec<[u32; 2]> = count.into_values().filter(|(n, _)| *n == 1).map(|(_, s)| s).collect();
    out.sort_unstable();
    out
}

/// Points of the model to points of the view `rect`, through `view_proj`; None behind the eye.
pub fn projector(view_proj: Mat4, rect: Rect) -> impl Fn(Vec3) -> Option<Pos2> {
    move |p: Vec3| {
        let c = view_proj * p.extend(1.0);
        (c.w > 1e-6).then(|| {
            let n = c.truncate() / c.w;
            pos2(rect.left() + (n.x * 0.5 + 0.5) * rect.width(), rect.top() + (0.5 - n.y * 0.5) * rect.height())
        })
    }
}

pub fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = if ab.length_sq() > 0.0 { ((p - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
    (p - (a + ab * t)).length()
}

/// How far `pos` is from the edge on screen, in points.
pub fn edge_distance(e: &EdgeInfo, project: &dyn Fn(Vec3) -> Option<Pos2>, pos: Pos2) -> f32 {
    let pts: Vec<Option<Pos2>> = e.points.iter().map(|&p| project(p)).collect();
    pts.windows(2)
        .filter_map(|w| Some(segment_distance(pos, w[0]?, w[1]?)))
        .fold(f32::INFINITY, f32::min)
}

/// The edge nearest `pos` within `max` points among those `keep` admits.
pub fn nearest_edge<'a>(edges: &'a [EdgeInfo], keep: impl Fn(&EdgeInfo) -> bool, project: &dyn Fn(Vec3) -> Option<Pos2>, pos: Pos2, max: f32) -> Option<&'a EdgeInfo> {
    edges
        .iter()
        .filter(|e| keep(e))
        .map(|e| (e, edge_distance(e, project, pos)))
        .filter(|(_, d)| *d <= max)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

/// A handle dragged along `dir` from `base`: how far in model units a pointer move of `delta`
/// points goes along it, or None when `dir` points at the eye and a drag cannot tell.
pub fn drag_amount(base: Vec3, dir: Vec3, project: &dyn Fn(Vec3) -> Option<Pos2>, delta: Vec2) -> Option<f32> {
    let v = project(base + dir)? - project(base)?;
    (v.length_sq() > 1e-4).then(|| delta.dot(v) / v.length_sq())
}

/// A round step for values dragged at `px_per_unit` points per model unit: 1, 2 or 5 times a
/// power of ten, about 4 points of pointer travel.
pub fn nice_step(px_per_unit: f32) -> f64 {
    let raw = (4.0 / px_per_unit.max(1e-6)) as f64;
    let p = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0].into_iter().map(|k| k * p).find(|s| *s >= raw).unwrap_or(10.0 * p)
}

/// The handle's arrow on screen: tail at `base`, `len` points long along `dir`.
pub fn arrow(base: Vec3, dir: Vec3, project: &dyn Fn(Vec3) -> Option<Pos2>, len: f32) -> Option<(Pos2, Pos2)> {
    let (a, b) = (project(base)?, project(base + dir)?);
    let d = (b - a).normalized();
    (d.x.is_finite() && d.y.is_finite() && (b - a).length() > 1e-3).then(|| (a, a + d * len))
}

pub const HANDLE: Color32 = Color32::from_rgb(30, 120, 230);

/// A drag handle: a shaft, a head and the value at its tip.
pub fn paint_arrow(painter: &egui::Painter, tail: Pos2, tip: Pos2, hot: bool, text: &str) {
    let color = if hot { Color32::from_rgb(250, 150, 30) } else { HANDLE };
    let d = (tip - tail).normalized();
    let side = egui::vec2(-d.y, d.x);
    painter.circle_filled(tail, 3.5, color);
    painter.line_segment([tail, tip - d * 10.0], Stroke::new(3.0, color));
    painter.add(egui::Shape::convex_polygon(vec![tip, tip - d * 14.0 + side * 7.0, tip - d * 14.0 - side * 7.0], color, Stroke::NONE));
    let font = egui::FontId::proportional(13.0);
    let at = tip + d * 10.0 + side * 4.0;
    let galley = painter.layout_no_wrap(text.to_string(), font, Color32::from_gray(20));
    let r = egui::Align2::LEFT_CENTER.anchor_size(at, galley.size()).expand2(egui::vec2(5.0, 2.0));
    painter.rect(r, 3.0, Color32::from_white_alpha(230), Stroke::new(1.0, color), egui::StrokeKind::Inside);
    painter.galley(r.min + egui::vec2(5.0, 2.0), galley, Color32::from_gray(20));
}

/// An edge drawn over the view, thick: picked (`hot` false) or under the cursor.
pub fn paint_edge(painter: &egui::Painter, e: &EdgeInfo, project: &dyn Fn(Vec3) -> Option<Pos2>, color: Color32) {
    let pts: Vec<Option<Pos2>> = e.points.iter().map(|&p| project(p)).collect();
    for w in pts.windows(2) {
        if let (Some(a), Some(b)) = (w[0], w[1]) {
            painter.line_segment([a, b], Stroke::new(3.5, color));
        }
    }
}

/// The solid a push or pull adds (blue) or cuts (red): the face moved `distance` along its
/// normal, its border at both ends and the walls between.
pub fn paint_extrusion(painter: &egui::Painter, f: &FaceInfo, distance: f32, project: &dyn Fn(Vec3) -> Option<Pos2>) {
    let (fill, line) = if distance >= 0.0 {
        (Color32::from_rgba_unmultiplied(40, 130, 235, 70), Color32::from_rgb(30, 110, 220))
    } else {
        (Color32::from_rgba_unmultiplied(225, 70, 50, 70), Color32::from_rgb(200, 50, 40))
    };
    let shift = f.normal * distance;
    let near: Vec<Option<Pos2>> = f.points.iter().map(|&p| project(p)).collect();
    let far: Vec<Option<Pos2>> = f.points.iter().map(|&p| project(p + shift)).collect();
    let mut mesh = egui::epaint::Mesh::default();
    let mut tri = |a: Option<Pos2>, b: Option<Pos2>, c: Option<Pos2>| {
        if let (Some(a), Some(b), Some(c)) = (a, b, c) {
            let i = mesh.vertices.len() as u32;
            for p in [a, b, c] {
                mesh.colored_vertex(p, fill);
            }
            mesh.add_triangle(i, i + 1, i + 2);
        }
    };
    for t in &f.triangles {
        let [a, b, c] = t.map(|i| i as usize);
        tri(far[a], far[b], far[c]);
    }
    for s in &f.border {
        let [a, b] = s.map(|i| i as usize);
        tri(near[a], near[b], far[b]);
        tri(near[a], far[b], far[a]);
    }
    painter.add(egui::Shape::mesh(mesh));
    for s in &f.border {
        let [a, b] = s.map(|i| i as usize);
        for (p, q) in [(far[a], far[b]), (near[a], near[b])] {
            if let (Some(p), Some(q)) = (p, q) {
                painter.line_segment([p, q], Stroke::new(1.5, line));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_border_of_a_square_is_its_four_sides() {
        let b = border(&[[0, 1, 2], [0, 2, 3]]);
        assert_eq!(b.len(), 4);
        assert!(!b.iter().any(|s| (s[0] == 0 && s[1] == 2) || (s[0] == 2 && s[1] == 0)), "the diagonal is inside");
    }

    #[test]
    fn edges_and_faces_parse_from_the_worker() {
        let e = parse_edges(&json!({ "edges": [{ "i": 3, "faces": [0, 4], "points": [[0, 0, 0], [0, 0, 6]], "mid": [0, 0, 3], "out": [-0.7, -0.7, 0] }] }));
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].index, e[0].faces.clone(), e[0].points.len()), (3, vec![0, 4], 2));
        let f = parse_face(&json!({ "obj": "Pocket", "sub": "Face5", "planar": true, "center": [40, 25, 6], "normal": [0, 0, 1],
            "points": [[0, 0, 6], [80, 0, 6], [80, 50, 6], [0, 50, 6]], "triangles": [[0, 1, 2], [0, 2, 3], [0, 2, 9]] }))
        .unwrap();
        assert_eq!(f.triangles.len(), 2, "a triangle naming a missing point is dropped");
        assert_eq!(f.border.len(), 4);
    }

    #[test]
    fn the_nearest_edge_within_reach_is_picked() {
        let project = |p: Vec3| Some(pos2(p.x, p.y));
        let edge = |i, a: [f32; 2], b: [f32; 2]| EdgeInfo { index: i, faces: vec![], points: vec![Vec3::new(a[0], a[1], 0.0), Vec3::new(b[0], b[1], 0.0)], mid: Vec3::ZERO, out: Vec3::ZERO };
        let edges = [edge(1, [0.0, 0.0], [100.0, 0.0]), edge(2, [0.0, 10.0], [100.0, 10.0])];
        assert_eq!(nearest_edge(&edges, |_| true, &project, pos2(50.0, 3.0), 6.0).map(|e| e.index), Some(1));
        assert_eq!(nearest_edge(&edges, |_| true, &project, pos2(50.0, 7.0), 6.0).map(|e| e.index), Some(2));
        assert!(nearest_edge(&edges, |_| true, &project, pos2(50.0, 30.0), 6.0).is_none());
        assert!(nearest_edge(&edges, |e| e.index == 2, &project, pos2(50.0, 1.0), 6.0).is_none());
    }

    #[test]
    fn a_drag_along_the_handle_measures_model_units() {
        // 10 points per unit along +x on screen.
        let project = |p: Vec3| Some(pos2(p.x * 10.0, -p.z * 10.0));
        let d = drag_amount(Vec3::ZERO, Vec3::X, &project, egui::vec2(25.0, 40.0)).unwrap();
        assert!((d - 2.5).abs() < 1e-6, "only the part along the handle counts");
        assert!(drag_amount(Vec3::ZERO, Vec3::Y, &project, egui::vec2(5.0, 5.0)).is_none(), "a handle pointing at the eye");
    }

    #[test]
    fn drag_steps_are_round() {
        for (px, step) in [(10.0, 0.5), (1.0, 5.0), (100.0, 0.05), (3.0, 2.0)] {
            assert!((nice_step(px) - step).abs() < 1e-9, "{px}: {}", nice_step(px));
        }
    }
}
