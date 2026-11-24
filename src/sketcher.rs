//! The window's sketch mode, without the window: a sketch as the worker describes it, its
//! plane, where the pointer lands on it, snapping, and the drawing tools that turn clicks into
//! one `sketch_edit` each. FreeCAD's Sketcher solves; nothing here does.
//!
//! Coordinates are the sketch's own (x, y) in mm (f64); the plane takes them to the document.

use eframe::egui::{Pos2, Rect};
use glam::{DVec2, Mat4, Vec3, Vec4};
use serde_json::{Value, json};

/// The plane a sketch lies on, in document coordinates: its (x, y) run along `x` and `y`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plane {
    pub origin: Vec3,
    pub normal: Vec3,
    pub x: Vec3,
    pub y: Vec3,
}

fn vec3(v: &Value) -> Option<Vec3> {
    Some(Vec3::new(v[0].as_f64()? as f32, v[1].as_f64()? as f32, v[2].as_f64()? as f32))
}

fn dvec2(v: &Value) -> Option<DVec2> {
    Some(DVec2::new(v[0].as_f64()?, v[1].as_f64()?))
}

impl Plane {
    pub fn from_reply(v: &Value) -> Option<Plane> {
        let (origin, normal, x) = (vec3(&v["origin"])?, vec3(&v["normal"])?.normalize_or_zero(), vec3(&v["x_dir"])?.normalize_or_zero());
        (normal != Vec3::ZERO && x != Vec3::ZERO).then(|| Plane { origin, normal, x, y: normal.cross(x) })
    }

    pub fn world(&self, p: DVec2) -> Vec3 {
        self.origin + self.x * p.x as f32 + self.y * p.y as f32
    }

    pub fn local(&self, w: Vec3) -> DVec2 {
        let d = w - self.origin;
        DVec2::new(d.dot(self.x) as f64, d.dot(self.y) as f64)
    }

    /// Where the line of sight through `pos` (a point of the view `rect` drawn with
    /// `view_proj`) meets the plane; None when it runs along the plane.
    pub fn hit(&self, view_proj: Mat4, rect: Rect, pos: Pos2) -> Option<DVec2> {
        let inv = view_proj.inverse();
        let nx = (pos.x - rect.left()) / rect.width() * 2.0 - 1.0;
        let ny = 1.0 - (pos.y - rect.top()) / rect.height() * 2.0;
        // Two depths inside the clip range whichever depth convention the projection uses.
        let at = |z: f32| {
            let w = inv * Vec4::new(nx, ny, z, 1.0);
            (w.w.abs() > 1e-12).then(|| w.truncate() / w.w)
        };
        let (a, b) = (at(0.2)?, at(0.8)?);
        let dir = b - a;
        let along = dir.dot(self.normal);
        if along.abs() < 1e-9 * dir.length().max(1.0) {
            return None;
        }
        let t = (self.origin - a).dot(self.normal) / along;
        Some(self.local(a + dir * t))
    }
}

/// A point of an element, as Sketcher names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    Start,
    End,
    Center,
}

impl At {
    pub fn name(self) -> &'static str {
        match self {
            At::Start => "start",
            At::End => "end",
            At::Center => "center",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Line(DVec2, DVec2),
    Circle(DVec2, f64),
    /// Centre, radius, start and end angle in radians, counter-clockwise.
    Arc(DVec2, f64, f64, f64),
    Point(DVec2),
    /// Anything else, as a polyline (ellipses, B-splines).
    Curve(Vec<DVec2>),
}

impl Shape {
    /// Points along it, closely enough to draw and to hit.
    pub fn polyline(&self) -> Vec<DVec2> {
        let round = |c: DVec2, r: f64, a0: f64, a1: f64| {
            let n = (((a1 - a0).abs() / std::f64::consts::TAU) * 96.0).ceil().max(4.0) as usize;
            (0..=n).map(|k| a0 + (a1 - a0) * k as f64 / n as f64).map(|a| c + r * DVec2::new(a.cos(), a.sin())).collect()
        };
        match self {
            Shape::Line(a, b) => vec![*a, *b],
            Shape::Circle(c, r) => round(*c, *r, 0.0, std::f64::consts::TAU),
            Shape::Arc(c, r, a0, a1) => round(*c, *r, *a0, *a1),
            Shape::Point(p) => vec![*p],
            Shape::Curve(pts) => pts.clone(),
        }
    }

