//! The window's sketch mode, as in Fusion: a sketch is edited in place, the camera facing it;
//! the ribbon holds the drawing, constraint and dimension tools; clicks snap to points and line
//! up with the click before, and each finished shape, constraint or dimension is one
//! `sketch_edit` (one undo step). Points and lines are dragged under FreeCAD's solver: while
//! the pointer moves, dry runs show where the solver puts everything, and the release makes the
//! move. The sketch is drawn over the view from the worker's description of it: elements,
//! closed profiles shaded, dimensions with their values and the marks of the other constraints.
//! Finish (or Esc) leaves the mode.

use eframe::egui::{self, Color32, PointerButton, Pos2, Rect, Stroke};
use glam::{DVec2, Mat4};
use serde_json::{Value, json};

use super::{App, Pending, Selection};
use crate::picking;
use crate::sketcher::{self, Align, Constrain, Drawing, Pick, Shape, Sketch, Snap, Target, Tool};

/// How near the pointer an element is picked, in points.
const ELEMENT_REACH: f32 = 6.0;

const PROFILE: Color32 = Color32::from_rgba_premultiplied(64, 43, 15, 64);
const FREE: Color32 = Color32::from_rgb(30, 110, 230);
const FIXED: Color32 = Color32::from_rgb(25, 25, 25);
const CONSTRUCTION: Color32 = Color32::from_rgb(230, 140, 50);
const PICKED: Color32 = Color32::from_rgb(250, 120, 20);
const HOVERED: Color32 = Color32::from_rgb(120, 185, 255);
const PREVIEW: Color32 = Color32::from_rgb(90, 160, 250);
const LABEL: Color32 = Color32::from_rgb(40, 40, 40);

pub(super) struct SketchMode {
    pub obj: String,
    pub data: Option<Sketch>,
    pub drawing: Option<Drawing>,
    /// A constraint or the dimension tool, and what it has picked so far.
    pub constrain: Option<Constrain>,
    picks: Vec<Pick>,
    /// Where a click would land now, while drawing.
    hover: Option<Snap>,
    /// What a click would pick now, while not drawing.
    hover_pick: Option<Pick>,
    /// Picked elements and constraints (dimensions and marks), for Delete and Construction.
    pub selected: Vec<usize>,
    pub selected_constraints: Vec<usize>,
    /// A dimension's value being typed.
    editor: Option<Editor>,
    drag: Option<Drag>,
    /// Elements that edits on their way to the worker will add: the next element's index is
    /// the sketch's count plus these.
    pending_new: usize,
    /// The camera was turned to face the sketch.
    faced: bool,
}

struct Editor {
    kind: EditorKind,
    text: String,
    /// Where it opens, in the view.
    at: Pos2,
    /// What the value is ("length", "angle", ...), for its caption.
    what: String,
    focus: bool,
}

enum EditorKind {
    /// A new dimension: the constraint without its value, and the value it measures now.
    New { constraint: Value, measured: f64 },
    /// A dimension the sketch has: its property ("Constraints.Width").
    Existing { property: String, measured: f64 },
}

/// A point or line being dragged.
struct Drag {
    pick: Pick,
    /// Where it was taken, in the sketch.
    from: DVec2,
    /// The pointer's place last sent for a dry run, and whether its answer is still out.
    sent: Option<DVec2>,
    waiting: bool,
    /// Dry runs drop the steps redo would bring back: with some waiting, only the release moves.
    live: bool,
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
            constrain: None,
            picks: Vec::new(),
            hover: None,
            hover_pick: None,
            selected: Vec::new(),
            selected_constraints: Vec::new(),
            editor: None,
            drag: None,
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
        if m.data.as_ref().is_some_and(|d| Sketch::parse(v).as_ref() != Some(d)) {
            m.editor = None;
            m.picks.clear();
        }
        m.data = Sketch::parse(v);
        let (count, constraints) = m.data.as_ref().map_or((0, 0), |d| (d.count, d.constraints.len()));
        m.selected.retain(|&i| i < count);
        m.selected_constraints.retain(|&i| i < constraints);
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

