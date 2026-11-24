//! The window's sketch mode, as in Fusion: a sketch is edited in place, the camera facing it;
//! the ribbon holds the drawing tools; clicks snap to points and line up with the click before,
//! and each finished shape is one `sketch_edit` (one undo step) with the constraints the snaps
//! imply. The sketch is drawn over the view from the worker's description of it, its closed
//! profiles shaded; FreeCAD's solver keeps it right. Finish (or Esc) leaves the mode.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke};
use glam::{DVec2, Mat4};
use serde_json::{Value, json};

use super::{App, Pending, Selection};
use crate::picking;
use crate::sketcher::{self, Align, Drawing, Shape, Sketch, Snap, Target, Tool};

/// How near the pointer an element is picked, in points.
const ELEMENT_REACH: f32 = 6.0;

const PROFILE: Color32 = Color32::from_rgba_premultiplied(64, 43, 15, 64);
const FREE: Color32 = Color32::from_rgb(30, 110, 230);
const FIXED: Color32 = Color32::from_rgb(25, 25, 25);
const CONSTRUCTION: Color32 = Color32::from_rgb(230, 140, 50);
const PICKED: Color32 = Color32::from_rgb(250, 120, 20);
const HOVERED: Color32 = Color32::from_rgb(120, 185, 255);
const PREVIEW: Color32 = Color32::from_rgb(90, 160, 250);

pub(super) struct SketchMode {
    pub obj: String,
    pub data: Option<Sketch>,
    pub drawing: Option<Drawing>,
    /// Where a click would land now, while drawing.
    hover: Option<Snap>,
    /// The element under the pointer, while not drawing.
    hover_element: Option<usize>,
    pub selected: Vec<usize>,
    /// Elements that edits on their way to the worker will add: the next element's index is
    /// the sketch's count plus these.
    pending_new: usize,
    /// The camera was turned to face the sketch.
    faced: bool,
}

impl App {
    /// Edit the sketch `obj` in place; `data` is its description when the reply that made it
    /// carried one.
    pub(super) fn edit_sketch(&mut self, obj: &str, data: Option<&Value>) {
        self.command = None;
        self.select(None);
        self.sketch = Some(SketchMode {
            obj: obj.to_string(),
            data: data.and_then(Sketch::parse),
            drawing: None,
            hover: None,
            hover_element: None,
            selected: Vec::new(),
            pending_new: 0,
            faced: false,
        });
        if self.sketch.as_ref().is_some_and(|m| m.data.is_some()) {
            self.face_sketch();
        } else {
            self.sketch_resync();
        }
        self.info = Some("sketch: pick a tool in the ribbon; Esc or Finish sketch leaves".into());
    }

    pub(super) fn finish_sketch(&mut self) {
        if let Some(m) = self.sketch.take() {
            self.select(Some(Selection { obj: m.obj, sub: None, face_id: 0 }));
            self.refresh();
        }
    }

    /// Ask for the sketch again (after an undo, the agent's edit, a refused edit).
    pub(super) fn sketch_resync(&mut self) {
        if let Some(obj) = self.sketch.as_ref().map(|m| m.obj.clone()) {
            self.request("sketch", json!({ "obj": obj, "outlines": true }), Pending::SketchData);
        }
    }

    pub(super) fn on_sketch_data(&mut self, v: &Value) {
        let Some(m) = self.sketch.as_mut().filter(|m| v["obj"].as_str() == Some(m.obj.as_str())) else { return };
        m.data = Sketch::parse(v);
        let count = m.data.as_ref().map_or(0, |d| d.count);
        m.selected.retain(|&i| i < count);
        if !m.faced {
            self.face_sketch();
        }
    }

    pub(super) fn on_sketch_edit(&mut self, adds: usize, v: &Value) {
        self.modified = true;
        if let Some(m) = self.sketch.as_mut() {
            m.pending_new = m.pending_new.saturating_sub(adds);
            if let Some(d) = Sketch::parse(&v["sketch"]).filter(|d| d.obj == m.obj) {
                m.data = Some(d);
            }
        }
        self.refresh();
    }

    /// A sketch edit was refused (the error is shown) or the sketch is gone.
    pub(super) fn on_sketch_error(&mut self, kind: &Pending) {
        match kind {
            Pending::SketchEdit(_) => {
                if let Some(m) = self.sketch.as_mut() {
                    m.pending_new = 0;
                    if let Some(d) = m.drawing.as_mut() {
                        *d = Drawing::new(d.tool); // indices it counted on may be wrong now
                    }
                }
                self.sketch_resync();
            }
            Pending::SketchData => {
                // An undo took the sketch away, or it was deleted.
                self.sketch = None;
                self.refresh();
            }
            _ => {}
        }
    }