    /// The points a click can snap to and a constraint can hold.
    pub fn points(&self) -> Vec<(At, DVec2)> {
        match self {
            Shape::Line(a, b) => vec![(At::Start, *a), (At::End, *b)],
            Shape::Circle(c, _) => vec![(At::Center, *c)],
            Shape::Arc(c, r, a0, a1) => {
                vec![(At::Start, *c + *r * DVec2::new(a0.cos(), a0.sin())), (At::End, *c + *r * DVec2::new(a1.cos(), a1.sin())), (At::Center, *c)]
            }
            // A Part.Point is its own start.
            Shape::Point(p) => vec![(At::Start, *p)],
            Shape::Curve(_) => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub index: usize,
    pub shape: Shape,
    pub construction: bool,
}

/// A closed region, shaded as a profile: triangles over it in the sketch's (x, y).
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub points: Vec<DVec2>,
    pub triangles: Vec<[u32; 3]>,
}

/// A constraint as the worker lists it; a dimension carries its value and property name.
#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub index: usize,
    pub kind: String,
    pub refs: Vec<Value>,
    pub value: Option<f64>,
    pub property: Option<String>,
    /// False for a reference dimension, which only measures.
    pub driving: bool,
    /// The expression a dimension is bound to, which sets its value.
    pub expression: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sketch {
    pub obj: String,
    pub label: String,
    pub plane: Plane,
    /// How many elements the sketch has (`elements` leaves out kinds it cannot draw).
    pub count: usize,
    pub elements: Vec<Element>,
    pub constraints: Vec<Constraint>,
    pub profiles: Vec<Profile>,
    pub dof: Option<i64>,
    pub fully_constrained: bool,
}

fn shape(g: &Value) -> Option<Shape> {
    let tau = std::f64::consts::TAU;
    Some(match g["type"].as_str()? {
        "LineSegment" => Shape::Line(dvec2(&g["start"])?, dvec2(&g["end"])?),
        "Circle" => Shape::Circle(dvec2(&g["center"])?, g["radius"].as_f64()?),
        "ArcOfCircle" => {
            let (a0, mut a1) = (g["start_angle"].as_f64()?.to_radians(), g["end_angle"].as_f64()?.to_radians());
            while a1 <= a0 {
                a1 += tau;
            }
            Shape::Arc(dvec2(&g["center"])?, g["radius"].as_f64()?, a0, a1)
        }
        "Point" => Shape::Point(dvec2(&g["at"])?),
        "BSplineCurve" => Shape::Curve(g["poles"].as_array()?.iter().filter_map(dvec2).collect()),
        _ => Shape::Curve(vec![dvec2(&g["start"])?, dvec2(&g["end"])?]),
    })
}

impl Sketch {
    /// A `sketch` reply of the worker (asked with outlines, for the profiles' triangles).
    pub fn parse(v: &Value) -> Option<Sketch> {
        let elements = v["geometry"]
            .as_array()?
            .iter()
            .enumerate()
            .filter_map(|(i, g)| {
                Some(Element { index: g["index"].as_u64().map_or(i, |x| x as usize), shape: shape(g)?, construction: g["construction"].as_bool() == Some(true) })
            })
            .collect();
        let constraints = v["constraints"]
            .as_array()
            .map(|cs| {
                cs.iter()
                    .enumerate()
                    .map(|(i, c)| Constraint {
                        index: c["index"].as_u64().map_or(i, |x| x as usize),
                        kind: c["type"].as_str().unwrap_or("").to_string(),
                        refs: c["refs"].as_array().cloned().unwrap_or_default(),
                        value: c["value"].as_f64(),
                        property: c["property"].as_str().map(String::from),
                        driving: c["driving"].as_bool() != Some(false),
                        expression: c["expression"].as_str().map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let profiles = v["profiles"]
            .as_array()
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| {
                        let m = &p["mesh"];
                        let pts = m["points"].as_array()?;
                        let tris = m["triangles"].as_array()?;
                        Some(Profile {
                            points: pts.chunks_exact(2).filter_map(|c| Some(DVec2::new(c[0].as_f64()?, c[1].as_f64()?))).collect(),
                            triangles: tris.chunks_exact(3).filter_map(|t| Some([t[0].as_u64()? as u32, t[1].as_u64()? as u32, t[2].as_u64()? as u32])).collect(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Sketch {
            obj: v["obj"].as_str()?.to_string(),
            label: v["label"].as_str().unwrap_or("").to_string(),
            plane: Plane::from_reply(&v["plane"])?,
            count: v["geometry"].as_array()?.len(),
            elements,
            constraints,
            profiles,
            dof: v["dof"].as_i64(),
            fully_constrained: v["fully_constrained"].as_bool() == Some(true),
        })
    }

    /// The box of its elements in its (x, y), or None when it is empty.
    pub fn bounds(&self) -> Option<(DVec2, DVec2)> {
        let pts: Vec<DVec2> = self.elements.iter().flat_map(|e| e.shape.polyline()).collect();
        let first = *pts.first()?;
        Some(pts.iter().fold((first, first), |(lo, hi), p| (lo.min(*p), hi.max(*p))))
    }
}

// ---- snapping -----------------------------------------------------------------------------

/// What a click lands on: an existing point (joined with a coincident constraint), the
/// sketch's origin, or free space; and whether it lines up with the click before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Point(usize, At),
    Origin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snap {
    pub at: DVec2,
    pub target: Option<Target>,
    pub align: Option<Align>,
}

impl Snap {
    pub fn free(at: DVec2) -> Snap {
        Snap { at, target: None, align: None }
    }
}

/// How far, in points of the screen, a click reaches for a point to snap to.
pub const SNAP_REACH: f64 = 9.0;

/// A round step near `mm` (1, 2 or 5 times a power of ten).
fn round_step(mm: f64) -> f64 {
    let p = 10f64.powf(mm.max(1e-9).log10().floor());
    [1.0, 2.0, 5.0, 10.0].into_iter().map(|k| k * p).find(|s| *s >= mm).unwrap_or(10.0 * p)
}

/// The snap for the pointer at `cursor`, `mm_per_pt` mm per point of the screen: the nearest
/// point within reach (a point of an element, the origin, or `extra`, a point drawn but not in
/// the sketch yet), else lined up with `prev` horizontally or vertically, else the cursor
/// rounded to a step of about 4 points.
pub fn snap(sketch: &Sketch, cursor: DVec2, mm_per_pt: f64, prev: Option<DVec2>, extra: &[(Target, DVec2)]) -> Snap {
    let reach = SNAP_REACH * mm_per_pt;
    let mut best: Option<(f64, Target, DVec2)> = None;
    let mut consider = |t: Target, p: DVec2| {
        let d = p.distance(cursor);
        if d <= reach && best.is_none_or(|(b, ..)| d < b) {
            best = Some((d, t, p));
        }
    };
    consider(Target::Origin, DVec2::ZERO);
    for e in &sketch.elements {
        for (at, p) in e.shape.points() {
            consider(Target::Point(e.index, at), p);
        }
    }
    for &(t, p) in extra {
        consider(t, p);
    }
    let align_with = |at: DVec2| {
        let p = prev?;
        if (at.y - p.y).abs() <= reach && (at.x - p.x).abs() > reach {
            Some(Align::Horizontal)
        } else if (at.x - p.x).abs() <= reach && (at.y - p.y).abs() > reach {
            Some(Align::Vertical)
        } else {
            None
        }
    };
    if let Some((_, t, p)) = best {
        return Snap { at: p, target: Some(t), align: align_with(p).filter(|a| lined_up(prev, p, *a)) };
    }
    let step = round_step(4.0 * mm_per_pt);
    let mut at = (cursor / step).round() * step;
    let align = align_with(cursor);
    match (align, prev) {
        (Some(Align::Horizontal), Some(p)) => at.y = p.y,
        (Some(Align::Vertical), Some(p)) => at.x = p.x,
        _ => {}
    }
    Snap { at, target: None, align }
}

/// Whether `p` lies exactly level with or plumb under `prev`, for a snapped point.
fn lined_up(prev: Option<DVec2>, p: DVec2, a: Align) -> bool {
    prev.is_some_and(|q| match a {
        Align::Horizontal => (p.y - q.y).abs() < 1e-9,
        Align::Vertical => (p.x - q.x).abs() < 1e-9,
    })
}

// ---- drawing tools ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Line,
    Rectangle,
    CenterRectangle,
    Circle,
    Arc,
    Point,
}

impl Tool {
    pub const ALL: [Tool; 6] = [Tool::Line, Tool::Rectangle, Tool::CenterRectangle, Tool::Circle, Tool::Arc, Tool::Point];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Line => "Line",
            Tool::Rectangle => "Rectangle",
            Tool::CenterRectangle => "Center rect",
            Tool::Circle => "Circle",
            Tool::Arc => "Arc",
            Tool::Point => "Point",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Tool::Line => "Lines end to end: click points, Esc or a double click ends (L)",
            Tool::Rectangle => "A rectangle by two opposite corners (R)",
            Tool::CenterRectangle => "A rectangle by its centre and a corner",
            Tool::Circle => "A circle by its centre and a point on it (C)",
            Tool::Arc => "An arc through three points: start, end, then one on the arc (A)",
            Tool::Point => "A point (P)",
        }
    }

    /// What the next click gives, for the prompt over the view.
    fn prompts(self) -> &'static [&'static str] {
        match self {
            Tool::Line => &["click the start point", "click the end point (Esc ends)"],
            Tool::Rectangle => &["click a corner", "click the opposite corner"],
            Tool::CenterRectangle => &["click the centre", "click a corner"],
            Tool::Circle => &["click the centre", "click a point on the circle"],
            Tool::Arc => &["click the start point", "click the end point", "click a point on the arc"],
            Tool::Point => &["click where the point goes"],
        }
    }
}

/// A tool in use: the clicks so far. A line chain also keeps the index its last segment got.
#[derive(Clone, Debug, PartialEq)]
pub struct Drawing {
    pub tool: Tool,
    pub clicks: Vec<Snap>,
    /// Line tool: the element the last segment became, whose end the next one starts at.
    pub chain: Option<usize>,
}

/// One edit for the worker, and how many elements it adds.
pub struct Edit {
    pub args: Value,
    pub adds: usize,
}

fn target_ref(t: Target) -> Value {
    match t {
        Target::Point(i, at) => json!({ "geometry": i, "at": at.name() }),
        Target::Origin => json!({ "origin": true }),
    }
}

fn xy(p: DVec2) -> Value {
    json!([round(p.x), round(p.y)])
}

/// Off by no more than float noise: 12.300000000000001 is sent as 12.3.
fn round(v: f64) -> f64 {
    (v * 1e9).round() / 1e9
}

/// A coincident constraint joining point `at` of new element `k` to what a click snapped to.
fn joined(k: usize, at: At, snap: &Snap) -> Option<Value> {
    snap.target.map(|t| json!({ "type": "coincident", "refs": [{ "new": k, "at": at.name() }, target_ref(t)] }))
}

impl Drawing {
    pub fn new(tool: Tool) -> Self {
        Drawing { tool, clicks: Vec::new(), chain: None }
    }

    pub fn prompt(&self) -> &'static str {
        let p = self.tool.prompts();
        p[self.clicks.len().min(p.len() - 1)]
    }

    /// The click before the next one, for lining up with it.
    pub fn last(&self) -> Option<DVec2> {
        self.clicks.last().map(|s| s.at)
    }

    /// A click. When the tool has all it needs, the edit that draws it; `next` is the index
    /// the edit's first new element will get.
    pub fn click(&mut self, s: Snap, next: usize) -> Option<Edit> {
        self.clicks.push(s);
        let c = &self.clicks;
        let mut cons: Vec<Value> = Vec::new();
        let (add, adds): (Value, usize) = match (self.tool, c.len()) {
            (Tool::Point, 1) => {
                cons.extend(joined(0, At::Start, &c[0]));
                (json!({ "type": "point", "at": xy(c[0].at) }), 1)
            }
            (Tool::Line, 2) => {
                let (a, b) = (c[0], c[1]);
                if a.at.distance(b.at) < 1e-9 {
                    self.clicks.pop();
                    return None;
                }
                match self.chain {
                    Some(prev) => cons.push(json!({ "type": "coincident", "refs": [{ "new": 0, "at": "start" }, { "geometry": prev, "at": "end" }] })),
                    None => cons.extend(joined(0, At::Start, &a)),
                }
                cons.extend(joined(0, At::End, &b));
                match b.align {
                    Some(Align::Horizontal) => cons.push(json!({ "type": "horizontal", "refs": [{ "new": 0 }] })),
                    Some(Align::Vertical) => cons.push(json!({ "type": "vertical", "refs": [{ "new": 0 }] })),
                    None => {}
                }
                // The next segment starts where this one ended, unless it closed on a point.
                let closed = b.target.is_some();
                self.clicks = if closed { Vec::new() } else { vec![Snap::free(b.at)] };
                self.chain = (!closed).then_some(next);
                return Some(Edit { args: json!({ "add": [{ "type": "line", "start": xy(a.at), "end": xy(b.at) }], "constraints": cons }), adds: 1 });
            }
            (Tool::Rectangle | Tool::CenterRectangle, 2) => {
                let (a, b) = (c[0].at, c[1].at);
                let (p, q) = if self.tool == Tool::Rectangle { (a, b) } else { (2.0 * a - b, b) };
                if (p.x - q.x).abs() < 1e-9 || (p.y - q.y).abs() < 1e-9 {
                    self.clicks.pop();
                    return None;
                }
                // The worker's lines run bottom, right, top, left, each from the corner it starts at.
                let (lo, hi) = (p.min(q), p.max(q));
                let corners = [lo, DVec2::new(hi.x, lo.y), hi, DVec2::new(lo.x, hi.y)];
                let corner_of = |pt: DVec2| corners.iter().position(|k| k.distance(pt) < 1e-9);
                if self.tool == Tool::Rectangle {
                    for s in [c[0], c[1]] {
                        if let Some(k) = corner_of(s.at) {
                            cons.extend(joined(k, At::Start, &s));
                        }
                    }
                    (json!({ "type": "rectangle", "corner": xy(a), "opposite": xy(b) }), 4)
                } else {
                    cons.extend(joined(4, At::Start, &c[0]));
                    if let Some(k) = corner_of(c[1].at) {
                        cons.extend(joined(k, At::Start, &c[1]));
                    }
                    (json!({ "type": "center_rectangle", "center": xy(a), "corner": xy(b) }), 5)
                }
            }
            (Tool::Circle, 2) => {
                let r = c[0].at.distance(c[1].at);
                if r < 1e-9 {
                    self.clicks.pop();
                    return None;
                }
                cons.extend(joined(0, At::Center, &c[0]));
                (json!({ "type": "circle", "center": xy(c[0].at), "radius": round(r) }), 1)
            }
            (Tool::Arc, 3) => {
                let (a, b, m) = (c[0].at, c[1].at, c[2].at);
                if (b - a).perp_dot(m - a).abs() < 1e-9 {
                    self.clicks.pop();
                    return None;
                }
                cons.extend(joined(0, At::Start, &c[0]));
                cons.extend(joined(0, At::End, &c[1]));
                (json!({ "type": "arc3", "start": xy(a), "through": xy(m), "end": xy(b) }), 1)
            }
            _ => return None,
        };
        self.clicks.clear();
        Some(Edit { args: json!({ "add": [add], "constraints": cons }), adds })
    }

    /// The shapes the next click would make, with the pointer at `cur`, to draw as a preview.
    pub fn preview(&self, cur: DVec2) -> Vec<Shape> {
        let c: Vec<DVec2> = self.clicks.iter().map(|s| s.at).collect();
        match (self.tool, c.as_slice()) {
            (Tool::Line, [a]) => vec![Shape::Line(*a, cur)],
            (Tool::Rectangle, [a]) => rectangle(*a, cur),
            (Tool::CenterRectangle, [a]) => rectangle(2.0 * *a - cur, cur),
            (Tool::Circle, [a]) => vec![Shape::Circle(*a, a.distance(cur))],
            (Tool::Arc, [a]) => vec![Shape::Line(*a, cur)],
            (Tool::Arc, [a, b]) => arc3(*a, cur, *b).map_or_else(|| vec![Shape::Line(*a, *b)], |s| vec![s]),
            _ => Vec::new(),
        }
    }
}

fn rectangle(p: DVec2, q: DVec2) -> Vec<Shape> {
    let k = [p, DVec2::new(q.x, p.y), q, DVec2::new(p.x, q.y)];
    (0..4).map(|i| Shape::Line(k[i], k[(i + 1) % 4])).collect()
}

/// The arc from `a` through `m` to `b`, counter-clockwise as Sketcher keeps it.
pub fn arc3(a: DVec2, m: DVec2, b: DVec2) -> Option<Shape> {
    let d = 2.0 * (a.x * (m.y - b.y) + m.x * (b.y - a.y) + b.x * (a.y - m.y));
    if d.abs() < 1e-12 {
        return None;
    }
    let (a2, m2, b2) = (a.length_squared(), m.length_squared(), b.length_squared());
    let c = DVec2::new(a2 * (m.y - b.y) + m2 * (b.y - a.y) + b2 * (a.y - m.y), a2 * (b.x - m.x) + m2 * (a.x - b.x) + b2 * (m.x - a.x)) / d;
    let ang = |p: DVec2| (p - c).y.atan2((p - c).x);
    let tau = std::f64::consts::TAU;
    let (mut s, mut e) = (ang(a), ang(b));
    if d < 0.0 {
        std::mem::swap(&mut s, &mut e); // drawn clockwise
    }
    while e <= s {
        e += tau;
    }
    Some(Shape::Arc(c, c.distance(a), s, e))
}

/// The element whose drawn line passes within `reach` points of `pos`, nearest first.
pub fn element_at(sketch: &Sketch, project: &dyn Fn(DVec2) -> Option<Pos2>, pos: Pos2, reach: f32) -> Option<usize> {
    let dist = |e: &Element| {
        let pts: Vec<Option<Pos2>> = e.shape.polyline().into_iter().map(project).collect();
        if let [Some(p)] = pts.as_slice() {
            return (*p - pos).length();
        }
        pts.windows(2).filter_map(|w| Some(crate::picking::segment_distance(pos, w[0]?, w[1]?))).fold(f32::INFINITY, f32::min)
    };
    sketch.elements.iter().map(|e| (e.index, dist(e))).filter(|(_, d)| *d <= reach).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i)
}

// ---- constraints and dimensions -----------------------------------------------------------

/// What a click picks for a constraint or a dimension: a point (of an element, or the
/// origin) before the element it lies on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    Point(Target),
    Element(usize),
}

/// How near the pointer a point is picked, in points; nearer than an element's line.
pub const POINT_REACH: f32 = 8.0;

/// The point or element under `pos`: a point within `POINT_REACH`, else an element within `reach`.
pub fn pick_at(sketch: &Sketch, project: &dyn Fn(DVec2) -> Option<Pos2>, pos: Pos2, reach: f32) -> Option<Pick> {
    let mut best: Option<(f32, Target)> = None;
    let mut consider = |t: Target, p: DVec2| {
        if let Some(s) = project(p) {
            let d = (s - pos).length();
            if d <= POINT_REACH && best.is_none_or(|(b, _)| d < b) {
                best = Some((d, t));
            }
        }
    };
    // An element's own point first: on a tie with the origin under it, the element's wins.
    for e in &sketch.elements {
        for (at, p) in e.shape.points() {
            consider(Target::Point(e.index, at), p);
        }
    }
    consider(Target::Origin, DVec2::ZERO);
    if let Some((_, t)) = best {
        return Some(Pick::Point(t));
    }
    element_at(sketch, project, pos, reach).map(Pick::Element)
}

/// The constraint tools, as Fusion's CONSTRAINTS panel has them, and the dimension tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constrain {
    Dimension,
    Coincident,
    HorizontalVertical,
    Parallel,
    Perpendicular,
    Tangent,
    Equal,
    Midpoint,
    Concentric,
    Fix,
}

impl Constrain {
    pub const ALL: [Constrain; 9] = [
        Constrain::Coincident,
        Constrain::HorizontalVertical,
        Constrain::Parallel,
        Constrain::Perpendicular,
        Constrain::Tangent,
        Constrain::Equal,
        Constrain::Midpoint,
        Constrain::Concentric,
        Constrain::Fix,
    ];

