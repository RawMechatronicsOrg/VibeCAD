//! Modelling commands: construction planes, extrude, fillet and chamfer, mirror, patterns and
//! copies. Each one shows a small dialog over the view; its fields are filled from the
//! selection when it starts and from clicks in the view or the tree while it is open, and OK
//! sends one request to the worker (see `Modelling` in `worker/vibecad_worker.py`).

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Stroke, pos2, vec2};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Plane,
    Extrude,
    Fillet,
    Chamfer,
    Mirror,
    Circular,
    Linear,
    Copy,
}

impl Op {
    pub fn title(self) -> &'static str {
        match self {
            Op::Plane => "Construction plane",
            Op::Extrude => "Extrude",
            Op::Fillet => "Fillet",
            Op::Chamfer => "Chamfer",
            Op::Mirror => "Mirror",
            Op::Circular => "Circular pattern",
            Op::Linear => "Linear pattern",
            Op::Copy => "Copy",
        }
    }

    /// The short name under its ribbon button.
    pub fn label(self) -> &'static str {
        match self {
            Op::Plane => "Plane",
            Op::Circular => "Circular",
            Op::Linear => "Linear",
            o => o.title(),
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Op::Plane => "A plane on a flat face, another plane or an origin plane, moved along its normal and tilted",
            Op::Extrude => "Pull a flat face out, or push it in to cut (a negative distance); or extrude a sketch",
            Op::Fillet => "Round every edge of a face",
            Op::Chamfer => "Bevel every edge of a face",
            Op::Mirror => "Mirror a part across a plane: a new solid, or joined into its body",
            Op::Circular => "Repeat a part around an axis",
            Op::Linear => "Repeat a part in a row along a direction",
            Op::Copy => "Copy a part, moved: independent, or a link that follows the original",
        }
    }

    /// Whether the command acts on a whole part, picked first.
    fn has_target(self) -> bool {
        matches!(self, Op::Mirror | Op::Circular | Op::Linear | Op::Copy)
    }

    /// What its reference field takes, if it has one.
    fn reference(self) -> Option<RefKind> {
        match self {
            Op::Plane | Op::Mirror => Some(RefKind::Plane),
            Op::Extrude => Some(RefKind::Profile),
            Op::Fillet | Op::Chamfer => Some(RefKind::Face),
            Op::Circular => Some(RefKind::Axis),
            Op::Linear => Some(RefKind::Direction),
            Op::Copy => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind {
    /// A flat face, a construction plane, or an origin plane.
    Plane,
    /// A flat face, or a sketch.
    Profile,
    /// Any face: its edges.
    Face,
    /// An origin axis, or a round face (its axis).
    Axis,
    /// An origin axis, or a flat face (its normal).
    Direction,
}

impl RefKind {
    fn bases(self) -> &'static [&'static str] {
        match self {
            RefKind::Plane => &["XY", "XZ", "YZ"],
            RefKind::Axis | RefKind::Direction => &["X", "Y", "Z"],
            RefKind::Profile | RefKind::Face => &[],
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            RefKind::Plane => "click a flat face or a plane",
            RefKind::Profile => "click a flat face, or a sketch in the tree",
            RefKind::Face => "click a face",
            RefKind::Axis => "click a round face",
            RefKind::Direction => "click a flat face",
        }
    }

    fn caption(self) -> &'static str {
        match self {
            RefKind::Plane => "Plane",
            RefKind::Profile => "Profile",
            RefKind::Face => "Face",
            RefKind::Axis => "Axis",
            RefKind::Direction => "Direction",
        }
    }
}

