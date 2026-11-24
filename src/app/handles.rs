//! Edges, corners and drag handles in the view: hovering and picking B-rep edges and vertices,
//! the arrow that pushes or pulls a face (with a preview of what it adds or cuts) and the one
//! that sets a fillet's radius, as in the big CAD tools, and the marking menu: a wheel of the
//! commands for what is picked, held open with Alt. The geometry comes from the worker on
//! demand (`edges`, `face`) and is kept until the model changes.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, PointerButton, Pos2, Rect};
use glam::{Mat4, Vec3};
use serde_json::{Value, json};

use super::{App, Pending, Selection};
use crate::modelling::{self, Op};
use crate::picking::{self, EdgeInfo, FaceInfo, VertexInfo};

/// How near the cursor an edge is picked, in points; a corner, which wins over its edges.
const EDGE_REACH: f32 = 7.0;
const VERTEX_REACH: f32 = 8.0;
const HOVER: Color32 = Color32::from_rgb(120, 185, 255);
/// How long a handle's arrow is on screen, in points, and how near it a press grabs it.
const ARROW: f32 = 80.0;
const GRAB: f32 = 10.0;
/// At most this often the view looks under the cursor for an edge to light.
const HOVER_EVERY: Duration = Duration::from_millis(50);
/// A look that took longer than this turns hovering off: the scene is too heavy for it.
const HOVER_BUDGET: Duration = Duration::from_millis(15);
const SLOW_LOOKS: u32 = 3;

#[derive(Default)]
pub(super) struct Gizmos {
    edges: HashMap<String, Vec<EdgeInfo>>,
    asked_edges: HashSet<String>,
    face: Option<FaceInfo>,
    asked_face: Option<(String, String)>,
    /// The object under the cursor and the edge lit there.
    pub(super) hover: Option<(String, Option<u32>)>,
    hovered_at: Option<(Pos2, Instant)>,
    /// Looks in a row that took too long; at `SLOW_LOOKS` hovering stops until the model changes.
    slow: u32,
    /// Bumped when the model changes; replies to requests sent before are dropped.
    generation: u64,
    drag: Option<Drag>,
    /// The last frame's view, for clicks and drags in the next one.
    view: Option<(Mat4, Rect)>,
    /// The corners of the objects whose edges are here, and the one lit under the cursor.
    vertices: HashMap<String, Vec<VertexInfo>>,
    hover_vertex: Option<u32>,
    /// The marking menu while Alt holds it open, Alt as the last frame saw it, and the size
    /// typed by the pointer for the command the menu started.
    wheel: Option<Wheel>,
    alt: bool,
    size: Option<SizeBox>,
}

/// The marking menu on screen.
struct Wheel {
    center: Pos2,
    choices: &'static [Op],
    /// What it acts on, as it reads: "Pocket.Edge3".
    what: String,
}

/// The number box by the pointer for the command the marking menu started.
struct SizeBox {
    at: Pos2,
    op: Op,
    text: String,
    /// Take the keyboard (and select the text) in the next frame.
    focus: bool,
}

impl Gizmos {
    /// The model changed: face and edge numbers may mean something else now.
    pub(super) fn clear(&mut self) {
        self.edges.clear();
        self.vertices.clear();
        self.hover_vertex = None;
        self.asked_edges.clear();
        self.face = None;
        self.asked_face = None;
        self.hover = None;
        self.slow = 0;
        self.generation += 1;
    }
}

struct Drag {
    /// A point on the handle's line and its direction, as at the press.
    base: Vec3,
    dir: Vec3,
    start: f64,
    step: f64,
    /// Fillet and chamfer sizes stay above zero.
    positive: bool,
}

/// A handle shown: where its arrow starts, which way it points, the value it sets.
struct Handle {
    base: Vec3,
    dir: Vec3,
    value: f64,
    /// A selected face with no command open: dragging starts an extrude.
    starts_extrude: bool,
    positive: bool,
}