    /// The short mark on its button and next to the elements it holds.
    pub fn glyph(self) -> &'static str {
        match self {
            Constrain::Dimension => "<->",
            Constrain::Coincident => "•",
            Constrain::HorizontalVertical => "H/V",
            Constrain::Parallel => "//",
            Constrain::Perpendicular => "_|_",
            Constrain::Tangent => "T",
            Constrain::Equal => "=",
            Constrain::Midpoint => "M",
            Constrain::Concentric => "(o)",
            Constrain::Fix => "F",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Constrain::Dimension => "Dimension (D): click a line for its length, a circle or arc for its size, two points, a point and a line, or two lines for their angle; type the value, Enter",
            Constrain::Coincident => "Coincident: two points together, or a point onto a line or curve",
            Constrain::HorizontalVertical => "Horizontal/Vertical: a line, or two points, level or plumb, whichever it is nearer",
            Constrain::Parallel => "Parallel: two lines",
            Constrain::Perpendicular => "Perpendicular: two lines",
            Constrain::Tangent => "Tangent: a line and a circle or arc, or two curves",
            Constrain::Equal => "Equal: two lines of one length, or two circles or arcs of one size",
            Constrain::Midpoint => "Midpoint: a point onto the middle of a line",
            Constrain::Concentric => "Concentric: two circles or arcs about one centre",
            Constrain::Fix => "Fix: an element stays where it is",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Constrain::Dimension => "Dimension",
            Constrain::Coincident => "Coincident",
            Constrain::HorizontalVertical => "Horizontal/Vertical",
            Constrain::Parallel => "Parallel",
            Constrain::Perpendicular => "Perpendicular",
            Constrain::Tangent => "Tangent",
            Constrain::Equal => "Equal",
            Constrain::Midpoint => "Midpoint",
            Constrain::Concentric => "Concentric",
            Constrain::Fix => "Fix",
        }
    }
}