/// Something picked in the view or the tree.
#[derive(Clone, PartialEq, Debug)]
pub struct Pick {
    pub obj: String,
    /// "Face6" for a face of the object; None for the object itself (a plane, a sketch).
    pub sub: Option<String>,
    /// How it reads in the dialog.
    pub text: String,
    /// The object itself can be a reference: a construction plane, or a sketch to extrude.
    pub whole: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Ref {
    None,
    /// An origin plane or axis: "XY", "Z", ...
    Base(&'static str),
    Picked(Pick),
}

/// Which field a click fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Target,
    Reference,
}

pub enum Outcome {
    Open,
    Ok,
    Cancel,
}

pub struct Command {
    pub op: Op,
    target: Option<Pick>,
    reference: Ref,
    active: Slot,
    /// Extrude distance, or the fillet radius / chamfer size.
    distance: f64,
    symmetric: bool,
    /// Construction plane: along its normal, and turned about its X axis in degrees.
    offset: f64,
    angle: f64,
    join: bool,
    count: i64,
    /// Circular: the angle the copies spread over; linear: the step between them.
    sweep: f64,
    spacing: f64,
    shift: [f64; 3],
    linked: bool,
}

impl Command {
    /// A command started with `selection` (what is selected now) filling what it can, and
    /// `size` (the selected part's or the scene's extent) giving a copy its default shift.
    pub fn new(op: Op, selection: Option<Pick>, size: Option<[f32; 3]>) -> Self {
        let mut c = Command {
            op,
            target: None,
            // The usual first guesses: a plane on XY, mirror across YZ, turn about Z, step along X.
            reference: match op {
                Op::Plane => Ref::Base("XY"),
                Op::Mirror => Ref::Base("YZ"),
                Op::Circular => Ref::Base("Z"),
                Op::Linear => Ref::Base("X"),
                _ => Ref::None,
            },
            active: if op.has_target() { Slot::Target } else { Slot::Reference },
            distance: if matches!(op, Op::Fillet | Op::Chamfer) { 1.0 } else { 10.0 },
            symmetric: false,
            offset: 10.0,
            angle: 0.0,
            join: false,
            count: if op == Op::Circular { 6 } else { 3 },
            sweep: 360.0,
            spacing: size.map_or(20.0, |s| (s[0] as f64 * 1.2).max(1.0)),
            shift: [size.map_or(20.0, |s| (s[0] as f64 * 1.2).max(1.0)), 0.0, 0.0],
            linked: false,
        };
        if let Some(sel) = selection
            && (op.has_target() || sel.sub.is_some() || sel.whole)
        {
            c.take(sel);
        }
        c
    }

    /// Whether a click in the view should try construction planes before faces.
    pub fn wants_plane(&self) -> bool {
        self.active == Slot::Reference && self.op.reference() == Some(RefKind::Plane)
    }

    /// The object or plane the command highlights, beside the selection.
    pub fn referenced(&self) -> Option<&str> {
        match &self.reference {
            Ref::Picked(p) => Some(&p.obj),
            _ => None,
        }
    }

    /// Fill the active field with a pick, then move on to the next empty field.
    pub fn take(&mut self, p: Pick) {
        match self.active {
            Slot::Target => {
                // A face picked for the part means the part; it may also be the reference.
                let text = p.text.split('.').next().unwrap_or(&p.text).to_string();
                self.target = Some(Pick { obj: p.obj, sub: None, text, whole: false });
                if self.op.reference().is_some() {
                    self.active = Slot::Reference;
                }
            }
            Slot::Reference => {
                let Some(kind) = self.op.reference() else { return };
                let fits = match kind {
                    // The worker says when a face is not flat.
                    RefKind::Plane | RefKind::Profile => p.sub.is_some() || p.whole,
                    RefKind::Face | RefKind::Axis | RefKind::Direction => p.sub.is_some(),
                };
                if fits {
                    self.reference = Ref::Picked(p);
                }
            }
        }
    }

    /// The worker command and its arguments; an error says what is missing.
    pub fn request(&self, context: Option<&str>) -> Result<(&'static str, Value), String> {
        let target = || self.target.as_ref().map(|t| t.obj.clone()).ok_or_else(|| "pick the part first".to_string());
        let reference = || match &self.reference {
            Ref::None => Err(format!("choose the {}", self.op.reference().map_or("reference", |k| k.caption()).to_lowercase())),
            Ref::Base(b) => Ok(json!({ "base": b })),
            Ref::Picked(p) => Ok(match &p.sub {
                Some(s) => json!({ "obj": p.obj, "sub": s }),
                None => json!({ "obj": p.obj }),
            }),
        };
        let picked = || match &self.reference {
            Ref::Picked(p) => Ok(p.clone()),
            _ => Err(self.op.reference().map_or("pick a face", |k| k.prompt()).to_string()),
        };
        Ok(match self.op {
            Op::Plane => ("datum_plane", json!({ "ref": reference()?, "offset": self.offset, "angle": self.angle, "context": context })),
            Op::Extrude => {
                let p = picked()?;
                ("extrude", json!({ "obj": p.obj, "sub": p.sub, "distance": self.distance, "symmetric": self.symmetric }))
            }
            Op::Fillet | Op::Chamfer => {
                let p = picked()?;
                ("fillet", json!({ "obj": p.obj, "sub": p.sub, "size": self.distance, "chamfer": self.op == Op::Chamfer }))
            }
            Op::Mirror => ("mirror", json!({ "obj": target()?, "plane": reference()?, "join": self.join })),
            Op::Circular => ("pattern", json!({ "obj": target()?, "kind": "circular", "axis": reference()?, "count": self.count, "angle": self.sweep })),
            Op::Linear => ("pattern", json!({ "obj": target()?, "kind": "linear", "axis": reference()?, "count": self.count, "spacing": self.spacing })),
            Op::Copy => ("copy", json!({ "obj": target()?, "offset": self.shift, "linked": self.linked })),
        })
    }