impl App {
    pub(super) fn on_edges(&mut self, generation: u64, v: &Value) {
        if generation != self.gizmos.generation {
            return; // asked about the model before it changed
        }
        if let Some(obj) = v["obj"].as_str() {
            self.gizmos.edges.insert(obj.to_string(), picking::parse_edges(v));
            self.gizmos.vertices.insert(obj.to_string(), picking::parse_vertices(v));
        }
    }

    pub(super) fn on_face(&mut self, generation: u64, v: &Value) {
        if generation == self.gizmos.generation {
            self.gizmos.face = picking::parse_face(v);
        }
    }

    /// A request for geometry failed (a shape without faces, say): stop asking for it.
    pub(super) fn on_geometry_error(&mut self, kind: &Pending) {
        if let Pending::Edges(obj, g) = kind
            && *g == self.gizmos.generation
        {
            self.gizmos.edges.insert(obj.clone(), Vec::new());
        }
    }

    /// The face whose handle or preview is wanted: the extrude's, else a selected face.
    fn wanted_face(&self) -> Option<(String, String)> {
        match &self.command {
            Some(c) => c.extrude_face().map(|(o, s)| (o.to_string(), s.to_string())),
            None => {
                let s = self.selection.as_ref()?;
                s.sub.as_ref().filter(|x| x.starts_with("Face")).map(|x| (s.obj.clone(), x.clone()))
            }
        }
    }

    /// Ask the worker for what the handles and edge picking need and do not have yet.
    fn ask_geometry(&mut self) {
        if let Some(want) = self.wanted_face()
            && self.gizmos.face.as_ref().is_none_or(|f| (f.obj.as_str(), f.sub.as_str()) != (want.0.as_str(), want.1.as_str()))
            && self.gizmos.asked_face.as_ref() != Some(&want)
        {
            self.request("face", json!({ "obj": want.0, "sub": want.1 }), Pending::Face(self.gizmos.generation));
            self.gizmos.asked_face = Some(want);
        }
        let mut objs: Vec<String> = Vec::new();
        objs.extend(self.gizmos.hover.as_ref().map(|h| h.0.clone()));
        let edge_or_corner = |x: &str| x.starts_with("Edge") || x.starts_with("Vertex");
        objs.extend(self.selection.as_ref().filter(|s| s.sub.as_deref().is_some_and(edge_or_corner)).map(|s| s.obj.clone()));
        objs.extend(self.command.as_ref().and_then(|c| c.picked_edges()).map(|(o, _)| o.to_string()));
        for obj in objs {
            self.ask_edges(&obj);
        }
    }

    pub(super) fn ask_edges(&mut self, obj: &str) {
        if !self.gizmos.edges.contains_key(obj) && self.gizmos.asked_edges.insert(obj.to_string()) {
            self.request("edges", json!({ "obj": obj }), Pending::Edges(obj.to_string(), self.gizmos.generation));
        }
    }

    fn handle(&self) -> Option<Handle> {
        let planar_face = |obj: &str, sub: &str| self.gizmos.face.as_ref().filter(|f| f.obj == obj && f.sub == sub && f.planar);
        match &self.command {
            Some(c) if c.op == Op::Extrude => {
                let (obj, sub) = c.extrude_face()?;
                let f = planar_face(obj, sub)?;
                let d = c.amount();
                Some(Handle { base: f.center + f.normal * d as f32, dir: f.normal, value: d, starts_extrude: false, positive: false })
            }
            Some(c) if matches!(c.op, Op::Fillet | Op::Chamfer) => {
                let (obj, picked) = c.picked_edges()?;
                let e = self.gizmos.edges.get(obj)?.iter().find(|e| picked.contains(&e.index) && e.out.length() > 0.5)?;
                // Stands on the edge: the radius is in its label, the drag measures from there.
                Some(Handle { base: e.mid, dir: e.out, value: c.amount(), starts_extrude: false, positive: true })
            }
            Some(_) => None,
            None => {
                let (obj, sub) = self.wanted_face()?;
                let f = planar_face(&obj, &sub)?;
                Some(Handle { base: f.center, dir: f.normal, value: 0.0, starts_extrude: true, positive: false })
            }
        }
    }