impl Sketch {
    pub fn element(&self, i: usize) -> Option<&Element> {
        self.elements.iter().find(|e| e.index == i)
    }

    fn is_line(&self, i: usize) -> bool {
        matches!(self.element(i).map(|e| &e.shape), Some(Shape::Line(..)))
    }

    fn is_round(&self, i: usize) -> bool {
        matches!(self.element(i).map(|e| &e.shape), Some(Shape::Circle(..) | Shape::Arc(..)))
    }

    /// Where a point is: of an element, or the origin.
    pub fn point(&self, t: Target) -> Option<DVec2> {
        match t {
            Target::Origin => Some(DVec2::ZERO),
            Target::Point(i, at) => self.element(i)?.shape.points().into_iter().find(|(a, _)| *a == at).map(|(_, p)| p),
        }
    }
}

fn pick_ref(p: Pick) -> Value {
    match p {
        Pick::Point(t) => target_ref(t),
        Pick::Element(i) => json!({ "geometry": i }),
    }
}

/// The constraint a tool makes of the picks so far: Ok(Some) when they are enough, Ok(None)
/// while it needs more, Err when they do not fit the tool (the picks start again).
pub fn constraint_for(tool: Constrain, sketch: &Sketch, picks: &[Pick]) -> Result<Option<Value>, String> {
    use Pick::{Element as E, Point as P};
    let c = |t: &str, refs: &[Pick]| Ok(Some(json!({ "type": t, "refs": refs.iter().map(|p| pick_ref(*p)).collect::<Vec<_>>() })));
    let wrong = || Err(format!("{}: {}", tool.name(), tool.hint().split_once(": ").map_or("", |x| x.1)));
    match (tool, picks) {
        (_, []) => Ok(None),
        (Constrain::Fix, [E(_)]) => c("fix", picks),
        (Constrain::HorizontalVertical, [E(i)]) if sketch.is_line(*i) => {
            let Some(Shape::Line(a, b)) = sketch.element(*i).map(|e| &e.shape) else { return wrong() };
            let d = *b - *a;
            c(if d.x.abs() >= d.y.abs() { "horizontal" } else { "vertical" }, picks)
        }
        (Constrain::HorizontalVertical | Constrain::Coincident, [P(_)]) => Ok(None),
        (Constrain::Coincident, [E(_)]) => Ok(None),
        (Constrain::HorizontalVertical, [P(a), P(b)]) => {
            let (Some(p), Some(q)) = (sketch.point(*a), sketch.point(*b)) else { return wrong() };
            let d = q - p;
            c(if d.x.abs() >= d.y.abs() { "horizontal" } else { "vertical" }, picks)
        }
        (Constrain::Coincident, [P(_), P(_)]) => c("coincident", picks),
        (Constrain::Coincident, [P(_), E(_)]) => c("point_on_object", picks),
        (Constrain::Coincident, [E(_), P(_)]) => c("point_on_object", &[picks[1], picks[0]]),
        (Constrain::Midpoint, [P(_)]) => Ok(None),
        (Constrain::Midpoint, [E(i)]) if sketch.is_line(*i) => Ok(None),
        (Constrain::Midpoint, [P(_), E(i)]) if sketch.is_line(*i) => c("midpoint", picks),
        (Constrain::Midpoint, [E(i), P(_)]) if sketch.is_line(*i) => c("midpoint", &[picks[1], picks[0]]),
        (Constrain::Parallel | Constrain::Perpendicular, [E(i)]) if sketch.is_line(*i) => Ok(None),
        (Constrain::Parallel, [E(i), E(j)]) if sketch.is_line(*i) && sketch.is_line(*j) => c("parallel", picks),
        (Constrain::Perpendicular, [E(i), E(j)]) if sketch.is_line(*i) && sketch.is_line(*j) => c("perpendicular", picks),
        (Constrain::Concentric, [E(i)]) if sketch.is_round(*i) => Ok(None),
        (Constrain::Concentric, [E(i), E(j)]) if sketch.is_round(*i) && sketch.is_round(*j) => c("concentric", picks),
        (Constrain::Equal | Constrain::Tangent, [E(_)]) => Ok(None),
        (Constrain::Equal, [E(i), E(j)]) if sketch.is_line(*i) == sketch.is_line(*j) => c("equal", picks),
        (Constrain::Tangent, [E(i), E(j)]) if !(sketch.is_line(*i) && sketch.is_line(*j)) => c("tangent", picks),
        _ => wrong(),
    }
}