    /// A dry run of a drag came back: show where the solver puts everything.
    pub(super) fn on_sketch_preview(&mut self, v: Option<&Value>) {
        let Some(m) = self.sketch.as_mut() else { return };
        if let Some(d) = m.drag.as_mut() {
            d.waiting = false;
        }
        if m.drag.is_some()
            && let Some(d) = v.and_then(|v| Sketch::parse(&v["sketch"])).filter(|d| d.obj == m.obj)
        {
            m.data = Some(d);
        }
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
            Pending::SketchPreview => self.on_sketch_preview(None),
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
            m.constrain = None;
            m.picks.clear();
            m.editor = None;
            m.hover = None;
            m.selected.clear();
            m.selected_constraints.clear();
        }
    }

    /// A constraint tool (or the dimension tool). Elements picked before apply at once when
    /// they are what the tool takes, as Fusion does.
    pub(super) fn set_constrain_tool(&mut self, tool: Option<Constrain>) {
        let Some(m) = self.sketch.as_mut() else { return };
        m.drawing = None;
        m.constrain = tool;
        m.editor = None;
        m.picks = m.selected.iter().map(|&i| Pick::Element(i)).collect();
        m.selected.clear();
        m.selected_constraints.clear();
        if m.picks.is_empty() {
            return;
        }
        let picks = std::mem::take(&mut m.picks);
        for p in picks {
            self.sketch_take_pick(p, None);
        }
    }

    /// The pointer on the sketch's plane and how many mm a point of the screen is there.
    fn on_plane(&self, pos: Pos2) -> Option<(DVec2, f64)> {
        let (vp, rect) = self.gizmos_last_view()?;
        let plane = self.sketch.as_ref()?.data.as_ref()?.plane;
        let here = plane.hit(vp, rect, pos)?;
        let next = plane.hit(vp, rect, pos + egui::vec2(1.0, 0.0))?;
        Some((here, here.distance(next).max(1e-9)))
    }

    /// Where a point of the sketch is in the view, as the last frame drew it.
    pub(super) fn sketch_screen_point(&self, p: DVec2) -> Option<Pos2> {
        let (vp, rect) = self.gizmos_last_view()?;
        let plane = self.sketch.as_ref()?.data.as_ref()?.plane;
        picking::projector(vp, rect)(plane.world(p))
    }

    fn snap_at(&self, pos: Pos2) -> Option<Snap> {
        let (cursor, mm_per_pt) = self.on_plane(pos)?;
        let m = self.sketch.as_ref()?;
        let prev = m.drawing.as_ref().and_then(Drawing::last);
        Some(sketcher::snap(m.data.as_ref()?, cursor, mm_per_pt, prev, &[]))
    }

    fn pick_under(&self, pos: Pos2) -> Option<Pick> {
        let (vp, rect) = self.gizmos_last_view()?;
        let d = self.sketch.as_ref()?.data.as_ref()?;
        let project = picking::projector(vp, rect);
        let plane = d.plane;
        sketcher::pick_at(d, &|q| project(plane.world(q)), pos, ELEMENT_REACH)
    }

    /// The pointer moved over the view (None: it left).
    pub(super) fn sketch_hover(&mut self, pos: Option<Pos2>) {
        let (drawing, constrain) = match &self.sketch {
            Some(m) => (m.drawing.is_some(), m.constrain.is_some()),
            None => return,
        };
        let snap = pos.filter(|_| drawing).and_then(|p| self.snap_at(p));
        // A constraint tool picks points on their own; with no tool, their element lights up.
        let pick = match pos.filter(|_| !drawing) {
            Some(p) if constrain => self.pick_under(p),
            Some(p) => self.sketch_element_at(p).map(Pick::Element),
            None => None,
        };
        if let Some(m) = self.sketch.as_mut() {
            m.hover = snap;
            m.hover_pick = pick;
        }
    }

    fn sketch_element_at(&self, pos: Pos2) -> Option<usize> {
        let (vp, rect) = self.gizmos_last_view()?;
        let d = self.sketch.as_ref()?.data.as_ref()?;
        let project = picking::projector(vp, rect);
        let plane = d.plane;
        sketcher::element_at(d, &|q| project(plane.world(q)), pos, ELEMENT_REACH)
    }

    /// A click in the view: the drawing tool's next point, a constraint tool's next pick, or
    /// picking elements and constraints (Ctrl or Shift adds).
    pub(super) fn sketch_click(&mut self, pos: Pos2, add: bool) {
        let (drawing, constrain) = match &self.sketch {
            Some(m) => (m.drawing.is_some(), m.constrain),
            None => return,
        };
        if constrain.is_some() {
            if let Some(p) = self.pick_under(pos) {
                self.sketch_take_pick(p, Some(pos));
            } else if let Some(m) = self.sketch.as_mut()
                && m.editor.is_none()
            {
                m.picks.clear();
            }
            return;
        }
        if !drawing {
            let mark = self.sketch_mark_at(pos);
            let hit = if mark.is_some() { None } else { self.sketch_element_at(pos) };
            if let Some(m) = self.sketch.as_mut() {
                if !add {
                    m.selected.clear();
                    m.selected_constraints.clear();
                }
                let toggle = |list: &mut Vec<usize>, i: usize| match list.iter().position(|&j| j == i) {
                    Some(k) => {
                        list.remove(k);
                    }
                    None => list.push(i),
                };
                if let Some(c) = mark {
                    toggle(&mut m.selected_constraints, c);
                } else if let Some(i) = hit {
                    toggle(&mut m.selected, i);
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

    /// A pick for the constraint or dimension tool: a constraint goes to the worker once the
    /// picks are enough; a dimension opens its value box by the pointer (`at`).
    fn sketch_take_pick(&mut self, p: Pick, at: Option<Pos2>) {
        let Some(m) = self.sketch.as_mut() else { return };
        let (Some(tool), Some(d)) = (m.constrain, m.data.as_ref()) else { return };
        if tool == Constrain::Dimension && m.picks.len() >= 2 {
            m.picks.clear();
        }
        m.picks.push(p);
        if tool == Constrain::Dimension {
            match sketcher::dimension_for(d, &m.picks) {
                Ok(Some((constraint, measured, what))) => {
                    let at = at.or_else(|| m.editor.as_ref().map(|e| e.at)).unwrap_or(Pos2::new(200.0, 200.0));
                    m.editor = Some(Editor { kind: EditorKind::New { constraint, measured }, text: trim(measured.abs()), at, what: what.into(), focus: true });
                }
                Ok(None) => {}
                Err(e) => {
                    m.picks.clear();
                    m.editor = None;
                    self.info = Some(e);
                }
            }
            return;
        }
        match sketcher::constraint_for(tool, d, &m.picks) {
            Ok(Some(c)) => {
                m.picks.clear();
                let args = json!({ "obj": m.obj, "constraints": [c], "outlines": true });
                self.request("sketch_edit", args, Pending::SketchEdit(0));
            }
            Ok(None) => {}
            Err(e) => {
                m.picks.clear();
                self.info = Some(e);
            }
        }
    }

    /// A double click ends a line chain, or opens a dimension's value to type a new one.
    /// True when it did something.
    pub(super) fn sketch_double_click(&mut self, pos: Option<Pos2>) -> bool {
        let Some(m) = self.sketch.as_mut() else { return false };
        if let Some(d) = m.drawing.as_mut() {
            *d = Drawing::new(d.tool);
            return true;
        }
        let Some(pos) = pos else { return false };
        let Some(c) = self.sketch_mark_at(pos) else { return self.sketch.is_some() };
        let Some(m) = self.sketch.as_mut() else { return false };
        let Some(con) = m.data.as_ref().and_then(|d| d.constraints.iter().find(|x| x.index == c)) else { return true };
        if let Some(e) = &con.expression {
            let p = con.property.clone().unwrap_or_default();
            self.info = Some(format!("{p} follows the expression {e}: change it there (or in the property panel)"));
            return true;
        }
        if let (Some(property), Some(v), true) = (con.property.clone(), con.value, con.driving) {
            let what = con.kind.to_lowercase();
            m.editor = Some(Editor { kind: EditorKind::Existing { property, measured: v }, text: trim(v.abs()), at: pos, what, focus: true });
        }
        true
    }

    /// Esc: close the value box, drop the shape being drawn or the picks, then the tool, then
    /// the picked elements, then leave.
    pub(super) fn sketch_escape(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        if m.drag.take().is_some() {
            self.sketch_resync(); // back to where it was before the dry runs
            return;
        }
        if m.editor.take().is_some() {
            m.picks.clear();
            return;
        }
        if m.constrain.is_some() {
            if m.picks.is_empty() {
                m.constrain = None;
            } else {
                m.picks.clear();
            }
            return;
        }
        match &mut m.drawing {
            Some(d) if !d.clicks.is_empty() || d.chain.is_some() => *d = Drawing::new(d.tool),
            Some(_) => m.drawing = None,
            None if !m.selected.is_empty() || !m.selected_constraints.is_empty() => {
                m.selected.clear();
                m.selected_constraints.clear();
            }
            None => self.finish_sketch(),
        }
    }

    /// Delete the picked elements and constraints.
    pub(super) fn sketch_delete(&mut self) {
        let Some(m) = self.sketch.as_mut() else { return };
        if m.selected.is_empty() && m.selected_constraints.is_empty() {
            return;
        }
        let args = json!({ "obj": m.obj, "remove": { "geometry": m.selected, "constraints": m.selected_constraints }, "outlines": true });
        m.selected.clear();
        m.selected_constraints.clear();
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

    /// A press on a point or line with no tool drags it: dry runs while it moves, one move at
    /// the release. True while it does, so the camera stays put.
    pub(super) fn sketch_drag(&mut self, ui: &egui::Ui, resp: &egui::Response) -> bool {
        let Some(m) = &self.sketch else { return false };
        if m.drawing.is_some() || m.constrain.is_some() {
            return false;
        }
        if m.drag.is_none()
            && resp.drag_started_by(PointerButton::Primary)
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && let Some(pick) = self.pick_under(origin).filter(|p| *p != Pick::Point(Target::Origin))
            && let Some((from, _)) = self.on_plane(origin)
        {
            let live = self.tree.redo == 0;
            if let Some(m) = self.sketch.as_mut() {
                m.drag = Some(Drag { pick, from, sent: None, waiting: false, live });
            }
        }
        let Some(m) = &self.sketch else { return false };
        let Some(d) = &m.drag else { return false };
        let now = ui.input(|i| i.pointer.latest_pos()).and_then(|p| self.on_plane(p)).map(|(q, _)| q);
        let released = resp.drag_stopped() || !ui.input(|i| i.pointer.primary_down());
        let (pick, from, live, waiting, sent) = (d.pick, d.from, d.live, d.waiting, d.sent);
        let shape = match pick {
            Pick::Element(i) | Pick::Point(Target::Point(i, _)) => m.data.as_ref().and_then(|s| s.element(i)).map(|e| e.shape.clone()),
            Pick::Point(Target::Origin) => None,
        };
        let obj = m.obj.clone();
        let mv = |to: DVec2| -> Option<Value> {
            Some(match pick {
                Pick::Point(Target::Point(i, at)) => json!({ "geometry": i, "at": at.name(), "to": [to.x, to.y] }),
                // A line moves as a whole; a circle or arc takes the pointer on its rim.
                Pick::Element(i) if matches!(shape, Some(Shape::Line(..))) => json!({ "geometry": i, "to": [to.x - from.x, to.y - from.y], "relative": true }),
                Pick::Element(i) => json!({ "geometry": i, "to": [to.x, to.y] }),
                Pick::Point(Target::Origin) => return None,
            })
        };
        if released {
            if let Some(m) = self.sketch.as_mut() {
                m.drag = None;
            }
            if let Some(to) = now.filter(|t| t.distance(from) > 1e-9)
                && let Some(mv) = mv(to)
            {
                self.request("sketch_edit", json!({ "obj": obj, "move": mv, "outlines": true }), Pending::SketchEdit(0));
            } else {
                self.sketch_resync(); // back to where it was before the dry runs
            }
            return true;
        }
        if live
            && !waiting
            && let Some(to) = now.filter(|t| sent.is_none_or(|s| s.distance(*t) > 1e-9))
            && let Some(mv) = mv(to)
        {
            let args = json!({ "obj": obj, "move": mv, "dry_run": true, "preview": true, "outlines": true });
            self.request("sketch_edit", args, Pending::SketchPreview);
            if let Some(d) = self.sketch.as_mut().and_then(|m| m.drag.as_mut()) {
                d.sent = Some(to);
                d.waiting = true;
            }
        }
        true
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
        let picked = m.selected.len() + m.selected_constraints.len();
        Some(match (&m.drawing, m.constrain) {
            (Some(dr), _) => format!("{label} · {}: {} · {state}", dr.tool.label(), dr.prompt()),
            (None, Some(Constrain::Dimension)) if m.editor.is_some() => format!("{label} · Dimension: type the value, Enter (Esc cancels) · {state}"),
            (None, Some(c)) => format!("{label} · {}: {} picked · {state}", c.name(), m.picks.len()),
            (None, None) if picked > 0 => format!("{label} · {picked} picked: Del deletes, X makes construction · {state}"),
            (None, None) => format!("{label} · pick a tool, drag points and lines, double-click a dimension to change it · {state}"),
        })
    }

    /// Dimensions and constraint marks as the view shows them: (constraint, place on screen,
    /// text, a dimension). Marks of one place stand side by side.
    fn sketch_marks(&self) -> Vec<(usize, Pos2, String, bool)> {
        let Some((vp, rect)) = self.gizmos_last_view() else { return Vec::new() };
        let Some(d) = self.sketch.as_ref().and_then(|m| m.data.as_ref()) else { return Vec::new() };
        let project = picking::projector(vp, rect);
        let at = |q: DVec2| project(d.plane.world(q));
        let mut out = Vec::new();
        let mut taken: Vec<(Pos2, usize)> = Vec::new();
        for c in &d.constraints {
            if let Some((p, text)) = sketcher::dimension_label(d, c)
                && let Some(s) = at(p)
            {
                out.push((c.index, s + egui::vec2(0.0, -14.0), text, true));
            }
            for (p, mark) in sketcher::constraint_marks(d, c) {
                let Some(s) = at(p) else { continue };
                let n = match taken.iter_mut().find(|(q, _)| (*q - s).length() < 4.0) {
                    Some((_, n)) => {
                        *n += 1;
                        *n
                    }
                    None => {
                        taken.push((s, 0));
                        0
                    }
                };
                out.push((c.index, s + egui::vec2(10.0 + 16.0 * n as f32, 10.0), mark.to_string(), false));
            }
        }
        out
    }

    /// The dimension or constraint mark under the pointer.
    fn sketch_mark_at(&self, pos: Pos2) -> Option<usize> {
        self.sketch_marks()
            .into_iter()
            .filter(|(_, at, text, _)| mark_rect(*at, text).expand(2.0).contains(pos))
            .map(|(c, ..)| c)
            .next()
    }

    /// The sketch over the view: profiles shaded, elements, points, the origin, dimensions and
    /// marks; while drawing, the shape the next click makes and what the pointer snaps to.
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
        let picked_by_tool = |i: usize| m.picks.contains(&Pick::Element(i));
        for e in &d.elements {
            let picked = m.selected.contains(&e.index) || picked_by_tool(e.index);
            let color = if picked {
                PICKED
            } else if m.hover_pick == Some(Pick::Element(e.index)) {
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
        // Points picked for a constraint, or under the pointer for one.
        for p in m.picks.iter().chain(m.hover_pick.iter()) {
            if let Pick::Point(t) = p
                && let Some(s) = d.point(*t).and_then(at)
            {
                let color = if m.picks.contains(p) { PICKED } else { HOVERED };
                painter.circle_stroke(s, 5.0, Stroke::new(2.0, color));
            }
        }
        for (c, pos, text, dimension) in self.sketch_marks() {
            let r = mark_rect(pos, &text);
            let lit = m.selected_constraints.contains(&c);
            let (fill, ink) = match (lit, dimension) {
                (true, _) => (PICKED, Color32::WHITE),
                (false, true) => (Color32::from_rgba_unmultiplied(255, 255, 255, 220), LABEL),
                (false, false) => (Color32::from_rgba_unmultiplied(235, 238, 244, 220), Color32::from_rgb(70, 80, 100)),
            };
            painter.rect_filled(r, 3.0, fill);
            painter.text(r.center(), egui::Align2::CENTER_CENTER, &text, mark_font(dimension), ink);
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
            Some(Shape::Circle(_, r)) => format!("Ø {} mm", trim(2.0 * r)),
            Some(Shape::Line(a, _)) if matches!(dr.tool, Tool::Rectangle | Tool::CenterRectangle) => {
                let (lo, hi) = (a.min(snap.at), a.max(snap.at));
                let w = if dr.tool == Tool::CenterRectangle { 2.0 * (snap.at - dr.last().unwrap_or(*a)).abs() } else { hi - lo };
                format!("{} × {} mm", trim(w.x), trim(w.y))
            }
            _ => format!("{}, {}", trim(snap.at.x), trim(snap.at.y)),
        };
        match (snap.align, snap.target) {
            (Some(Align::Horizontal), _) => label.push_str("  — horizontal"),
            (Some(Align::Vertical), _) => label.push_str("  | vertical"),
            (None, Some(Target::Origin)) => label.push_str("  on the origin"),
            (None, Some(Target::Point(..))) => label.push_str("  on a point"),
            _ => {}
        }
        painter.text(cur + egui::vec2(12.0, -12.0), egui::Align2::LEFT_BOTTOM, label, egui::FontId::proportional(12.0), PREVIEW);
    }

    /// The value box of a dimension, by where it was asked for: Enter applies, Esc cancels.
    pub(super) fn sketch_editor(&mut self, ctx: &egui::Context) {
        let Some(m) = self.sketch.as_mut() else { return };
        let Some(ed) = m.editor.as_mut() else { return };
        let mut done: Option<bool> = None;
        egui::Area::new(egui::Id::unique("sketch-dimension")).fixed_pos(ed.at + egui::vec2(14.0, -34.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&ed.what).small().weak());
                    let unit = if ed.what == "angle" { "°" } else { "mm" };
                    let r = ui.add(egui::TextEdit::singleline(&mut ed.text).desired_width(70.0));
                    ui.label(unit);
                    if ed.focus {
                        r.request_focus();
                        ed.focus = false;
                    }
                    if r.lost_focus() {
                        let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
                        if enter || esc {
                            done = Some(enter);
                        } else {
                            // A click in the view (a second pick for the dimension) takes the
                            // focus; the box stays and takes it back.
                            ed.focus = true;
                        }
                    }
                });
            });
        });
        match done {
            Some(true) => self.sketch_apply_value(None),
            Some(false) => {
                if let Some(m) = self.sketch.as_mut() {
                    m.editor = None;
                    m.picks.clear();
                }
            }
            None => {}
        }
    }

    /// The value in the dimension's box (or `typed`, as the self-test types it) goes to the
    /// sketch: a new dimension, or a new value for one it has.
    pub(super) fn sketch_apply_value(&mut self, typed: Option<&str>) {
        let Some(m) = self.sketch.as_mut() else { return };
        let Some(mut ed) = m.editor.take() else { return };
        let text = typed.unwrap_or(&ed.text).to_string();
        let Ok(typed) = text.trim().replace(',', ".").parse::<f64>() else {
            self.error = Some(format!("{text}: not a number"));
            ed.focus = true;
            m.editor = Some(ed);
            return;
        };
        m.picks.clear();
        let obj = m.obj.clone();
        // An angle keeps the way round it measures; sizes are what was typed.
        let signed = |measured: f64| if ed.what == "angle" && measured < 0.0 { -typed.abs() } else { typed };
        match ed.kind {
            EditorKind::New { mut constraint, measured } => {
                constraint["value"] = json!(signed(measured));
                self.request("sketch_edit", json!({ "obj": obj, "constraints": [constraint], "outlines": true }), Pending::SketchEdit(0));
            }
            EditorKind::Existing { property, measured } => {
                let v = signed(measured);
                self.set_prop(&obj, &property, json!(v));
            }
        }
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
            ui.label(format!("{} ({} dimensions)", d.constraints.len(), d.constraints.iter().filter(|c| c.value.is_some()).count()));
            ui.end_row();
            ui.label("Solver");
            ui.label(constrained);
            ui.end_row();
            ui.label("Profiles");
            ui.label(d.profiles.len().to_string()).on_hover_text("Closed regions an extrude takes, shaded orange");
            ui.end_row();
        });
        let picked = m.selected.len();
        let anything = picked + m.selected_constraints.len();
        ui.separator();
        let mut act = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(picked > 0, egui::Button::new("Construction")).on_hover_text("Picked elements become guides, or back (X)").clicked() {
                act = Some(0);
            }
            if ui.add_enabled(anything > 0, egui::Button::new("Delete")).on_hover_text("The picked elements and constraints (Del)").clicked() {
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

fn mark_font(dimension: bool) -> egui::FontId {
    egui::FontId::proportional(if dimension { 12.0 } else { 10.5 })
}

/// The box a dimension or mark is drawn in, about its place.
fn mark_rect(at: Pos2, text: &str) -> Rect {
    let w = 7.0 * text.chars().count() as f32 + 6.0;
    Rect::from_center_size(at, egui::vec2(w.max(14.0), 15.0))
}

/// A number without trailing zeros, to a hundredth: 12.5, 3, 0.05.
fn trim(v: f64) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}