    /// Turn the camera to look straight at the sketch, its x to the right, and centre it.
    fn face_sketch(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        let Some(d) = &m.data else { return };
        m.faced = true;
        let p = d.plane;
        if p.normal.z.abs() > 0.999 {
            self.camera.pitch = p.normal.z.signum() * (std::f32::consts::FRAC_PI_2 - 1e-4);
            self.camera.yaw = (-p.x.x).atan2(p.x.y);
        } else {
            crate::ui_chrome::look_from(&mut self.camera, p.normal);
        }
        match d.bounds() {
            Some((lo, hi)) => {
                let (c, half) = ((lo + hi) * 0.5, ((hi - lo) * 0.75).max(DVec2::splat(10.0)));
                let corners = [c - half, c + half].map(|q| p.world(q));
                self.camera.fit(corners[0].min(corners[1]), corners[0].max(corners[1]));
            }
            // An empty sketch: keep the zoom, centre the plane's point under the old target.
            None => self.camera.target = p.world(p.local(self.camera.target)),
        }
    }

    pub(super) fn set_sketch_tool(&mut self, tool: Option<Tool>) {
        if let Some(m) = self.sketch.as_mut() {
            m.drawing = tool.map(Drawing::new);
            m.hover = None;
            m.selected.clear();
        }
    }

    /// Where a point of the sketch is in the view, as the last frame drew it.
    pub(super) fn sketch_screen_point(&self, p: DVec2) -> Option<Pos2> {
        let (vp, rect) = self.gizmos_last_view()?;
        let plane = self.sketch.as_ref()?.data.as_ref()?.plane;
        picking::projector(vp, rect)(plane.world(p))
    }

    /// The pointer on the sketch's plane and how many mm a point of the screen is there.
    fn on_plane(&self, pos: Pos2) -> Option<(DVec2, f64)> {
        let (vp, rect) = self.gizmos_last_view()?;
        let plane = self.sketch.as_ref()?.data.as_ref()?.plane;
        let here = plane.hit(vp, rect, pos)?;
        let next = plane.hit(vp, rect, pos + egui::vec2(1.0, 0.0))?;
        Some((here, here.distance(next).max(1e-9)))
    }

    fn snap_at(&self, pos: Pos2) -> Option<Snap> {
        let (cursor, mm_per_pt) = self.on_plane(pos)?;
        let m = self.sketch.as_ref()?;
        let prev = m.drawing.as_ref().and_then(Drawing::last);
        Some(sketcher::snap(m.data.as_ref()?, cursor, mm_per_pt, prev, &[]))
    }

    /// The pointer moved over the view (None: it left).
    pub(super) fn sketch_hover(&mut self, pos: Option<Pos2>) {
        let drawing = self.sketch.as_ref().is_some_and(|m| m.drawing.is_some());
        let snap = pos.filter(|_| drawing).and_then(|p| self.snap_at(p));
        let element = pos.filter(|_| !drawing).and_then(|p| self.sketch_element_at(p));
        if let Some(m) = self.sketch.as_mut() {
            m.hover = snap;
            m.hover_element = element;
        }
    }

    fn sketch_element_at(&self, pos: Pos2) -> Option<usize> {
        let (vp, rect) = self.gizmos_last_view()?;
        let d = self.sketch.as_ref()?.data.as_ref()?;
        let project = picking::projector(vp, rect);
        let plane = d.plane;
        sketcher::element_at(d, &|q| project(plane.world(q)), pos, ELEMENT_REACH)
    }

    /// A click in the view: the drawing tool's next point, or picking elements (Ctrl or Shift
    /// adds to the picked ones).
    pub(super) fn sketch_click(&mut self, pos: Pos2, add: bool) {
        let drawing = self.sketch.as_ref().is_some_and(|m| m.drawing.is_some());
        if !drawing {
            let hit = self.sketch_element_at(pos);
            if let Some(m) = self.sketch.as_mut() {
                match hit {
                    Some(i) if add => match m.selected.iter().position(|&j| j == i) {
                        Some(k) => {
                            m.selected.remove(k);
                        }
                        None => m.selected.push(i),
                    },
                    Some(i) => m.selected = vec![i],
                    None if !add => m.selected.clear(),
                    None => {}
                }
            }
            return;
        }
        let Some(snap) = self.snap_at(pos) else { return };
        let Some(m) = self.sketch.as_mut() else { return };
        let Some(count) = m.data.as_ref().map(|d| d.count) else { return };
        let next = count + m.pending_new;
        let Some(edit) = m.drawing.as_mut().and_then(|d| d.click(snap, next)) else { return };
        m.pending_new += edit.adds;
        let mut args = edit.args;
        args["obj"] = json!(m.obj);
        args["outlines"] = json!(true);
        self.request("sketch_edit", args, Pending::SketchEdit(edit.adds));
    }