/// A dimension of the picks: the constraint without its value, the value as it measures now
/// (mm or degrees), and what the value box says it is. None while more picks are needed.
pub fn dimension_for(sketch: &Sketch, picks: &[Pick]) -> Result<Option<(Value, f64, &'static str)>, String> {
    use Pick::{Element as E, Point as P};
    let refs = |ps: &[Pick]| ps.iter().map(|p| pick_ref(*p)).collect::<Vec<_>>();
    let shape = |i: usize| sketch.element(i).map(|e| e.shape.clone());
    match picks {
        [] | [P(_)] => Ok(None),
        [E(i)] => match shape(*i) {
            Some(Shape::Line(a, b)) => Ok(Some((json!({ "type": "distance", "refs": refs(picks) }), a.distance(b), "length"))),
            Some(Shape::Circle(_, r)) => Ok(Some((json!({ "type": "diameter", "refs": refs(picks) }), 2.0 * r, "diameter"))),
            Some(Shape::Arc(_, r, ..)) => Ok(Some((json!({ "type": "radius", "refs": refs(picks) }), r, "radius"))),
            _ => Err("Dimension: pick a line, a circle or an arc, or points".into()),
        },
        [P(a), P(b)] => {
            let (Some(p), Some(q)) = (sketch.point(*a), sketch.point(*b)) else { return Err("Dimension: no such point".into()) };
            Ok(Some((json!({ "type": "distance", "refs": refs(picks) }), p.distance(q), "distance")))
        }
        [P(a), E(i)] | [E(i), P(a)] => match (sketch.point(*a), shape(*i)) {
            (Some(p), Some(Shape::Line(s, e))) => {
                let d = (e - s).normalize_or_zero();
                let dist = (p - s).perp_dot(d).abs();
                Ok(Some((json!({ "type": "distance", "refs": [pick_ref(Pick::Point(*a)), { "geometry": i }] }), dist, "distance")))
            }
            _ => Err("Dimension: a point and a line".into()),
        },
        [E(i), E(j)] => match (shape(*i), shape(*j)) {
            (Some(Shape::Line(a, b)), Some(Shape::Line(c, d))) => {
                let (u, v) = (b - a, d - c);
                if u.perp_dot(v).abs() < 1e-9 * u.length() * v.length() {
                    // Parallel: the distance between them, from the second's start to the first.
                    let dist = (c - a).perp_dot(u.normalize_or_zero()).abs();
                    Ok(Some((json!({ "type": "distance", "refs": [{ "geometry": j, "at": "start" }, { "geometry": i }] }), dist, "distance")))
                } else {
                    let angle = u.perp_dot(v).atan2(u.dot(v)).to_degrees();
                    Ok(Some((json!({ "type": "angle", "refs": refs(picks) }), angle, "angle")))
                }
            }
            _ => Err("Dimension: two lines, for their angle or distance".into()),
        },
        _ => Err("Dimension: pick a line, a circle, an arc, two points, a point and a line, or two lines".into()),
    }
}

