//! The validators in the window: EDIT → Check asks the worker for the places its validators
//! find (`validate`, see `cmd_validate` in the worker) and lists them worst first. A click on
//! a place turns the view to it, a second click goes close. While the panel is open the check
//! runs again after every change to the model, so a place that was mended leaves the list.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke};
use glam::{Mat4, Vec3};
use serde_json::{Value, json};

use super::{App, Pending};

/// Seconds the worker may spend on one check from the window; its other requests wait.
const SECONDS: f64 = 10.0;
const MARK: Color32 = Color32::from_rgb(226, 74, 51);

/// One place of the worker's reply, as the list and the view show it.
struct Place {
    /// "0.02 mm" or "4.7°": how small.
    measure: String,
    /// The smallest length in it, in mm; for a place measured by an angle, the limit.
    small: f32,
    kind: String,
    body: String,
    what: Vec<String>,
    /// The feature it first shows at, and the sketches that feature is built from.
    since: Option<String>,
    at: Vec3,
    lo: Vec3,
    hi: Vec3,
}

#[derive(Default)]
pub(super) struct Validation {
    pub(super) show: bool,
    /// The model changed, or the limit did: check again.
    pub(super) dirty: bool,
    /// The latest check asked for; replies to earlier ones are dropped.
    asked: u64,
    places: Vec<Place>,
    picked: Option<usize>,
    /// What counts as too small, in mm: as typed, else as the worker chose (`used`).
    min_size: Option<f64>,
    used: f64,
    /// What the last check left out, in words.
    note: Option<String>,
    checked: bool,
}

impl Validation {
    /// With the panel open or not, as the last run left it.
    pub(super) fn new(show: bool) -> Self {
        Validation { show, ..Default::default() }
    }

    /// Another document, or FreeCAD started again: nothing of the last places holds.
    pub(super) fn clear(&mut self) {
        *self = Validation { show: self.show, dirty: true, min_size: self.min_size, ..Default::default() };
    }

    /// The check with this id failed: places from before it would pass for what the model has now.
    pub(super) fn failed(&mut self, id: u64, why: &str) {
        if id == self.asked {
            self.places.clear();
            self.picked = None;
            self.checked = false;
            self.note = Some(format!("The check failed: {why}"));
        }
    }
}

fn vec3(v: &Value) -> Option<Vec3> {
    Some(Vec3::new(v.get(0)?.as_f64()? as f32, v.get(1)?.as_f64()? as f32, v.get(2)?.as_f64()? as f32))
}