    /// The dialog's fields and its OK and Cancel buttons.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Outcome {
        // Enter in a number field commits the number; it is OK only when no field has focus.
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        ui.weak(self.op.hint());
        ui.add_space(4.0);
        let mut out = Outcome::Open;
        egui::Grid::new("command").num_columns(2).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
            if self.op.has_target() {
                ui.label("Part");
                let text = self.target.as_ref().map(|t| t.text.clone());
                if pick_field(ui, text, self.active == Slot::Target, "click a part").clicked() {
                    self.active = Slot::Target;
                }
                ui.end_row();
            }
            if let Some(kind) = self.op.reference() {
                ui.label(kind.caption());
                ui.horizontal(|ui| {
                    for &b in kind.bases() {
                        if ui.selectable_label(self.reference == Ref::Base(b), b).clicked() {
                            self.reference = Ref::Base(b);
                            self.active = Slot::Reference;
                        }
                    }
                    let text = match &self.reference {
                        Ref::Picked(p) => Some(p.text.clone()),
                        _ => None,
                    };
                    let active = self.active == Slot::Reference && !matches!(self.reference, Ref::Base(_));
                    if pick_field(ui, text, active, kind.prompt()).clicked() {
                        self.active = Slot::Reference;
                        if matches!(self.reference, Ref::Base(_)) {
                            self.reference = Ref::None;
                        }
                    }
                });
                ui.end_row();
            }
            match self.op {
                Op::Plane => {
                    number(ui, "Offset", &mut self.offset, " mm");
                    number(ui, "Angle", &mut self.angle, "°");
                }
                Op::Extrude => {
                    number(ui, "Distance", &mut self.distance, " mm");
                    ui.label("");
                    ui.checkbox(&mut self.symmetric, "Both sides");
                    ui.end_row();
                }
                Op::Fillet => number(ui, "Radius", &mut self.distance, " mm"),
                Op::Chamfer => number(ui, "Size", &mut self.distance, " mm"),
                Op::Mirror => {
                    ui.label("Result");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.join, false, "New solid").on_hover_text("A solid of its own that follows the original");
                        ui.selectable_value(&mut self.join, true, "Join").on_hover_text("Added to the part's own body");
                    });
                    ui.end_row();
                }
                Op::Circular => {
                    count(ui, &mut self.count);
                    number(ui, "Angle", &mut self.sweep, "°");
                }
                Op::Linear => {
                    count(ui, &mut self.count);
                    number(ui, "Spacing", &mut self.spacing, " mm");
                }
                Op::Copy => {
                    for (i, axis) in ["Move X", "Move Y", "Move Z"].into_iter().enumerate() {
                        number(ui, axis, &mut self.shift[i], " mm");
                    }
                    ui.label("");
                    ui.checkbox(&mut self.linked, "Linked").on_hover_text("A link: it changes with the original");
                    ui.end_row();
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("  OK  ").strong()).on_hover_text("Enter").clicked() {
                out = Outcome::Ok;
            }
            if ui.button("Cancel").on_hover_text("Esc").clicked() {
                out = Outcome::Cancel;
            }
        });
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            out = Outcome::Ok;
        }
        out
    }
}

/// A field showing what was picked; lit while a click in the view fills it.
fn pick_field(ui: &mut egui::Ui, text: Option<String>, active: bool, prompt: &str) -> egui::Response {
    let shown = match (&text, active) {
        (Some(t), _) => RichText::new(t),
        (None, true) => RichText::new(prompt).italics(),
        (None, false) => RichText::new("—").weak(),
    };
    let r = ui.add(egui::Button::selectable(active, shown).truncate());
    if active { r.on_hover_text(prompt) } else { r.on_hover_text("Click, then pick in the view or the tree") }
}

fn number(ui: &mut egui::Ui, label: &str, v: &mut f64, unit: &str) {
    ui.label(label);
    let speed = (v.abs() * 0.01).clamp(0.01, 10.0);
    ui.add(egui::DragValue::new(v).speed(speed).max_decimals(4).suffix(unit));
    ui.end_row();
}

fn count(ui: &mut egui::Ui, n: &mut i64) {
    ui.label("Count");
    ui.add(egui::DragValue::new(n).range(2..=1000).speed(0.1));
    ui.end_row();
}