    /// The view as the last frame drew it: its projection and where it is on screen.
    pub(super) fn gizmos_last_view(&self) -> Option<(Mat4, Rect)> {
        self.gizmos.view
    }

    fn project(&self) -> Option<impl Fn(Vec3) -> Option<Pos2> + use<>> {
        self.gizmos.view.map(|(m, r)| picking::projector(m, r))
    }

    /// A press on a handle's arrow drags it instead of the camera. True while it does.
    pub(super) fn drag_handle(&mut self, ui: &egui::Ui, resp: &egui::Response) -> bool {
        let Some(project) = self.project() else { return false };
        if self.command.is_none() {
            self.gizmos.drag = None; // Esc closed the command mid-drag
        }
        if resp.drag_started_by(PointerButton::Primary)
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && let Some(h) = self.handle()
            && let Some((tail, tip)) = picking::arrow(h.base, h.dir, &project, ARROW)
            && picking::segment_distance(origin, tail, tip) <= GRAB
        {
            if h.starts_extrude {
                // Push-pull on the part: an extrude of the selected face, its distance dragged.
                self.start_command(Op::Extrude);
                if let Some(c) = self.command.as_mut() {
                    *c.amount_mut() = 0.0;
                }
            }
            let px = match (project(h.base + h.dir), project(h.base)) {
                (Some(a), Some(b)) => (a - b).length(),
                _ => 1.0,
            };
            self.gizmos.drag = Some(Drag { base: h.base, dir: h.dir, start: h.value, step: picking::nice_step(px), positive: h.positive });
        }
        let Some(d) = &self.gizmos.drag else { return false };
        if let (Some(origin), Some(now)) = ui.input(|i| (i.pointer.press_origin(), i.pointer.latest_pos()))
            && let Some(moved) = picking::drag_amount(d.base, d.dir, &project, now - origin)
        {
            let mut v = ((d.start + moved as f64) / d.step).round() * d.step;
            if d.positive {
                v = v.max(d.step);
            }
            if let Some(c) = self.command.as_mut() {
                *c.amount_mut() = v;
            }
        }
        if resp.drag_stopped() || !ui.input(|i| i.pointer.primary_down()) {
            self.gizmos.drag = None;
        }
        true
    }

    /// Look under the resting cursor for an edge to light, now and then.
    pub(super) fn hover(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: Rect) {
        let pos = resp.hover_pos().filter(|_| !resp.dragged() && self.gizmos.slow < SLOW_LOOKS);
        let Some(pos) = pos else {
            self.gizmos.hover = None;
            self.gizmos.hover_vertex = None;
            return;
        };
        if let Some((at, when)) = self.gizmos.hovered_at {
            if at == pos {
                return;
            }
            if when.elapsed() < HOVER_EVERY {
                ui.ctx().request_repaint_after(HOVER_EVERY);
                return;
            }
        }
        self.gizmos.hovered_at = Some((pos, Instant::now()));
        let ppp = ui.ctx().pixels_per_point();
        let started = Instant::now();
        let hit = self.face_at(((pos.x - rect.min.x) * ppp) as u32, ((pos.y - rect.min.y) * ppp) as u32);
        self.gizmos.slow = if started.elapsed() > HOVER_BUDGET { self.gizmos.slow + 1 } else { 0 };
        let edges_ok = self.command.as_ref().is_none_or(|c| c.wants_edges());
        let mut corner = None;
        self.gizmos.hover = hit.map(|(sel, _)| {
            let hits = std::slice::from_ref(&sel);
            corner = if self.command.is_none() { self.vertex_near(pos, hits) } else { None };
            let edge = if edges_ok && corner.is_none() { self.edge_near(pos, hits) } else { None };
            (sel.obj, edge)
        });
        self.gizmos.hover_vertex = corner;
    }