    /// A double click ends a line chain; True when it did something.
    pub(super) fn sketch_double_click(&mut self) -> bool {
        match self.sketch.as_mut().and_then(|m| m.drawing.as_mut()) {
            Some(d) => {
                *d = Drawing::new(d.tool);
                true
            }
            None => false,
        }
    }

    /// Esc: drop the shape being drawn, then the tool, then the picked elements, then leave.
    pub(super) fn sketch_escape(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        match &mut m.drawing {
            Some(d) if !d.clicks.is_empty() || d.chain.is_some() => *d = Drawing::new(d.tool),
            Some(_) => m.drawing = None,
            None if !m.selected.is_empty() => m.selected.clear(),
            None => self.finish_sketch(),
        }
    }

    pub(super) fn sketch_delete(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        if m.selected.is_empty() {
            return;
        }
        let args = json!({ "obj": m.obj, "remove": { "geometry": m.selected }, "outlines": true });
        m.selected.clear();
        self.request("sketch_edit", args, Pending::SketchEdit(0));
    }

    /// Picked elements become construction geometry, or back when they all are.
    pub(super) fn sketch_construction(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        let Some(d) = &m.data else { return };
        if m.selected.is_empty() {
            return;
        }
        let all = m.selected.iter().all(|&i| d.elements.iter().any(|e| e.index == i && e.construction));
        let list: Vec<Value> = m.selected.iter().map(|&i| json!({ "geometry": i, "on": !all })).collect();
        let args = json!({ "obj": m.obj, "construction": list, "outlines": true });
        self.request("sketch_edit", args, Pending::SketchEdit(0));
    }

    /// What the next click does, for the line over the view.
    pub(super) fn sketch_prompt(&self) -> Option<String> {
        let m = self.sketch.as_ref()?;
        let d = m.data.as_ref();
        let state = match d {
            None => "loading…".to_string(),
            Some(d) if d.fully_constrained => "fully constrained".to_string(),
            Some(d) => d.dof.map_or(String::new(), |n| format!("{n} degrees of freedom")),
        };
        let label = d.map_or(m.obj.as_str(), |d| d.label.as_str());
        Some(match &m.drawing {
            Some(dr) => format!("{label} · {}: {} · {state}", dr.tool.label(), dr.prompt()),
            None if !m.selected.is_empty() => format!("{label} · {} picked: Del deletes, X makes construction · {state}", m.selected.len()),
            None => format!("{label} · pick a tool, or click elements · Esc finishes · {state}"),
        })
    }