/// A construction plane as the view draws it: translucent orange, as in the big CAD tools.
pub fn paint_plane(painter: &egui::Painter, corners: &[Pos2], lit: bool) {
    let fill = Color32::from_rgba_unmultiplied(236, 150, 50, if lit { 90 } else { 40 });
    let line = Color32::from_rgba_unmultiplied(214, 120, 30, if lit { 255 } else { 170 });
    painter.add(egui::Shape::convex_polygon(corners.to_vec(), fill, Stroke::NONE));
    painter.add(egui::Shape::closed_line(corners.to_vec(), Stroke::new(if lit { 2.0 } else { 1.2 }, line)));
}

/// The ribbon icon of a command, drawn with lines like the others.
pub fn paint_icon(p: &egui::Painter, r: Rect, op: Op, c: Color32) {
    let s = Stroke::new(1.5, c);
    let thin = Stroke::new(1.0, c);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let poly = |pts: &[(f32, f32)], closed: bool, st: Stroke| {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| at(x, y)).collect();
        p.add(if closed { egui::Shape::closed_line(pts, st) } else { egui::Shape::line(pts, st) });
    };
    let arrow = |from: (f32, f32), to: (f32, f32)| {
        p.line_segment([at(from.0, from.1), at(to.0, to.1)], s);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let len = (dx * dx + dy * dy).sqrt().max(1e-3);
        let (ux, uy) = (dx / len * 0.18, dy / len * 0.18);
        poly(&[(to.0 - ux - uy * 0.6, to.1 - uy + ux * 0.6), to, (to.0 - ux + uy * 0.6, to.1 - uy - ux * 0.6)], false, s);
    };
    match op {
        Op::Plane => {
            // A parallelogram seen at an angle, filled faintly orange.
            let quad = [(0.2, 0.2), (1.0, 0.2), (0.8, 0.8), (0.0, 0.8)];
            let pts: Vec<Pos2> = quad.iter().map(|&(x, y)| at(x, y)).collect();
            p.add(egui::Shape::convex_polygon(pts, Color32::from_rgba_unmultiplied(236, 150, 50, 70), Stroke::NONE));
            poly(&quad, true, s);
        }
        Op::Extrude => {
            poly(&[(0.05, 0.7), (0.55, 0.7), (0.95, 0.5), (0.45, 0.5)], true, thin);
            poly(&[(0.05, 0.7), (0.05, 0.95), (0.55, 0.95), (0.95, 0.75), (0.95, 0.5)], false, thin);
            p.line_segment([at(0.55, 0.7), at(0.55, 0.95)], thin);
            arrow((0.5, 0.6), (0.5, 0.02));
        }
        Op::Fillet | Op::Chamfer => {
            // A block's corner, rounded or cut.
            p.line_segment([at(0.1, 0.9), at(0.1, 0.1)], s);
            p.line_segment([at(0.9, 0.9), at(0.1, 0.9)], s);
            p.line_segment([at(0.1, 0.1), at(0.45, 0.1)], s);
            p.line_segment([at(0.9, 0.9), at(0.9, 0.55)], s);
            if op == Op::Chamfer {
                p.line_segment([at(0.45, 0.1), at(0.9, 0.55)], s);
            } else {
                let pts: Vec<Pos2> = (0..=12)
                    .map(|i| {
                        let t = std::f32::consts::FRAC_PI_2 * i as f32 / 12.0;
                        at(0.45 + 0.45 * t.sin(), 0.55 - 0.45 * t.cos())
                    })
                    .collect();
                p.add(egui::Shape::line(pts, s));
            }
        }
        Op::Mirror => {
            poly(&[(0.05, 0.3), (0.38, 0.15), (0.38, 0.85), (0.05, 0.7)], true, s);
            poly(&[(0.95, 0.3), (0.62, 0.15), (0.62, 0.85), (0.95, 0.7)], true, thin);
            for y in [0.0f32, 0.25, 0.5, 0.75] {
                p.line_segment([at(0.5, y), at(0.5, y + 0.15)], thin);
            }
        }
        Op::Circular => {
            let pts: Vec<Pos2> = (0..=32)
                .map(|i| {
                    let t = std::f32::consts::TAU * i as f32 / 32.0;
                    at(0.5 + 0.32 * t.cos(), 0.5 + 0.32 * t.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, thin));
            for k in 0..6 {
                let t = std::f32::consts::TAU * k as f32 / 6.0;
                p.circle_filled(at(0.5 + 0.38 * t.cos(), 0.5 + 0.38 * t.sin()), r.width() * 0.09, c);
            }
        }
        Op::Linear => {
            for k in 0..3 {
                let x = 0.05 + k as f32 * 0.33;
                p.rect_stroke(Rect::from_min_max(at(x, 0.35), at(x + 0.24, 0.65)), 1.0, if k == 0 { s } else { thin }, egui::StrokeKind::Middle);
            }
        }
        Op::Copy => {
            p.rect_stroke(Rect::from_min_max(at(0.05, 0.05), at(0.6, 0.6)), 1.0, thin, egui::StrokeKind::Middle);
            p.rect_stroke(Rect::from_min_max(at(0.4, 0.4), at(0.95, 0.95)), 1.0, s, egui::StrokeKind::Middle);
        }
    }
}