    /// The faces `hits` (picked under and around the cursor) name, 0-based, of the first one's
    /// object: the edges and corners the view can see there are theirs.
    fn hit_faces(hits: &[Selection]) -> Option<(&String, Vec<u32>)> {
        let face_of = |s: &Selection| s.sub.as_deref()?.strip_prefix("Face")?.parse::<u32>().ok()?.checked_sub(1);
        let obj = &hits.first()?.obj;
        Some((obj, hits.iter().filter(|s| &s.obj == obj).filter_map(face_of).collect()))
    }

    /// The edge within reach of `pos` that bounds one of the faces `hits`, so that edges hidden
    /// behind the part are not picked.
    fn edge_near(&self, pos: Pos2, hits: &[Selection]) -> Option<u32> {
        let project = self.project()?;
        let (obj, faces) = Self::hit_faces(hits)?;
        let edges = self.gizmos.edges.get(obj)?;
        picking::nearest_edge(edges, |e| e.faces.iter().any(|f| faces.contains(f)), &project, pos, EDGE_REACH).map(|e| e.index)
    }

    /// The corner within reach of `pos` at the end of an edge of the faces `hits`.
    fn vertex_near(&self, pos: Pos2, hits: &[Selection]) -> Option<u32> {
        let project = self.project()?;
        let (obj, faces) = Self::hit_faces(hits)?;
        let seen: HashSet<u32> = self.gizmos.edges.get(obj)?.iter().filter(|e| e.faces.iter().any(|f| faces.contains(f))).map(|e| e.index - 1).collect();
        let vertices = self.gizmos.vertices.get(obj)?;
        picking::nearest_vertex(vertices, |v| v.edges.iter().any(|e| seen.contains(e)), &project, pos, VERTEX_REACH).map(|v| v.index)
    }

    /// A click in the view at `pos`, pixel (`px`, `py`): a corner near the cursor (with no
    /// command open), else an edge near it, else the face under it, else a construction plane.
    /// A command asking for a plane takes a plane in front of a face. While the marking menu
    /// is open the click is its own (see `marking_menu`).
    pub(super) fn click_view(&mut self, pos: Pos2, px: u32, py: u32, ppp: f32) {
        if self.gizmos.wheel.is_some() {
            return;
        }
        let plane = self.plane_at(pos);
        if plane.is_some() && self.command.as_ref().is_some_and(|c| c.wants_plane()) {
            self.picked(plane);
            return;
        }
        // The face under the cursor, then those a few points around it: an edge on the
        // outline of the part has the part only on one side.
        let center = self.face_at(px, py);
        let edges_ok = self.command.as_ref().is_none_or(|c| c.wants_edges());
        // Picking around costs four more looks: only when some known edge is within reach.
        let near_edge = edges_ok
            && self.project().is_some_and(|project| {
                let objs: Vec<&String> = match &center {
                    Some((c, _)) => vec![&c.obj],
                    None => self.gizmos.edges.keys().collect(),
                };
                objs.iter().any(|o| {
                    self.gizmos.edges.get(*o).is_some_and(|es| picking::nearest_edge(es, |_| true, &project, pos, EDGE_REACH).is_some())
                })
            });
        let r = (5.0 * ppp) as i64;
        let mut around: Vec<Selection> = Vec::new();
        let offsets = [(-r, 0), (r, 0), (0, -r), (0, r)];
        for &(dx, dy) in if near_edge { &offsets[..] } else { &[] } {
            let (x, y) = (px as i64 + dx, py as i64 + dy);
            if x >= 0 && y >= 0 && let Some((s, _)) = self.face_at(x as u32, y as u32) {
                around.push(s);
            }
        }
        let sels: Vec<Selection> = match &center {
            // The object under the cursor first: its edges win over a neighbour's.
            Some((c, _)) => std::iter::once(c.clone()).chain(around.into_iter().filter(|s| s.obj == c.obj)).collect(),
            None => around,
        };
        let corner = if self.command.is_none() { self.vertex_near(pos, &sels) } else { None };
        let edge = if edges_ok && corner.is_none() { self.edge_near(pos, &sels) } else { None };
        for s in &sels {
            self.ask_edges(&s.obj);
        }
        let element = corner.map(|v| format!("Vertex{v}")).or(edge.map(|e| format!("Edge{e}")));
        let hit = match (element, sels.first(), center.clone()) {
            (Some(sub), Some(s), _) => {
                let label = self.tree.nodes.get(&s.obj).map_or(s.obj.as_str(), |n| n.label.as_str()).to_string();
                self.info = Some(format!("picked {label}.{sub}"));
                Some(Selection { obj: s.obj.clone(), sub: Some(sub), face_id: 0 })
            }
            (None, _, Some((sel, text))) => {
                self.info = Some(format!("picked {text}"));
                Some(sel)
            }
            _ => plane,
        };
        self.picked(hit);
    }