    /// The sketch over the view: profiles shaded, elements, points, the origin, and while
    /// drawing the shape the next click makes and what the pointer snaps to.
    pub(super) fn paint_sketch(&self, painter: &egui::Painter, rect: Rect, view_proj: Mat4) {
        let Some(m) = &self.sketch else { return };
        let Some(d) = &m.data else { return };
        let project = picking::projector(view_proj, rect);
        let plane = d.plane;
        let at = |q: DVec2| project(plane.world(q));
        for p in &d.profiles {
            let mut mesh = egui::Mesh::default();
            let pts: Vec<Option<Pos2>> = p.points.iter().map(|&q| at(q)).collect();
            if pts.iter().any(Option::is_none) {
                continue;
            }
            for q in pts.into_iter().flatten() {
                mesh.colored_vertex(q, PROFILE);
            }
            for t in &p.triangles {
                mesh.add_triangle(t[0], t[1], t[2]);
            }
            painter.add(egui::Shape::mesh(mesh));
        }
        // The origin, and its axes a little way out, as Fusion shows them.
        if let (Some(o), Some(x), Some(y)) = (at(DVec2::ZERO), project(plane.origin + plane.x), project(plane.origin + plane.y)) {
            for (end, color) in [(x, Color32::from_rgb(220, 60, 60)), (y, Color32::from_rgb(60, 170, 60))] {
                let dir = (end - o).normalized();
                if dir.x.is_finite() {
                    painter.line_segment([o, o + dir * 28.0], Stroke::new(1.2, color));
                }
            }
            painter.circle_filled(o, 3.0, CONSTRUCTION);
        }
        let line = if d.fully_constrained { FIXED } else { FREE };
        for e in &d.elements {
            let picked = m.selected.contains(&e.index);
            let color = if picked {
                PICKED
            } else if m.hover_element == Some(e.index) {
                HOVERED
            } else if e.construction {
                CONSTRUCTION
            } else {
                line
            };
            let width = if picked { 2.6 } else { 1.8 };
            let pts: Vec<Pos2> = e.shape.polyline().into_iter().filter_map(at).collect();
            match (&e.shape, pts.as_slice()) {
                (Shape::Point(_), [p]) => {
                    painter.circle_filled(*p, 3.2, color);
                }
                _ if e.construction && !picked => painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(width, color), 6.0, 4.0)),
                _ => {
                    painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
                }
            }
            if !matches!(e.shape, Shape::Point(_)) {
                for (_, q) in e.shape.points() {
                    if let Some(s) = at(q) {
                        painter.circle_filled(s, 2.2, color);
                    }
                }
            }
        }
        let (Some(dr), Some(snap)) = (&m.drawing, &m.hover) else { return };
        for s in dr.preview(snap.at) {
            let pts: Vec<Pos2> = s.polyline().into_iter().filter_map(at).collect();
            painter.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.6, PREVIEW), 7.0, 4.0));
        }
        let Some(cur) = at(snap.at) else { return };
        if snap.target.is_some() {
            painter.rect_stroke(Rect::from_center_size(cur, egui::vec2(10.0, 10.0)), 1.0, Stroke::new(1.8, PICKED), egui::StrokeKind::Middle);
        } else {
            painter.circle_stroke(cur, 3.0, Stroke::new(1.4, PREVIEW));
        }
        let mut label = match dr.preview(snap.at).first() {
            Some(Shape::Line(a, b)) if dr.tool == Tool::Line => format!("{} mm", trim(a.distance(*b))),
            Some(Shape::Circle(_, r)) => format!("⌀ {} mm", trim(2.0 * r)),
            Some(Shape::Line(a, _)) if matches!(dr.tool, Tool::Rectangle | Tool::CenterRectangle) => {
                let (lo, hi) = (a.min(snap.at), a.max(snap.at));
                let w = if dr.tool == Tool::CenterRectangle { 2.0 * (snap.at - dr.last().unwrap_or(*a)).abs() } else { hi - lo };
                format!("{} × {} mm", trim(w.x), trim(w.y))
            }
            _ => format!("{}, {}", trim(snap.at.x), trim(snap.at.y)),
        };
        match (snap.align, snap.target) {
            (Some(Align::Horizontal), _) => label.push_str("  ⟷ horizontal"),
            (Some(Align::Vertical), _) => label.push_str("  ↕ vertical"),
            (None, Some(Target::Origin)) => label.push_str("  on the origin"),
            (None, Some(Target::Point(..))) => label.push_str("  on a point"),
            _ => {}
        }
        painter.text(cur + egui::vec2(12.0, -12.0), egui::Align2::LEFT_BOTTOM, label, egui::FontId::proportional(12.0), PREVIEW);
    }

    /// The sketch's panel, where the properties stand outside sketch mode.
    pub(super) fn sketch_panel(&mut self, ui: &mut egui::Ui) {
        let Some(m) = &self.sketch else { return };
        let Some(d) = &m.data else {
            ui.weak("loading…");
            return;
        };
        ui.strong(&d.label);
        let constrained = if d.fully_constrained { "fully constrained".to_string() } else { d.dof.map_or("—".into(), |n| format!("{n} degrees of freedom")) };
        egui::Grid::new("sketch").num_columns(2).spacing(egui::vec2(10.0, 4.0)).show(ui, |ui| {
            ui.label("Elements");
            ui.label(format!("{} ({} construction)", d.count, d.elements.iter().filter(|e| e.construction).count()));
            ui.end_row();
            ui.label("Constraints");
            ui.label(d.constraints.len().to_string());
            ui.end_row();
            ui.label("Solver");
            ui.label(constrained);
            ui.end_row();
            ui.label("Profiles");
            ui.label(d.profiles.len().to_string()).on_hover_text("Closed regions an extrude takes, shaded orange");
            ui.end_row();
        });
        let picked = m.selected.len();
        ui.separator();
        let mut act = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(picked > 0, egui::Button::new("Construction")).on_hover_text("Picked elements become guides, or back (X)").clicked() {
                act = Some(0);
            }
            if ui.add_enabled(picked > 0, egui::Button::new("Delete")).on_hover_text("Del").clicked() {
                act = Some(1);
            }
            if ui.button(egui::RichText::new("Finish sketch").strong()).on_hover_text("Esc").clicked() {
                act = Some(2);
            }
        });
        match act {
            Some(0) => self.sketch_construction(),
            Some(1) => self.sketch_delete(),
            Some(2) => self.finish_sketch(),
            _ => {}
        }
    }
}

/// A number without trailing zeros, to a hundredth: 12.5, 3, 0.05.
fn trim(v: f64) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}