/// The ribbon icon of Delete: a bin.
pub fn paint_delete(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.5, c);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    p.line_segment([at(0.1, 0.2), at(0.9, 0.2)], s);
    p.add(egui::Shape::line(vec![at(0.38, 0.2), at(0.38, 0.05), at(0.62, 0.05), at(0.62, 0.2)], s));
    p.add(egui::Shape::closed_line(vec![at(0.2, 0.2), at(0.8, 0.2), at(0.72, 0.95), at(0.28, 0.95)], s));
    for x in [0.42f32, 0.58] {
        p.line_segment([at(x, 0.35), at(x, 0.8)], Stroke::new(1.0, c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(obj: &str, n: u32) -> Pick {
        Pick { obj: obj.into(), sub: Some(format!("Face{n}")), text: format!("{obj}.Face{n}"), whole: false }
    }

    #[test]
    fn a_picked_face_starts_an_extrude() {
        let c = Command::new(Op::Extrude, Some(face("Pocket", 5)), None);
        let (cmd, args) = c.request(None).unwrap();
        assert_eq!(cmd, "extrude");
        assert_eq!(args["obj"], "Pocket");
        assert_eq!(args["sub"], "Face5");
        assert_eq!(args["distance"], 10.0);
    }

    #[test]
    fn an_extrude_without_a_profile_asks_for_one() {
        let c = Command::new(Op::Extrude, None, None);
        assert!(c.request(None).unwrap_err().contains("flat face"));
    }

    #[test]
    fn mirror_takes_the_part_then_a_plane() {
        let mut c = Command::new(Op::Mirror, Some(face("Pocket", 3)), Some([80.0, 50.0, 6.0]));
        // The selection gave the part; the plane starts as YZ.
        let (_, args) = c.request(None).unwrap();
        assert_eq!(args["obj"], "Pocket");
        assert_eq!(args["plane"], json!({ "base": "YZ" }));
        assert!(c.wants_plane());
        c.take(face("Pocket", 6));
        let (_, args) = c.request(None).unwrap();
        assert_eq!(args["plane"], json!({ "obj": "Pocket", "sub": "Face6" }));
        assert_eq!(c.target.as_ref().unwrap().text, "Pocket");
    }

    #[test]
    fn a_plane_on_a_selected_face_uses_it_and_otherwise_starts_on_xy() {
        let c = Command::new(Op::Plane, Some(face("Pocket", 5)), None);
        assert_eq!(c.request(None).unwrap().1["ref"], json!({ "obj": "Pocket", "sub": "Face5" }));
        let c = Command::new(Op::Plane, Some(Pick { obj: "Body".into(), sub: None, text: "Body".into(), whole: false }), None);
        let (_, args) = c.request(Some("Body")).unwrap();
        assert_eq!(args["ref"], json!({ "base": "XY" }));
        assert_eq!(args["context"], "Body");
    }

    #[test]
    fn an_axis_needs_a_face_not_a_whole_object() {
        let mut c = Command::new(Op::Circular, None, None);
        c.take(Pick { obj: "Body".into(), sub: None, text: "Body".into(), whole: false });
        assert_eq!(c.reference, Ref::Base("Z"));
        c.take(Pick { obj: "Plane".into(), sub: None, text: "Plane".into(), whole: true });
        assert_eq!(c.reference, Ref::Base("Z"), "a whole object is no axis");
        c.take(face("Pocket", 7));
        assert_eq!(c.request(None).unwrap().1["axis"], json!({ "obj": "Pocket", "sub": "Face7" }));
    }

    #[test]
    fn a_copy_moves_by_the_parts_width_and_a_bit() {
        let c = Command::new(Op::Copy, Some(face("Pocket", 1)), Some([80.0, 50.0, 6.0]));
        let (cmd, args) = c.request(None).unwrap();
        assert_eq!(cmd, "copy");
        assert!((args["offset"][0].as_f64().unwrap() - 96.0).abs() < 1e-9);
        assert_eq!(args["linked"], false);
    }
}