    /// Picked and hovered edges, the extrude's preview and the handle, over the view.
    pub(super) fn paint_handles(&mut self, painter: &egui::Painter, rect: Rect, view_proj: Mat4) {
        self.gizmos.view = Some((view_proj, rect));
        self.ask_geometry();
        let project = picking::projector(view_proj, rect);
        let edge = |obj: &str, i: u32| self.gizmos.edges.get(obj)?.iter().find(|e| e.index == i);
        let mut picked: Vec<(String, u32)> = Vec::new();
        if let Some(s) = &self.selection
            && let Some(i) = s.sub.as_deref().and_then(|x| x.strip_prefix("Edge")?.parse().ok())
        {
            picked.push((s.obj.clone(), i));
        }
        if let Some((obj, list)) = self.command.as_ref().and_then(|c| c.picked_edges()) {
            picked.extend(list.into_iter().map(|i| (obj.to_string(), i)));
        }
        if let Some((obj, Some(i))) = &self.gizmos.hover
            && !picked.iter().any(|(o, j)| o == obj && j == i)
            && let Some(e) = edge(obj, *i)
        {
            picking::paint_edge(painter, e, &project, HOVER);
        }
        for (obj, i) in &picked {
            if let Some(e) = edge(obj, *i) {
                picking::paint_edge(painter, e, &project, picking::HANDLE);
            }
        }
        let corner = |obj: &str, i: u32| self.gizmos.vertices.get(obj)?.iter().find(|v| v.index == i);
        let picked_corner = self.selection.as_ref().and_then(|s| Some((s.obj.as_str(), s.sub.as_deref()?.strip_prefix("Vertex")?.parse::<u32>().ok()?)));
        if let (Some((obj, _)), Some(i)) = (&self.gizmos.hover, self.gizmos.hover_vertex)
            && picked_corner != Some((obj.as_str(), i))
            && let Some(v) = corner(obj, i)
        {
            picking::paint_vertex(painter, v, &project, HOVER);
        }
        if let Some(v) = picked_corner.and_then(|(obj, i)| corner(obj, i)) {
            picking::paint_vertex(painter, v, &project, picking::HANDLE);
        }
        if let Some(c) = &self.command
            && let Some((obj, sub)) = c.extrude_face()
            && let Some(f) = self.gizmos.face.as_ref().filter(|f| f.obj == obj && f.sub == sub)
            && c.amount().abs() > 1e-9
        {
            picking::paint_extrusion(painter, f, c.amount() as f32, &project);
        }
        if let Some(h) = self.handle()
            && let Some((tail, tip)) = picking::arrow(h.base, h.dir, &project, ARROW)
        {
            let near = painter.ctx().pointer_hover_pos().is_some_and(|p| picking::segment_distance(p, tail, tip) <= GRAB);
            let text = if h.starts_extrude { "drag to extrude".to_string() } else { format!("{} mm", trim(h.value)) };
            picking::paint_arrow(painter, tail, tip, near || self.gizmos.drag.is_some(), &text);
        }
        self.marking_menu(painter.ctx(), rect);
        self.size_box(painter.ctx());
    }

