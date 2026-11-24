//! Edges and drag handles in the view: hovering and picking B-rep edges, the arrow that pushes
//! or pulls a face (with a preview of what it adds or cuts) and the one that sets a fillet's
//! radius, as in the big CAD tools. The geometry comes from the worker on demand (`edges`,
//! `face`) and is kept until the model changes.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, PointerButton, Pos2, Rect};
use glam::{Mat4, Vec3};
use serde_json::{Value, json};

use super::{App, Pending, Selection};
use crate::modelling::Op;
use crate::picking::{self, EdgeInfo, FaceInfo};

/// How near the cursor an edge is picked, in points.
const EDGE_REACH: f32 = 7.0;
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
}

impl Gizmos {
    /// The model changed: face and edge numbers may mean something else now.
    pub(super) fn clear(&mut self) {
        self.edges.clear();
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
        objs.extend(self.selection.as_ref().filter(|s| s.sub.as_deref().is_some_and(|x| x.starts_with("Edge"))).map(|s| s.obj.clone()));
        objs.extend(self.command.as_ref().and_then(|c| c.picked_edges()).map(|(o, _)| o.to_string()));
        for obj in objs {
            self.ask_edges(&obj);
        }
    }

    fn ask_edges(&mut self, obj: &str) {
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
        self.gizmos.hover = hit.map(|(sel, _)| {
            let edge = if edges_ok { self.edge_near(pos, std::slice::from_ref(&sel)) } else { None };
            (sel.obj, edge)
        });
    }

    /// The edge within reach of `pos` that bounds one of the faces `hits` (picked around it),
    /// so that edges hidden behind the part are not picked.
    fn edge_near(&self, pos: Pos2, hits: &[Selection]) -> Option<u32> {
        let project = self.project()?;
        let face_of = |s: &Selection| s.sub.as_deref()?.strip_prefix("Face")?.parse::<u32>().ok()?.checked_sub(1);
        let obj = &hits.first()?.obj;
        let faces: Vec<u32> = hits.iter().filter(|s| &s.obj == obj).filter_map(face_of).collect();
        let edges = self.gizmos.edges.get(obj)?;
        picking::nearest_edge(edges, |e| e.faces.iter().any(|f| faces.contains(f)), &project, pos, EDGE_REACH).map(|e| e.index)
    }

    /// A click in the view at `pos`, pixel (`px`, `py`): an edge near the cursor, else the
    /// face under it, else a construction plane. A command asking for a plane takes a plane in
    /// front of a face.
    pub(super) fn click_view(&mut self, pos: Pos2, px: u32, py: u32, ppp: f32) {
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
        let edge = if edges_ok { self.edge_near(pos, &sels) } else { None };
        for s in &sels {
            self.ask_edges(&s.obj);
        }
        let hit = match (edge, sels.first(), center.clone()) {
            (Some(e), Some(s), _) => {
                let label = self.tree.nodes.get(&s.obj).map_or(s.obj.as_str(), |n| n.label.as_str()).to_string();
                self.info = Some(format!("picked {label}.Edge{e}"));
                Some(Selection { obj: s.obj.clone(), sub: Some(format!("Edge{e}")), face_id: 0 })
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
            picking::paint_edge(painter, e, &project, Color32::from_rgb(120, 185, 255));
        }
        for (obj, i) in &picked {
            if let Some(e) = edge(obj, *i) {
                picking::paint_edge(painter, e, &project, picking::HANDLE);
            }
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
    }
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
}