/// Where a reference of a constraint is: a point, or the middle of an element.
fn ref_place(sketch: &Sketch, r: &Value) -> Option<DVec2> {
    if r["origin"].as_bool() == Some(true) {
        return Some(DVec2::ZERO);
    }
    let i = r["geometry"].as_u64()? as usize;
    let e = sketch.element(i)?;
    if let Some(at) = r["at"].as_str() {
        let at = match at {
            "start" => At::Start,
            "end" => At::End,
            _ => At::Center,
        };
        return sketch.point(Target::Point(i, at));
    }
    Some(match &e.shape {
        Shape::Line(a, b) => (*a + *b) * 0.5,
        Shape::Circle(c, r) => *c + DVec2::new(r * std::f64::consts::FRAC_1_SQRT_2, r * std::f64::consts::FRAC_1_SQRT_2),
        Shape::Arc(c, r, a0, a1) => *c + *r * DVec2::new(((a0 + a1) / 2.0).cos(), ((a0 + a1) / 2.0).sin()),
        Shape::Point(p) => *p,
        Shape::Curve(pts) => *pts.get(pts.len() / 2)?,
    })
}

/// How a dimension reads, and where its text goes in the sketch: by the middle of what it
/// measures. Reference dimensions are in parentheses.
pub fn dimension_label(sketch: &Sketch, c: &Constraint) -> Option<(DVec2, String)> {
    let v = c.value?;
    let places: Vec<DVec2> = c.refs.iter().filter_map(|r| ref_place(sketch, r)).collect();
    let at = match places.as_slice() {
        [] => return None,
        [p] => *p,
        [p, q, ..] => (*p + *q) * 0.5,
    };
    let num = {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let text = match c.kind.as_str() {
        "Diameter" => format!("Ø{num}"),
        "Radius" => format!("R{num}"),
        "Angle" => format!("{num}°"),
        _ => num,
    };
    Some((at, if c.driving { text } else { format!("({text})") }))
}

/// The marks of a geometric constraint, each by what it holds: (place, mark).
pub fn constraint_marks(sketch: &Sketch, c: &Constraint) -> Vec<(DVec2, &'static str)> {
    let mark = match c.kind.as_str() {
        "Horizontal" => "H",
        "Vertical" => "V",
        "Parallel" => "//",
        "Perpendicular" => "_|_",
        "Tangent" => "T",
        "Equal" => "=",
        "Block" => "F",
        "PointOnObject" => "on",
        "Symmetric" => "M",
        _ => return Vec::new(),
    };
    // Midpoint is a point symmetric about a line's two ends: mark it at the point.
    let refs: Vec<&Value> = if c.kind == "Symmetric" { c.refs.iter().skip(2).collect() } else { c.refs.iter().collect() };
    refs.into_iter().filter_map(|r| ref_place(sketch, r)).map(|p| (p, mark)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch(elements: Vec<Shape>) -> Sketch {
        Sketch {
            obj: "Sketch".into(),
            label: "Sketch".into(),
            plane: Plane { origin: Vec3::new(0.0, 0.0, 6.0), normal: Vec3::Z, x: Vec3::X, y: Vec3::Y },
            count: elements.len(),
            elements: elements.into_iter().enumerate().map(|(index, shape)| Element { index, shape, construction: false }).collect(),
            constraints: Vec::new(),
            profiles: Vec::new(),
            dof: None,
            fully_constrained: false,
        }
    }

    #[test]
    fn a_reply_reads_as_a_sketch() {
        let v = json!({
            "obj": "Sketch", "label": "Sketch", "dof": 4, "fully_constrained": false,
            "plane": { "origin": [0.0, 0.0, 6.0], "normal": [0.0, 0.0, 1.0], "x_dir": [1.0, 0.0, 0.0] },
            "geometry": [
                { "index": 0, "type": "LineSegment", "start": [0.0, 0.0], "end": [10.0, 0.0], "construction": false },
                { "index": 1, "type": "ArcOfCircle", "center": [0.0, 0.0], "radius": 5.0, "start_angle": 0.0, "end_angle": 90.0, "construction": true }
            ],
            "constraints": [{ "index": 0, "type": "DistanceX", "refs": [{ "geometry": 0 }], "value": 10.0, "property": "Constraints.Width" }],
            "profiles": [{ "mesh": { "points": [0.0, 0.0, 1.0, 0.0, 0.0, 1.0], "triangles": [0, 1, 2] } }]
        });
        let s = Sketch::parse(&v).unwrap();
        assert_eq!(s.plane.y, Vec3::Y);
        assert_eq!(s.elements[0].shape, Shape::Line(DVec2::ZERO, DVec2::new(10.0, 0.0)));
        assert!(s.elements[1].construction);
        let ends = s.elements[1].shape.points();
        assert!((ends[1].1 - DVec2::new(0.0, 5.0)).length() < 1e-9, "the arc ends at 90 degrees");
        assert_eq!(s.constraints[0].property.as_deref(), Some("Constraints.Width"));
        assert_eq!(s.profiles[0].triangles, vec![[0, 1, 2]]);
        assert_eq!(s.plane.local(s.plane.world(DVec2::new(3.0, 4.0))), DVec2::new(3.0, 4.0));
    }

    #[test]
    fn the_line_of_sight_meets_the_plane_under_the_pointer() {
        let plane = Plane { origin: Vec3::new(0.0, 0.0, 6.0), normal: Vec3::Z, x: Vec3::X, y: Vec3::Y };
        let mut cam = crate::camera::Camera::default();
        cam.set_view(crate::camera::View::Top);
        cam.fit(Vec3::new(-50.0, -50.0, 0.0), Vec3::new(50.0, 50.0, 10.0));
        let rect = Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2(800.0, 600.0));
        cam.aspect = 800.0 / 600.0;
        let vp = cam.view_proj(cam.aspect, (Vec3::splat(-60.0), Vec3::splat(60.0)));
        let target = DVec2::new(12.0, -7.0);
        let pos = crate::picking::projector(vp, rect)(plane.world(target)).unwrap();
        let hit = plane.hit(vp, rect, pos).unwrap();
        assert!(hit.distance(target) < 1e-3, "{hit:?}");
    }

    #[test]
    fn clicks_snap_to_points_then_line_up_then_round() {
        let s = sketch(vec![Shape::Line(DVec2::new(10.0, 10.0), DVec2::new(30.0, 10.0))]);
        let near_end = snap(&s, DVec2::new(30.4, 10.3), 0.1, None, &[]);
        assert_eq!(near_end.target, Some(Target::Point(0, At::End)));
        assert_eq!(near_end.at, DVec2::new(30.0, 10.0));
        assert_eq!(snap(&s, DVec2::new(0.3, -0.2), 0.1, None, &[]).target, Some(Target::Origin));
        let level = snap(&s, DVec2::new(50.0, 20.4), 0.1, Some(DVec2::new(40.0, 20.0)), &[]);
        assert_eq!((level.align, level.at.y), (Some(Align::Horizontal), 20.0));
        let free = snap(&s, DVec2::new(51.234, 63.21), 0.1, None, &[]);
        assert_eq!((free.target, free.align), (None, None));
        assert!((free.at - DVec2::new(51.0, 63.0)).length() < 1e-9, "rounded to a step of 0.5 mm: {:?}", free.at);
    }

    #[test]
    fn a_line_chain_joins_each_segment_to_the_last() {
        let mut d = Drawing::new(Tool::Line);
        assert!(d.click(Snap { at: DVec2::ZERO, target: Some(Target::Origin), align: None }, 0).is_none());
        let first = d.click(Snap { at: DVec2::new(20.0, 0.0), target: None, align: Some(Align::Horizontal) }, 3).unwrap();
        assert_eq!(first.adds, 1);
        assert_eq!(first.args["constraints"][0]["refs"][1], json!({ "origin": true }));
        assert_eq!(first.args["constraints"][1]["type"], "horizontal");
        assert_eq!(d.chain, Some(3));
        let second = d.click(Snap::free(DVec2::new(20.0, 15.0)), 4).unwrap();
        assert_eq!(second.args["add"][0]["start"], json!([20.0, 0.0]));
        assert_eq!(second.args["constraints"][0]["refs"][1], json!({ "geometry": 3, "at": "end" }));
        // Closing on the first point joins it there and ends the chain.
        let third = d.click(Snap { at: DVec2::ZERO, target: Some(Target::Point(3, At::Start)), align: None }, 5).unwrap();
        assert_eq!(third.args["constraints"][1]["refs"][1], json!({ "geometry": 3, "at": "start" }));
        assert_eq!((d.chain, d.clicks.len()), (None, 0));
    }

    #[test]
    fn a_rectangle_joins_the_corner_it_was_started_on() {
        let mut d = Drawing::new(Tool::Rectangle);
        d.click(Snap { at: DVec2::new(30.0, 20.0), target: Some(Target::Point(7, At::End)), align: None }, 9);
        let e = d.click(Snap::free(DVec2::new(10.0, 5.0)), 9).unwrap();
        assert_eq!(e.adds, 4);
        // (30, 20) is the top-right corner: the start of the third line, "top".
        assert_eq!(e.args["constraints"][0]["refs"][0], json!({ "new": 2, "at": "start" }));
        let mut c = Drawing::new(Tool::CenterRectangle);
        c.click(Snap { at: DVec2::ZERO, target: Some(Target::Origin), align: None }, 0);
        let e = c.click(Snap::free(DVec2::new(10.0, 5.0)), 0).unwrap();
        assert_eq!((e.adds, &e.args["constraints"][0]["refs"][0]), (5, &json!({ "new": 4, "at": "start" })));
        assert_eq!(c.preview(DVec2::new(4.0, 2.0)).len(), 0, "done: no preview until the next click");
    }

    #[test]
    fn an_arc_through_three_points_runs_counter_clockwise() {
        let Some(Shape::Arc(c, r, a0, a1)) = arc3(DVec2::new(10.0, 0.0), DVec2::new(0.0, 10.0), DVec2::new(-10.0, 0.0)) else { panic!() };
        assert!(c.length() < 1e-9 && (r - 10.0).abs() < 1e-9 && a0.abs() < 1e-9 && (a1 - std::f64::consts::PI).abs() < 1e-9);
        let Some(Shape::Arc(_, _, b0, b1)) = arc3(DVec2::new(-10.0, 0.0), DVec2::new(0.0, 10.0), DVec2::new(10.0, 0.0)) else { panic!() };
        assert!((b0 - a0).abs() < 1e-9 && (b1 - a1).abs() < 1e-9, "the same arc, drawn the other way");
        assert!(arc3(DVec2::ZERO, DVec2::new(1.0, 1.0), DVec2::new(2.0, 2.0)).is_none());
    }

    #[test]
    fn constraint_tools_take_the_picks_they_fit() {
        let s = sketch(vec![
            Shape::Line(DVec2::ZERO, DVec2::new(10.0, 1.0)),
            Shape::Line(DVec2::new(0.0, 5.0), DVec2::new(1.0, 15.0)),
            Shape::Circle(DVec2::new(20.0, 0.0), 3.0),
            Shape::Arc(DVec2::new(20.0, 0.0), 5.0, 0.0, 1.0),
        ]);
        let (l0, l1, c, a) = (Pick::Element(0), Pick::Element(1), Pick::Element(2), Pick::Element(3));
        let p = |i, at| Pick::Point(Target::Point(i, at));
        assert_eq!(constraint_for(Constrain::HorizontalVertical, &s, &[l0]).unwrap().unwrap()["type"], "horizontal");
        assert_eq!(constraint_for(Constrain::HorizontalVertical, &s, &[l1]).unwrap().unwrap()["type"], "vertical");
        assert_eq!(constraint_for(Constrain::Coincident, &s, &[p(0, At::End)]).unwrap(), None, "waits for a second pick");
        let on = constraint_for(Constrain::Coincident, &s, &[l1, p(0, At::End)]).unwrap().unwrap();
        assert_eq!((on["type"].as_str(), &on["refs"][0]), (Some("point_on_object"), &json!({ "geometry": 0, "at": "end" })));
        assert_eq!(constraint_for(Constrain::Concentric, &s, &[c, a]).unwrap().unwrap()["type"], "concentric");
        assert!(constraint_for(Constrain::Parallel, &s, &[l0, c]).is_err(), "a circle is not parallel to anything");
        assert!(constraint_for(Constrain::Equal, &s, &[l0, c]).is_err(), "a line and a circle are not of one size");
        assert_eq!(constraint_for(Constrain::Tangent, &s, &[l0, c]).unwrap().unwrap()["type"], "tangent");
        assert_eq!(constraint_for(Constrain::Fix, &s, &[a]).unwrap().unwrap(), json!({ "type": "fix", "refs": [{ "geometry": 3 }] }));
    }

    #[test]
    fn a_dimension_measures_what_was_picked() {
        let s = sketch(vec![
            Shape::Line(DVec2::ZERO, DVec2::new(3.0, 4.0)),
            Shape::Circle(DVec2::new(20.0, 0.0), 3.0),
            Shape::Line(DVec2::new(0.0, 2.0), DVec2::new(3.0, 6.0)),
            Shape::Line(DVec2::ZERO, DVec2::new(0.0, 8.0)),
        ]);
        let (c, v, what) = dimension_for(&s, &[Pick::Element(0)]).unwrap().unwrap();
        assert_eq!((c["type"].as_str(), v, what), (Some("distance"), 5.0, "length"));
        assert_eq!(dimension_for(&s, &[Pick::Element(1)]).unwrap().unwrap().1, 6.0, "a circle by its diameter");
        let (c, v, _) = dimension_for(&s, &[Pick::Element(0), Pick::Element(2)]).unwrap().unwrap();
        assert_eq!(c["type"], "distance", "parallel lines: the distance between them");
        assert!((v - 1.2).abs() < 1e-9, "{v}");
        let (c, v, _) = dimension_for(&s, &[Pick::Element(3), Pick::Element(0)]).unwrap().unwrap();
        assert_eq!(c["type"], "angle");
        assert!((v.abs() - 36.8698976).abs() < 1e-6, "{v}");
        let from_origin = dimension_for(&s, &[Pick::Point(Target::Origin), Pick::Point(Target::Point(1, At::Center))]).unwrap().unwrap();
        assert_eq!(from_origin.1, 20.0);
        assert!(dimension_for(&s, &[Pick::Point(Target::Origin)]).unwrap().is_none(), "a point alone waits for a second");
    }

    #[test]
    fn dimensions_and_marks_sit_by_what_they_hold() {
        let s = sketch(vec![Shape::Line(DVec2::ZERO, DVec2::new(10.0, 0.0)), Shape::Circle(DVec2::new(20.0, 0.0), 3.0)]);
        let dim = Constraint { index: 0, kind: "Diameter".into(), refs: vec![json!({ "geometry": 1 })], value: Some(6.0), property: Some("Constraints.0".into()), driving: true, expression: None };
        let (_, text) = dimension_label(&s, &dim).unwrap();
        assert_eq!(text, "Ø6");
        let len = Constraint { index: 1, kind: "Distance".into(), refs: vec![json!({ "geometry": 0 })], value: Some(12.5), property: None, driving: false, expression: None };
        assert_eq!(dimension_label(&s, &len).unwrap(), (DVec2::new(5.0, 0.0), "(12.5)".into()));
        let h = Constraint { index: 2, kind: "Horizontal".into(), refs: vec![json!({ "geometry": 0 })], value: None, property: None, driving: true, expression: None };
        assert_eq!(constraint_marks(&s, &h), vec![(DVec2::new(5.0, 0.0), "H")]);
        let project = |p: DVec2| Some(Pos2::new(p.x as f32 * 10.0, p.y as f32 * 10.0));
        assert_eq!(pick_at(&s, &project, Pos2::new(98.0, 3.0), 6.0), Some(Pick::Point(Target::Point(0, At::End))));
        assert_eq!(pick_at(&s, &project, Pos2::new(50.0, 3.0), 6.0), Some(Pick::Element(0)));
    }

    #[test]
    fn the_element_under_the_pointer_is_found_on_screen() {
        let s = sketch(vec![Shape::Line(DVec2::ZERO, DVec2::new(100.0, 0.0)), Shape::Circle(DVec2::new(50.0, 50.0), 10.0)]);
        let project = |p: DVec2| Some(Pos2::new(p.x as f32, 200.0 - p.y as f32));
        assert_eq!(element_at(&s, &project, Pos2::new(40.0, 203.0), 6.0), Some(0));
        assert_eq!(element_at(&s, &project, Pos2::new(60.0, 150.0), 6.0), Some(1));
        assert_eq!(element_at(&s, &project, Pos2::new(50.0, 150.0), 6.0), None, "the circle's middle is empty");
    }
}