fn place(p: &Value, limit: f64) -> Option<Place> {
    let b = &p["bbox"];
    let since = p["since"]["feature"].as_str().map(|f| {
        let sketches: Vec<&str> = p["since"]["sketches"].as_array().map(|s| s.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let label = p["since"]["label"].as_str().unwrap_or(f);
        if sketches.is_empty() { format!("since {label}") } else { format!("since {label} (from {})", sketches.join(", ")) }
    });
    Some(Place {
        measure: match (p["size"].as_f64(), p["angle"].as_f64()) {
            (Some(s), _) => format!("{} mm", (s * 1e4).round() / 1e4),
            (None, Some(a)) => format!("{}°", (a * 10.0).round() / 10.0),
            (None, None) => String::new(),
        },
        small: p["size"].as_f64().unwrap_or(limit) as f32,
        kind: p["kind"].as_str().unwrap_or("").to_string(),
        body: p["label"].as_str().unwrap_or("").to_string(),
        what: p["what"].as_array().map(|w| w.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default(),
        since,
        at: vec3(&p["at"])?,
        lo: vec3(b)?,
        hi: Vec3::new(b.get(3)?.as_f64()? as f32, b.get(4)?.as_f64()? as f32, b.get(5)?.as_f64()? as f32),
    })
}

impl App {
    /// Open or close the panel; opening it checks.
    pub(super) fn toggle_validation(&mut self) {
        self.validation.show = !self.validation.show;
        self.validation.dirty |= self.validation.show;
    }

    fn validate(&mut self) {
        let mut args = json!({ "seconds": SECONDS });
        if let Some(m) = self.validation.min_size {
            args["min_size"] = json!(m);
        }
        if let Some(id) = self.request("validate", args, Pending::Validate) {
            self.validation.asked = id;
        }
    }

    pub(super) fn on_validated(&mut self, id: u64, v: &Value) {
        let val = &mut self.validation;
        if id != val.asked {
            return;
        }
        val.used = v["min_size"].as_f64().unwrap_or(0.0);
        // The picked place stays picked if it is still there: same kind, same spot.
        let was = val.picked.and_then(|i| val.places.get(i)).map(|p| (p.kind.clone(), p.at));
        val.places = v["places"].as_array().map(|ps| ps.iter().filter_map(|p| place(p, val.used)).collect()).unwrap_or_default();
        val.picked = was.and_then(|(kind, at)| val.places.iter().position(|p| p.kind == kind && p.at.distance(at) < 1e-3));
        val.checked = true;
        let left: Vec<String> = v["bodies"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter_map(|b| {
                let why = b["skipped"].as_str().or(b["incomplete"].as_str())?;
                Some(format!("{}: {why}", b["label"].as_str().unwrap_or("?")))
            })
            .collect();
        let more = v["places_not_listed"].as_u64().map(|n| format!("{n} more places are not listed"));
        val.note = (!left.is_empty() || more.is_some())
            .then(|| format!("Not all was looked at. {}", left.into_iter().chain(more).collect::<Vec<_>>().join("; ")));
    }

    pub(super) fn validation_panel(&mut self, ui: &mut egui::Ui) {
        // Not while a sketch is drawn: every stroke changes the model, and a check holds the worker.
        // One at a time: a check asked while another runs would queue behind it on the worker,
        // and with it everything else; it is asked once that one is back.
        let running = self.pending.values().any(|p| matches!(p, Pending::Validate));
        if self.validation.dirty && !running && self.sketch.is_none() && self.doc_label.is_some() && self.worker_error.is_none() {
            self.validation.dirty = false;
            self.validate();
        }
        let running = self.pending.values().any(|p| matches!(p, Pending::Validate));
        ui.horizontal(|ui| {
            ui.label("Smaller than");
            let hint = "What is too small to be meant: walls, gaps, strips and edges under this are listed.\nLeft alone it is 0.1 mm, or a thousandth of the model's size if that is less.";
            // Until the first check says what it used there is no number to show: a field
            // would clamp 0 into its range and pass that on as the user's choice.
            let before = self.validation.min_size.or(Some(self.validation.used).filter(|u| *u > 0.0));
            if let Some(before) = before {
                let mut m = before;
                let field = egui::DragValue::new(&mut m).speed(0.005).range(0.0001..=1000.0).max_decimals(4).suffix(" mm").update_while_editing(false);
                let r = ui.add(field).on_hover_text(hint);
                if r.changed() && m != before {
                    self.validation.min_size = Some(m);
                    self.validation.dirty |= !r.dragged();
                }
                if r.drag_stopped() {
                    self.validation.dirty = true;
                }
            } else {
                ui.weak("…").on_hover_text(hint);
            }
            if ui.button("Check").on_hover_text("Look again now; it also runs after every change while this panel is open").clicked() {
                self.validation.dirty = true;
            }
            if running {
                ui.spinner();
            }
        });
        if let Some(note) = &self.validation.note {
            ui.colored_label(Color32::from_rgb(214, 140, 40), egui::RichText::new(note).small());
        }
        ui.separator();
        let val = &self.validation;
        if val.places.is_empty() {
            ui.weak(match (val.checked, running) {
                (true, _) => format!("Nothing under {} mm, no sharp edge: no degenerate place found.", val.used),
                (false, true) => "Looking…".to_string(),
                (false, false) => "Open a document to check it.".to_string(),
            });
            return;
        }
        let mut clicked = None;
        for (i, p) in val.places.iter().enumerate() {
            let picked = val.picked == Some(i);
            let row = format!("{}  ·  {}  ·  {}", p.measure, p.kind, p.body);
            let r = ui.selectable_label(picked, row);
            if r.on_hover_text(format!("{}\nClick: turn the view to it. Click again: go close.", p.what.join("\n"))).clicked() {
                clicked = Some(i);
            }
            if picked {
                for line in p.what.iter().chain(&p.since) {
                    ui.label(egui::RichText::new(format!("    {line}")).small().weak());
                }
            }
        }
        if let Some(i) = clicked {
            let p = &self.validation.places[i];
            let close = p.small.max(1e-4) * 12.0;
            if self.validation.picked == Some(i) {
                self.camera.frame(p.at, close);
            } else {
                // All of the place, and room around it to see where on the part it is.
                self.camera.frame((p.lo + p.hi) * 0.5, ((p.hi - p.lo).length() * 0.75).max(close));
            }
            self.validation.picked = Some(i);
        }
    }

    /// The places over the view: a dot each, and the box of the picked one.
    pub(super) fn paint_places(&self, painter: &egui::Painter, rect: Rect, view_proj: Mat4) {
        if !self.validation.show {
            return;
        }
        let project = |p: Vec3| -> Option<Pos2> {
            let c = view_proj * p.extend(1.0);
            (c.w > 1e-6).then(|| {
                let n = c.truncate() / c.w;
                egui::pos2(rect.left() + (n.x * 0.5 + 0.5) * rect.width(), rect.top() + (0.5 - n.y * 0.5) * rect.height())
            })
        };
        for (i, p) in self.validation.places.iter().enumerate() {
            let Some(at) = project(p.at) else { continue };
            if self.validation.picked != Some(i) {
                painter.circle_filled(at, 3.5, MARK);
                painter.circle_stroke(at, 3.5, Stroke::new(1.0, Color32::WHITE));
                continue;
            }
            let corner = |k: usize| Vec3::new(if k & 1 == 0 { p.lo.x } else { p.hi.x }, if k & 2 == 0 { p.lo.y } else { p.hi.y }, if k & 4 == 0 { p.lo.z } else { p.hi.z });
            for k in 0..8usize {
                for bit in [1usize, 2, 4] {
                    if k & bit == 0
                        && let (Some(a), Some(b)) = (project(corner(k)), project(corner(k | bit)))
                    {
                        painter.line_segment([a, b], Stroke::new(1.5, MARK));
                    }
                }
            }
            painter.circle_stroke(at, 9.0, Stroke::new(2.0, MARK));
            painter.circle_filled(at, 2.5, MARK);
        }
    }
}