    /// The marking menu. Alt pressed alone over the view, with something picked and no command
    /// or sketch open, opens a wheel around the pointer with the commands for what is picked
    /// (`modelling::marking_choices`); moving the pointer towards one lights it, and letting
    /// Alt go or clicking starts it, as the ribbon would, with a box by the pointer for its
    /// size. Letting go in the middle, Esc or leaving the window closes it with nothing done.
    fn marking_menu(&mut self, ctx: &egui::Context, view: Rect) {
        let (alt, alone, pointer, clicked, esc, focused) = ctx.input(|i| {
            let m = i.modifiers;
            (m.alt, m.alt && !m.ctrl && !m.shift && !m.command, i.pointer.latest_pos(), i.pointer.primary_clicked(), i.key_pressed(egui::Key::Escape), i.focused)
        });
        let pressed = alone && !self.gizmos.alt;
        self.gizmos.alt = alt;
        if pressed
            && self.gizmos.wheel.is_none()
            && self.command.is_none()
            && self.sketch.is_none()
            && !ctx.egui_wants_keyboard_input()
            // Over the view itself, not over a panel floating on it.
            && let Some(at) = pointer.filter(|p| view.contains(*p) && ctx.layer_id_at(*p).is_none_or(|l| l.order == egui::Order::Background))
            && let Some(sel) = &self.selection
        {
            let label = self.tree.nodes.get(&sel.obj).map_or(sel.obj.as_str(), |n| n.label.as_str());
            let what = match &sel.sub {
                Some(sub) => format!("{label}.{sub}"),
                None => label.to_string(),
            };
            self.gizmos.wheel = Some(Wheel { center: at, choices: modelling::marking_choices(sel.sub.as_deref()), what });
            self.gizmos.size = None;
        }
        let Some(w) = &self.gizmos.wheel else { return };
        let hot = pointer.and_then(|p| modelling::wheel_choice(p - w.center, w.choices.len()));
        if !focused || esc || self.command.is_some() {
            self.gizmos.wheel = None;
        } else if !alt || clicked {
            let center = w.center;
            let chosen = hot.map(|k| w.choices[k]);
            self.gizmos.wheel = None;
            if let Some(op) = chosen {
                self.marking_pick(op, center);
            }
        } else {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("marking-menu")));
            modelling::paint_wheel(&painter, w.center, w.choices, hot, pointer, &w.what);
        }
    }

    /// A choice of the marking menu: its command starts on the selection, as from the ribbon,
    /// and the box for its size opens where the wheel was.
    fn marking_pick(&mut self, op: Op, at: Pos2) {
        self.start_command(op);
        if let Some(c) = self.command.as_ref().filter(|c| c.op == op) {
            self.gizmos.size = Some(SizeBox { at, op, text: trim(c.amount()), focus: true });
        }
    }

    /// The size box of the command the marking menu started: Enter makes the command's step
    /// with the size typed, as its dialog's OK does; Esc closes the command. Gone once the
    /// command is.
    fn size_box(&mut self, ctx: &egui::Context) {
        let (Some(b), Some(c)) = (self.gizmos.size.as_mut(), self.command.as_mut()) else {
            self.gizmos.size = None;
            return;
        };
        if c.op != b.op {
            self.gizmos.size = None;
            return;
        }
        let mut done: Option<bool> = None;
        let id = egui::Id::unique("marking-size");
        egui::Area::new(id.with("box")).fixed_pos(b.at + egui::vec2(14.0, 10.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{} {}", b.op.title(), b.op.amount_name().to_lowercase())).small().weak());
                    let r = ui.add(egui::TextEdit::singleline(&mut b.text).id(id).desired_width(70.0));
                    ui.label("mm");
                    if b.focus {
                        r.request_focus();
                        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all_str(&b.text)));
                            state.store(ui.ctx(), id);
                        }
                        b.focus = false;
                    }
                    if r.changed() {
                        // The arrow on the edge follows what is typed.
                        if let Some(v) = size_of(&b.text) {
                            *c.amount_mut() = v;
                        }
                    } else if !r.has_focus() {
                        b.text = trim(c.amount()); // the arrow was dragged
                    }
                    if r.lost_focus() {
                        let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
                        if enter || esc {
                            done = Some(enter);
                        }
                    }
                });
            });
        });
        match done {
            Some(true) => self.marking_apply(),
            Some(false) => {
                self.command = None;
                self.gizmos.size = None;
            }
            None => {}
        }
    }

    /// The size in the box goes to the command, which makes its step.
    fn marking_apply(&mut self) {
        let Some(mut b) = self.gizmos.size.take() else { return };
        let Some(v) = size_of(&b.text) else {
            self.error = Some(format!("{}: not a size above zero", b.text));
            b.focus = true;
            self.gizmos.size = Some(b);
            return;
        };
        let Some(c) = self.command.as_mut().filter(|c| c.op == b.op) else { return };
        *c.amount_mut() = v;
        // One step per Enter, as the dialog's OK: a second one before the reply would make it twice.
        if !self.pending.values().any(|p| matches!(p, Pending::Create)) {
            self.run_command();
        }
        // It goes with the command when the step is made; if FreeCAD refuses the size, it stays
        // for another.
        self.gizmos.size = Some(b);
    }

    // ---- for the self-test -----------------------------------------------------------------

    /// Click, as the mouse would, on the middle of an edge (`corner` false) or on a corner of
    /// `obj` until one is picked: what was picked and where, or None when no click picked one.
    pub(super) fn selftest_click(&mut self, obj: &str, corner: bool, ppp: f32) -> Option<(String, Pos2)> {
        let (project, rect) = (self.project()?, self.gizmos.view?.1);
        let points: Vec<(u32, Pos2)> = if corner {
            self.gizmos.vertices.get(obj)?.iter().filter_map(|v| Some((v.index, project(v.at)?))).collect()
        } else {
            self.gizmos.edges.get(obj)?.iter().filter_map(|e| Some((e.index, project(e.mid)?))).collect()
        };
        let want = if corner { "Vertex" } else { "Edge" };
        for (i, at) in points.into_iter().filter(|(_, p)| rect.shrink(30.0).contains(*p)) {
            self.click_view(at, ((at.x - rect.min.x) * ppp) as u32, ((at.y - rect.min.y) * ppp) as u32, ppp);
            let sub = self.selection.as_ref().and_then(|s| s.sub.clone());
            if sub.as_deref() == Some(&format!("{want}{i}")) {
                return Some((sub?, at));
            }
        }
        None
    }

    /// The marking menu and its size box as they stand.
    pub(super) fn marking_state(&self) -> Value {
        let pointer = self.ctx.input(|i| i.pointer.latest_pos());
        let hot = self.gizmos.wheel.as_ref().and_then(|w| Some(w.choices[modelling::wheel_choice(pointer? - w.center, w.choices.len())?].label()));
        json!({
            "hot": hot,
            "wheel": self.gizmos.wheel.as_ref().map(|w| json!({
                "at": [w.center.x, w.center.y],
                "choices": w.choices.iter().map(|o| o.label()).collect::<Vec<_>>(),
                "what": w.what,
            })),
            "size_box": self.gizmos.size.as_ref().map(|b| json!({ "op": b.op.label(), "text": b.text })),
        })
    }
}

/// A size as typed: a number above zero, with a comma or a point, "mm" after it or not.
fn size_of(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches("mm").trim().replace(',', ".");
    t.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0)
}

/// A number without trailing zeros: 12.5, 3, 0.05.
fn trim(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn values_read_without_trailing_zeros() {
        assert_eq!(super::trim(12.5), "12.5");
        assert_eq!(super::trim(3.0), "3");
        assert_eq!(super::trim(0.05), "0.05");
        assert_eq!(super::trim(-0.00001), "0");
    }

    #[test]
    fn sizes_read_as_typed() {
        assert_eq!(super::size_of(" 2,5 "), Some(2.5));
        assert_eq!(super::size_of("3mm"), Some(3.0));
        assert_eq!(super::size_of("0"), None);
        assert_eq!(super::size_of("-1"), None);
        assert_eq!(super::size_of("abc"), None);
    }
}
