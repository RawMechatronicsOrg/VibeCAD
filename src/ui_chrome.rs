//! The window's chrome around the 3D view: a ribbon of tabs on top, panels floating
//! translucent over the view, and a view cube for picking standard views and orbiting.

use eframe::egui::{self, Align2, Color32, Id, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use glam::Vec3;

use crate::camera::Camera;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    File,
    Edit,
    View,
}

/// What a click in the ribbon asks the app to do.
#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    Open,
    Save,
    SaveAs,
    Export,
    Restart,
    Quit,
    Undo,
    Redo,
    Fit,
    /// Look along minus this direction (the camera sits on its side).
    ViewFrom(Vec3),
    ToggleOrtho,
    ToggleEdges,
    ToggleLog,
    ToggleGrid,
    /// Between the document's colours and a palette colour per default-grey object.
    TogglePalette,
    /// In pixels.
    EdgeWidth(f32),
}

/// What the ribbon needs to know to enable and mark its buttons.
pub struct RibbonState {
    pub has_doc: bool,
    pub can_open: bool,
    pub undo: u64,
    pub redo: u64,
    pub ortho: bool,
    pub edges: bool,
    pub log: bool,
    pub grid: bool,
    pub palette: bool,
    pub edge_width: f32,
}

pub fn ribbon(ui: &mut egui::Ui, tab: &mut Tab, s: &RibbonState, status: impl FnOnce(&mut egui::Ui)) -> Vec<Action> {
    let mut out = Vec::new();
    ui.horizontal(|ui| {
        ui.label(RichText::new("VibeCAD").strong());
        ui.add_space(12.0);
        for (t, name) in [(Tab::File, "FILE"), (Tab::Edit, "EDIT"), (Tab::View, "VIEW")] {
            if ui.selectable_label(*tab == t, RichText::new(name).small().strong()).clicked() {
                *tab = t;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), status);
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        match tab {
            Tab::File => {
                group(ui, "DOCUMENT", |ui| {
                    button(ui, &mut out, Action::Open, Icon::Open, "Open", s.can_open, false, "Ctrl+O");
                    button(ui, &mut out, Action::Save, Icon::Save, "Save", s.has_doc, false, "Ctrl+S");
                    button(ui, &mut out, Action::SaveAs, Icon::SaveAs, "Save as", s.has_doc, false, "Ctrl+Shift+S");
                });
                group(ui, "EXCHANGE", |ui| {
                    button(ui, &mut out, Action::Export, Icon::Export, "Export", s.has_doc, false, "STEP, STL, 3MF, OBJ (Ctrl+E)");
                });
                group(ui, "FREECAD", |ui| {
                    button(ui, &mut out, Action::Restart, Icon::Restart, "Restart", true, false, "Start a new FreeCAD process and reopen the file");
                    button(ui, &mut out, Action::Quit, Icon::Quit, "Quit", true, false, "");
                });
            }
            Tab::Edit => {
                group(ui, "HISTORY", |ui| {
                    let undo = if s.undo > 0 { format!("Undo ({})", s.undo) } else { "Undo".into() };
                    let redo = if s.redo > 0 { format!("Redo ({})", s.redo) } else { "Redo".into() };
                    button(ui, &mut out, Action::Undo, Icon::Undo, &undo, s.undo > 0, false, "Ctrl+Z");
                    button(ui, &mut out, Action::Redo, Icon::Redo, &redo, s.redo > 0, false, "Ctrl+Y");
                });
            }
            Tab::View => {
                group(ui, "NAVIGATE", |ui| {
                    button(ui, &mut out, Action::Fit, Icon::Fit, "Fit all", true, false, "F, or double-click the view");
                });
                group(ui, "STANDARD VIEWS", |ui| {
                    for (label, key, dir) in [
                        ("Iso", "0", Vec3::new(1.0, -1.0, 1.0)),
                        ("Front", "1", Vec3::NEG_Y),
                        ("Top", "2", Vec3::Z),
                        ("Right", "3", Vec3::X),
                    ] {
                        button(ui, &mut out, Action::ViewFrom(dir), Icon::ViewFrom(dir), label, true, false, key);
                    }
                });
                group(ui, "DISPLAY", |ui| {
                    button(ui, &mut out, Action::ToggleOrtho, Icon::Ortho, "Ortho", true, s.ortho, "Orthographic projection (O); off: perspective");
                    button(ui, &mut out, Action::ToggleEdges, Icon::Edges, "Edges", true, s.edges, "");
                    button(
                        ui,
                        &mut out,
                        Action::TogglePalette,
                        Icon::Palette,
                        "Palette",
                        true,
                        s.palette,
                        "On: objects with FreeCAD's default grey get a colour each. Off: the document's colours only",
                    );
                    button(ui, &mut out, Action::ToggleGrid, Icon::Grid, "Grid", true, s.grid, "The ground grid with its scale");
                    button(ui, &mut out, Action::ToggleLog, Icon::Log, "FreeCAD log", true, s.log, "");
                });
                group(ui, "EDGE WIDTH", |ui| {
                    ui.vertical(|ui| {
                        ui.add_space(12.0);
                        let mut w = s.edge_width;
                        let slider = egui::Slider::new(&mut w, 1.0..=4.0).step_by(0.25).fixed_decimals(2).suffix(" px");
                        if ui.add_sized(vec2(130.0, 20.0), slider).on_hover_text("Width of the edges in the 3D view, in pixels").changed() {
                            out.push(Action::EdgeWidth(w));
                        }
                    });
                });
            }
        }
    });
    out
}

/// A row of buttons with a caption under it, as wide as the buttons.
fn group(ui: &mut egui::Ui, caption: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        let row = ui.horizontal(add).response.rect;
        ui.allocate_ui_with_layout(vec2(row.width(), 12.0), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.label(RichText::new(caption).size(9.0).weak());
        });
    });
    ui.separator();
}

/// Ribbon icons, drawn with lines so they do not depend on what the fonts contain.
#[derive(Clone, Copy)]
enum Icon {
    Open,
    Save,
    SaveAs,
    Export,
    Restart,
    Quit,
    Undo,
    Redo,
    Fit,
    ViewFrom(Vec3),
    Ortho,
    Edges,
    Palette,
    Log,
    Grid,
}

#[allow(clippy::too_many_arguments)]
fn button(ui: &mut egui::Ui, out: &mut Vec<Action>, action: Action, icon: Icon, label: &str, enabled: bool, on: bool, hint: &str) {
    let size = vec2(64.0, 46.0);
    let (rect, mut r) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let vis = ui.style().interact_selectable(&r, on);
        let painter = ui.painter();
        if on || r.hovered() && enabled {
            painter.rect(rect, 4.0, vis.weak_bg_fill, vis.bg_stroke, egui::StrokeKind::Inside);
        }
        let color = if enabled { ui.visuals().text_color() } else { ui.visuals().weak_text_color().gamma_multiply(0.6) };
        let icon_rect = Rect::from_center_size(pos2(rect.center().x, rect.top() + 15.0), Vec2::splat(20.0));
        paint_icon(painter, icon_rect, icon, color);
        painter.text(pos2(rect.center().x, rect.bottom() - 9.0), Align2::CENTER_CENTER, label, egui::FontId::proportional(11.0), color);
    }
    if !hint.is_empty() {
        r = r.on_hover_text(hint);
    }
    if enabled && r.clicked() {
        out.push(action);
    }
}

fn paint_icon(p: &egui::Painter, r: Rect, icon: Icon, c: Color32) {
    let s = Stroke::new(1.5, c);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let poly = |pts: &[(f32, f32)], closed: bool| {
        let pts: Vec<Pos2> = pts.iter().map(|&(x, y)| at(x, y)).collect();
        if closed {
            p.add(egui::Shape::closed_line(pts, s));
        } else {
            p.add(egui::Shape::line(pts, s));
        }
    };
    let arc = |cx: f32, cy: f32, rad: f32, from: f32, to: f32| -> Vec<Pos2> {
        (0..=16).map(|i| {
            let t = from + (to - from) * i as f32 / 16.0;
            at(cx + rad * t.cos(), cy + rad * t.sin())
        }).collect()
    };
    match icon {
        Icon::Open => {
            poly(&[(0.05, 0.25), (0.4, 0.25), (0.5, 0.35), (0.95, 0.35), (0.95, 0.85), (0.05, 0.85)], true);
            poly(&[(0.05, 0.85), (0.2, 0.5), (1.0, 0.5), (0.95, 0.85)], false);
        }
        Icon::Save | Icon::SaveAs => {
            poly(&[(0.1, 0.1), (0.75, 0.1), (0.9, 0.25), (0.9, 0.9), (0.1, 0.9)], true);
            poly(&[(0.28, 0.1), (0.28, 0.35), (0.65, 0.35), (0.65, 0.1)], false);
            p.rect_stroke(Rect::from_min_max(at(0.25, 0.55), at(0.75, 0.9)), 0.0, s, egui::StrokeKind::Middle);
            if let Icon::SaveAs = icon {
                p.line_segment([at(0.55, 1.0), at(1.0, 0.55)], Stroke::new(2.0, c));
            }
        }
        Icon::Export => {
            poly(&[(0.3, 0.4), (0.1, 0.4), (0.1, 0.9), (0.9, 0.9), (0.9, 0.4), (0.7, 0.4)], false);
            p.line_segment([at(0.5, 0.7), at(0.5, 0.05)], s);
            poly(&[(0.3, 0.25), (0.5, 0.05), (0.7, 0.25)], false);
        }
        Icon::Restart => {
            let pts = arc(0.5, 0.5, 0.38, -1.2, 4.2);
            let tip = *pts.last().unwrap();
            p.add(egui::Shape::line(pts, s));
            p.add(egui::Shape::line(vec![tip + vec2(-5.0, -1.0), tip, tip + vec2(1.0, -6.0)], s));
        }
        Icon::Quit => {
            p.add(egui::Shape::line(arc(0.5, 0.55, 0.36, -1.0, 4.14), s));
            p.line_segment([at(0.5, 0.05), at(0.5, 0.5)], s);
        }
        Icon::Undo | Icon::Redo => {
            let flip = matches!(icon, Icon::Redo);
            let fx = |x: f32| if flip { 1.0 - x } else { x };
            let pts: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let t = std::f32::consts::PI * (1.0 - i as f32 / 16.0);
                    at(fx(0.55 + 0.33 * t.cos()), 0.62 - 0.33 * t.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, s));
            poly(&[(fx(0.05), 0.45), (fx(0.22), 0.65), (fx(0.42), 0.48)], false);
        }
        Icon::Fit => {
            for (x, y, dx, dy) in [(0.05, 0.05, 1.0, 1.0), (0.95, 0.05, -1.0, 1.0), (0.05, 0.95, 1.0, -1.0), (0.95, 0.95, -1.0, -1.0)] {
                poly(&[(x, y + 0.3 * dy), (x, y), (x + 0.3 * dx, y)], false);
            }
            p.rect_stroke(Rect::from_min_max(at(0.32, 0.32), at(0.68, 0.68)), 1.0, s, egui::StrokeKind::Middle);
        }
        Icon::ViewFrom(dir) => {
            // A small cube in the isometric view, with the face the view looks at filled.
            let iso = [(0.5, 0.05), (0.92, 0.27), (0.92, 0.73), (0.5, 0.95), (0.08, 0.73), (0.08, 0.27)];
            let centre = (0.5, 0.5);
            let faces: [(Vec3, [(f32, f32); 4]); 3] = [
                (Vec3::Z, [iso[0], iso[1], centre, iso[5]]),
                (Vec3::NEG_Y, [iso[5], centre, iso[3], iso[4]]),
                (Vec3::X, [centre, iso[1], iso[2], iso[3]]),
            ];
            for (n, quad) in faces {
                let pts: Vec<Pos2> = quad.iter().map(|&(x, y)| at(x, y)).collect();
                let lit = n.dot(dir.normalize_or(Vec3::Z)) > 0.3;
                let fill = if lit { c.gamma_multiply(0.55) } else { Color32::TRANSPARENT };
                p.add(egui::Shape::convex_polygon(pts, fill, s));
            }
        }
        Icon::Ortho => {
            poly(&[(0.25, 0.1), (0.95, 0.1), (0.75, 0.9), (0.05, 0.9)], true);
            for t in [0.37, 0.63] {
                p.line_segment([at(0.25 + 0.7 * t, 0.1), at(0.05 + 0.7 * t, 0.9)], Stroke::new(1.0, c));
                p.line_segment([at(0.25 - 0.2 * t, 0.1 + 0.8 * t), at(0.95 - 0.2 * t, 0.1 + 0.8 * t)], Stroke::new(1.0, c));
            }
        }
        Icon::Edges => {
            poly(&[(0.1, 0.3), (0.6, 0.3), (0.6, 0.9), (0.1, 0.9)], true);
            poly(&[(0.1, 0.3), (0.4, 0.05), (0.9, 0.05), (0.6, 0.3)], false);
            poly(&[(0.9, 0.05), (0.9, 0.65), (0.6, 0.9)], false);
        }
        Icon::Palette => {
            for (x, y, rgb) in [(0.32, 0.38, [158, 188, 222]), (0.68, 0.38, [228, 178, 156]), (0.5, 0.7, [166, 206, 166])] {
                p.circle(at(x, y), r.width() * 0.22, Color32::from_rgb(rgb[0], rgb[1], rgb[2]), s);
            }
        }
        Icon::Grid => {
            // The ground grid seen from above at an angle: a parallelogram with its lattice.
            let quad = [(0.3, 0.15), (1.0, 0.15), (0.7, 0.85), (0.0, 0.85)];
            poly(&quad, true);
            for t in [0.33f32, 0.67] {
                let lerp = |a: (f32, f32), b: (f32, f32)| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                let (a, b) = (lerp(quad[0], quad[1]), lerp(quad[3], quad[2]));
                p.line_segment([at(a.0, a.1), at(b.0, b.1)], Stroke::new(1.0, c));
                let (a, b) = (lerp(quad[0], quad[3]), lerp(quad[1], quad[2]));
                p.line_segment([at(a.0, a.1), at(b.0, b.1)], Stroke::new(1.0, c));
            }
        }
        Icon::Log => {
            poly(&[(0.15, 0.05), (0.85, 0.05), (0.85, 0.95), (0.15, 0.95)], true);
            for y in [0.3, 0.5, 0.7] {
                p.line_segment([at(0.3, y), at(0.7, y)], Stroke::new(1.0, c));
            }
        }
    }
}

/// A small triangle pointing right (folded) or down (open).
fn paint_fold(p: &egui::Painter, r: Rect, open: bool, c: Color32) {
    let m = r.center();
    let pts = if open {
        vec![m + vec2(-4.0, -2.0), m + vec2(4.0, -2.0), m + vec2(0.0, 3.0)]
    } else {
        vec![m + vec2(-2.0, -4.0), m + vec2(3.0, 0.0), m + vec2(-2.0, 4.0)]
    };
    p.add(egui::Shape::convex_polygon(pts, c, Stroke::NONE));
}

/// A house outline: back to the home view.
fn paint_home(p: &egui::Painter, r: Rect, c: Color32) {
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let s = Stroke::new(1.5, c);
    p.add(egui::Shape::closed_line(vec![at(0.5, 0.1), at(0.9, 0.5), at(0.78, 0.5), at(0.78, 0.9), at(0.22, 0.9), at(0.22, 0.5), at(0.1, 0.5)], s));
}

/// A translucent panel floating over the 3D view. It is an egui window without a title bar:
/// - drag its header to move it (kept inside `bounds`), double-click the header to put it back;
/// - click the triangle or the title to fold it to its header;
/// - drag its edges or corner to resize it while it is open.
///
/// Only the header moves it, so a drag that starts on a row of the tree does not.
#[allow(clippy::too_many_arguments)]
pub fn overlay(
    ctx: &egui::Context,
    id: &str,
    bounds: Rect,
    anchor: Pos2,
    pivot: Align2,
    width: f32,
    max_height: f32,
    title: &str,
    add: impl FnOnce(&mut egui::Ui),
) {
    let open_id = Id::unique(("overlay-open", id));
    let pos_id = Id::unique(("overlay-pos", id));
    let rect_id = Id::unique(("overlay-rect", id));
    let mut open = ctx.data_mut(|d| *d.get_persisted_mut_or(open_id, true));
    let moved_to: Option<Pos2> = ctx.data_mut(|d| d.get_persisted(pos_id));
    let v = ctx.global_style().visuals.clone();
    let frame = egui::Frame::new()
        .fill(v.panel_fill.gamma_multiply(0.82))
        .stroke(Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6)))
        .corner_radius(6.0)
        .inner_margin(8.0);
    let mut window = egui::Window::new(title)
        .id(Id::unique(("overlay", id)))
        .title_bar(false)
        .resizable(open)
        .movable(false)
        .frame(frame)
        .default_size(vec2(width, max_height * 0.65))
        .min_size(vec2(160.0, 60.0))
        .max_size(vec2(width * 3.0, max_height.max(120.0)));
    window = match moved_to {
        Some(p) => window.current_pos(p),
        None => window.fixed_pos(anchor).pivot(pivot),
    };
    let mut drag = Vec2::ZERO;
    let mut reset = false;
    let shown = window.show(ctx, |ui| {
        ui.horizontal(|ui| {
            let (r, fold) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::click());
            paint_fold(ui.painter(), r, open, ui.visuals().text_color());
            let label = ui.add(egui::Label::new(RichText::new(title).strong()).selectable(false).sense(Sense::click_and_drag()));
            // The rest of the header is a grip, with dots to show it.
            let (grip, grip_resp) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::click_and_drag());
            paint_grip(ui.painter(), grip, ui.visuals().weak_text_color());
            if fold.clicked() || label.clicked() {
                open = !open;
            }
            for r in [&label, &grip_resp] {
                drag += r.drag_delta();
                reset |= r.double_clicked();
                if r.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                } else if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
            }
        });
        if open {
            ui.separator();
            // Fill the size the user gave the panel; long names are cut, not widening it.
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, add);
        }
    });
    let rect = shown.map(|s| s.response.rect);
    ctx.data_mut(|d| {
        d.insert_persisted(open_id, open);
        if reset {
            d.remove::<Pos2>(pos_id);
        } else if drag != Vec2::ZERO
            && let Some(start) = moved_to.or(d.get_temp::<Rect>(rect_id).map(|r| r.min)).or(rect.map(|r| r.min))
        {
            // Keep the header reachable: at least 60 x 24 points of it stay inside the view.
            let size = rect.map_or(vec2(width, 30.0), |r| r.size());
            let p = start + drag;
            let x = p.x.clamp(bounds.left() - size.x + 60.0, bounds.right() - 60.0);
            let y = p.y.clamp(bounds.top(), bounds.bottom() - 24.0);
            d.insert_persisted(pos_id, pos2(x, y));
        }
        if let Some(r) = rect {
            d.insert_temp(rect_id, r);
        }
    });
}

/// Two rows of dots at the right end of a panel header: it can be dragged.
fn paint_grip(p: &egui::Painter, r: Rect, c: Color32) {
    let right = r.right() - 4.0;
    for i in 0..3 {
        for j in 0..2 {
            p.circle_filled(pos2(right - 5.0 * i as f32, r.center().y - 2.5 + 5.0 * j as f32), 1.2, c);
        }
    }
}

pub enum CubeAction {
    None,
    /// The camera was moved (orbit by drag, or a face, edge or corner clicked).
    Moved,
    /// Back to the isometric view, fitted.
    Home,
}

const FACES: [(Vec3, Vec3, Vec3, &str); 6] = [
    // normal, u axis, v axis (right and up on the face as seen from outside), label
    (Vec3::NEG_Y, Vec3::X, Vec3::Z, "FRONT"),
    (Vec3::Y, Vec3::NEG_X, Vec3::Z, "BACK"),
    (Vec3::X, Vec3::Y, Vec3::Z, "RIGHT"),
    (Vec3::NEG_X, Vec3::NEG_Y, Vec3::Z, "LEFT"),
    (Vec3::Z, Vec3::X, Vec3::Y, "TOP"),
    (Vec3::NEG_Z, Vec3::X, Vec3::NEG_Y, "BOTTOM"),
];

/// Turn the camera so that it sits on the side of `dir` and looks at its target.
pub fn look_from(cam: &mut Camera, dir: Vec3) {
    let d = dir.normalize_or(Vec3::Z);
    cam.pitch = d.z.clamp(-1.0, 1.0).asin().clamp(-1.5707, 1.5707);
    if d.x.abs() > 1e-6 || d.y.abs() > 1e-6 {
        cam.yaw = d.y.atan2(d.x);
    }
}

/// How much of each cube edge is cut off, as a fraction of the half side: the chamfers are the
/// 45-degree views between two faces, the corner triangles the isometric ones.
const CHAMFER: f32 = 0.25;

/// One clickable facet of the chamfered cube: the direction the camera goes to, its outline
/// in cube coordinates (-1..1), and for the six main faces a label with the face's right and up
/// directions as seen from outside, which the label is laid along.
struct Facet {
    dir: Vec3,
    outline: Vec<Vec3>,
    label: Option<(&'static str, Vec3, Vec3)>,
}

/// The 26 facets: 6 faces, 12 edge chamfers, 8 corner triangles.
fn cube_facets() -> Vec<Facet> {
    let k = 1.0 - CHAMFER;
    let mut out: Vec<Facet> = FACES
        .iter()
        .map(|&(n, u, w, label)| Facet {
            dir: n,
            outline: vec![n - u * k - w * k, n + u * k - w * k, n + u * k + w * k, n - u * k + w * k],
            label: Some((label, u, w)),
        })
        .collect();
    for (i, a) in FACES.iter().enumerate() {
        for b in &FACES[i + 1..] {
            let (n1, n2) = (a.0, b.0);
            if n1.dot(n2).abs() > 0.5 {
                continue; // opposite faces share no edge
            }
            let e = n1.cross(n2);
            out.push(Facet {
                dir: n1 + n2,
                outline: vec![n1 + n2 * k + e * k, n1 + n2 * k - e * k, n2 + n1 * k - e * k, n2 + n1 * k + e * k],
                label: None,
            });
        }
    }
    for sx in [-1.0f32, 1.0] {
        for sy in [-1.0f32, 1.0] {
            for sz in [-1.0f32, 1.0] {
                out.push(Facet {
                    dir: Vec3::new(sx, sy, sz),
                    outline: vec![Vec3::new(sx, sy * k, sz * k), Vec3::new(sx * k, sy, sz * k), Vec3::new(sx * k, sy * k, sz)],
                    label: None,
                });
            }
        }
    }
    out
}

/// The view cube in the top-right corner of `view`: shows the orientation; a click on a face,
/// an edge chamfer or a corner turns the camera there; a drag orbits.
pub fn view_cube(ui: &mut egui::Ui, view: Rect, cam: &mut Camera) -> CubeAction {
    let size = 110.0;
    let rect = Rect::from_min_size(pos2(view.right() - size - 10.0, view.top() + 10.0), Vec2::splat(size));
    let resp = ui.interact(rect, ui.scope_id().with("view-cube"), Sense::click_and_drag());
    let home = Rect::from_min_size(rect.left_top() + vec2(-4.0, -2.0), Vec2::splat(20.0));
    let home_resp = ui.interact(home, ui.scope_id().with("view-cube-home"), Sense::click());

    let f = cam.forward();
    let right = f.cross(Vec3::Z).try_normalize().unwrap_or(Vec3::X);
    let up = right.cross(f);
    let center = rect.center();
    let scale = size * 0.27;
    let project = |p: Vec3| pos2(center.x + p.dot(right) * scale, center.y - p.dot(up) * scale);

    let painter = ui.painter_at(rect.expand(4.0));
    let v = ui.visuals();
    let dark = v.dark_mode;
    let base = if dark { Color32::from_rgb(70, 74, 82) } else { Color32::from_rgb(236, 238, 242) };
    let bevel = if dark { Color32::from_rgb(58, 62, 70) } else { Color32::from_rgb(214, 218, 226) };
    let hot = Color32::from_rgb(90, 160, 230);
    let line = if dark { Color32::from_gray(150) } else { Color32::from_gray(120) };
    let hover = resp.hover_pos();

    let mut action = CubeAction::None;
    let mut hit_dir: Option<Vec3> = None;
    // The cube is convex: the facets turned to the eye never overlap, so no depth sorting.
    let mut outline_segments: Vec<[Pos2; 2]> = Vec::new();
    for facet in cube_facets() {
        let facing = -facet.dir.normalize().dot(f);
        // Facets seen almost edge-on are slivers; they add nothing and are hard to hit.
        if facing <= 0.06 {
            continue;
        }
        let outline: Vec<Pos2> = facet.outline.iter().map(|&p| project(p)).collect();
        let mut fill = if facet.label.is_some() { base } else { bevel };
        if hover.is_some_and(|h| inside_convex(h, &outline)) {
            hit_dir = Some(facet.dir);
            fill = fill.lerp_to_gamma(hot, 0.5);
        }
        // Fill, then the outline as separate segments: a polygon stroke joins its corners with
        // mitres, which on a thin sliver shoot out far past the cube; the segments give the
        // soft edge the plain fill does not have.
        for (i, a) in outline.iter().enumerate() {
            outline_segments.push([*a, outline[(i + 1) % outline.len()]]);
        }
        // A plain triangle fan: egui's anti-aliased polygon pushes its corners out along the
        // bisector, which at the sharp tips of the corner triangles leaves visible spikes.
        let mut mesh = egui::epaint::Mesh::default();
        for p in &outline {
            mesh.colored_vertex(*p, fill);
        }
        for i in 1..outline.len() as u32 - 1 {
            mesh.add_triangle(0, i, i + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
        if let Some((label, u, w)) = facet.label
            && facing > 0.12
        {
            let color = v.text_color().gamma_multiply(facing.clamp(0.25, 1.0));
            paint_on_face(&painter, label, facet.dir, u, w, 1.0 - CHAMFER, &project, color);
        }
    }
    for seg in outline_segments {
        painter.line_segment(seg, Stroke::new(1.0, line));
    }
    // axis triad at the cube's lower left
    let o = pos2(rect.left() + 8.0, rect.bottom() - 8.0);
    for (axis, color, name) in [(Vec3::X, Color32::from_rgb(220, 70, 60), "X"), (Vec3::Y, Color32::from_rgb(70, 180, 80), "Y"), (Vec3::Z, Color32::from_rgb(70, 120, 230), "Z")] {
        let tip = o + vec2(axis.dot(right), -axis.dot(up)) * 20.0;
        painter.line_segment([o, tip], Stroke::new(2.0, color));
        painter.text(tip, Align2::CENTER_CENTER, name, egui::FontId::proportional(9.0), color);
    }
    paint_home(&painter, home.shrink(3.0), if home_resp.hovered() { hot } else { line });

    if home_resp.clicked() {
        action = CubeAction::Home;
    } else if resp.dragged() {
        let d = resp.drag_delta();
        cam.orbit(glam::Vec2::new(d.x, d.y));
        action = CubeAction::Moved;
    } else if resp.clicked()
        && let Some(dir) = hit_dir
    {
        look_from(cam, dir);
        action = CubeAction::Moved;
    }
    action
}

/// `text` laid flat on the cube face with outward normal `n`, along its right `u` and up `w`
/// directions, centred and as wide as fits (the face's half side is `half`).
fn paint_on_face(painter: &egui::Painter, text: &str, n: Vec3, u: Vec3, w: Vec3, half: f32, project: &dyn Fn(Vec3) -> Pos2, color: Color32) {
    let size = painter.layout_no_wrap(text.to_string(), LABEL_FONT, color).size();
    // Cube units per text point: the label spans at most 85% of the face and a third of its height.
    let k = (1.7 * half / size.x).min(0.66 * half / size.y);
    paint_text_on_plane(painter, text, n * 1.002, u, w, k, &|p| Some(project(p)), color);
}

const LABEL_FONT: egui::FontId = egui::FontId::proportional(14.0);

/// `text` lying in a plane in 3D, centred on `center`, reading along `u` with its top towards
/// `w`, `k` world units per text point. Every vertex of the glyph mesh is projected on its own,
/// so the text follows the plane in any projection. Drawn twice, half a point apart, for a
/// bolder weight than the default font has. Nothing is drawn if a point does not project.
#[allow(clippy::too_many_arguments)]
pub fn paint_text_on_plane(painter: &egui::Painter, text: &str, center: Vec3, u: Vec3, w: Vec3, k: f32, project: &dyn Fn(Vec3) -> Option<Pos2>, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_string(), LABEL_FONT, color);
    let [aw, ah] = painter.fonts(|f| f.font_image_size());
    let size = galley.size();
    let mut mesh = egui::epaint::Mesh::default();
    for dx in [-0.35f32, 0.35] {
        for placed in &galley.rows {
            let rm = &placed.row.visuals.mesh;
            let base = mesh.vertices.len() as u32;
            for vtx in &rm.vertices {
                let x = placed.pos.x + vtx.pos.x - size.x * 0.5 + dx;
                let y = placed.pos.y + vtx.pos.y - size.y * 0.5;
                let Some(pos) = project(center + u * (x * k) - w * (y * k)) else { return };
                mesh.vertices.push(egui::epaint::Vertex {
                    pos,
                    uv: pos2(vtx.uv.x / aw as f32, vtx.uv.y / ah as f32),
                    // The galley is laid out in `color` already; keep each vertex's own colour.
                    color: vtx.color,
                });
            }
            mesh.indices.extend(rm.indices.iter().map(|i| base + i));
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The scale of the ground grid: the distance from the origin at every major line, along the
/// two edges of the square farthest from the camera, lying on the ground and reading outward.
/// About 11 pixels high wherever they are; every second or fourth one when they would crowd.
pub fn ground_labels(painter: &egui::Painter, grid: crate::render::GroundGrid, view: Rect, view_proj: glam::Mat4, forward: Vec3) {
    let project = |p: Vec3| -> Option<Pos2> {
        let c = view_proj * p.extend(1.0);
        (c.w > 1e-6).then(|| {
            let n = c.truncate() / c.w;
            pos2(view.left() + (n.x * 0.5 + 0.5) * view.width(), view.top() + (0.5 - n.y * 0.5) * view.height())
        })
    };
    let h = grid.half;
    // (outward normal, direction along the edge)
    let mut edges = [(Vec3::Y, Vec3::X), (Vec3::NEG_Y, Vec3::X), (Vec3::X, Vec3::Y), (Vec3::NEG_X, Vec3::Y)];
    edges.sort_by(|a, b| (b.0 * h).dot(forward).total_cmp(&(a.0 * h).dot(forward)));
    let color = Color32::from_gray(105);
    let ticks = (grid.half / grid.major).round() as i32;
    for (out, along) in edges.into_iter().take(2) {
        // Screen pixels per world unit at the middle of this edge, along the edge (where the
        // labels stack) and across it (the way they read).
        let mid = out * h;
        let (Some(m), Some(a), Some(b)) = (project(mid), project(mid + along * grid.major), project(mid + out * grid.major)) else { continue };
        let px_along = (a - m).length() / grid.major;
        let px_out = (b - m).length() / grid.major;
        // Seen at a grazing angle the plane squashes the text past reading: leave this edge bare.
        if px_along.min(px_out) < 0.3 * px_along.max(px_out) || px_along <= 0.0 {
            continue;
        }
        // At most 11 pixels per 16-point line in either direction, so no label grows past the
        // normal size however the plane is turned.
        let k = 11.0 / (16.0 * px_along.max(px_out));
        let px_per_step = px_along * grid.major;
        let line_px = 16.0 * k * px_along;
        let every = [1, 2, 4, 8, 16].into_iter().find(|n| px_per_step * *n as f32 > 1.5 * line_px).unwrap_or(32);
        let px_per_unit = px_out;
        // Read left to right on screen; the text's top towards the side the camera sees.
        let reads_right = project(mid + out).zip(project(mid)).is_some_and(|(p, q)| p.x >= q.x);
        let u = if reads_right { out } else { -out };
        let w = Vec3::Z.cross(u) * if forward.z <= 0.0 { 1.0 } else { -1.0 };
        for i in -ticks..=ticks {
            if i % every != 0 {
                continue;
            }
            let t = i as f32 * grid.major;
            let text = format!("{}", (t.abs() * 1000.0).round() / 1000.0);
            let width = painter.layout_no_wrap(text.clone(), LABEL_FONT, color).size().x * k;
            let at = out * (h + 6.0 / px_per_unit + width * 0.5) + along * t;
            // Zoomed in, most of a long edge is off screen; lay out only what can be seen.
            if !project(at).is_some_and(|p| view.expand(60.0).contains(p)) {
                continue;
            }
            paint_text_on_plane(painter, &text, at, u, w, k, &project, color);
        }
    }
}

/// Whether `p` is inside the convex polygon `poly`, whichever way round its points go.
fn inside_convex(p: Pos2, poly: &[Pos2]) -> bool {
    let mut sign = 0.0f32;
    for (i, a) in poly.iter().enumerate() {
        let b = poly[(i + 1) % poly.len()];
        let cross = (b - *a).x * (p - *a).y - (b - *a).y * (p - *a).x;
        if cross.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    sign != 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_convex_works_both_ways_round() {
        let square = [pos2(0.0, 0.0), pos2(10.0, 0.0), pos2(10.0, 10.0), pos2(0.0, 10.0)];
        let reversed: Vec<Pos2> = square.iter().rev().copied().collect();
        for poly in [&square[..], &reversed[..]] {
            assert!(inside_convex(pos2(5.0, 5.0), poly));
            assert!(inside_convex(pos2(9.9, 0.1), poly));
            assert!(!inside_convex(pos2(11.0, 5.0), poly));
        }
    }

    #[test]
    fn the_chamfered_cube_has_a_facet_for_every_face_edge_and_corner() {
        let facets = cube_facets();
        assert_eq!(facets.len(), 6 + 12 + 8);
        for f in &facets {
            // every outline lies in a plane facing its own direction, outside the cube's centre
            let n = f.dir.normalize();
            let d = f.outline[0].dot(n);
            assert!(d > 0.5, "{:?}", f.dir);
            assert!(f.outline.iter().all(|p| (p.dot(n) - d).abs() < 1e-5), "{:?} not planar", f.dir);
        }
        let mut dirs: Vec<[i32; 3]> = facets.iter().map(|f| f.dir.to_array().map(|x| x as i32)).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), 26, "every direction once");
    }

    #[test]
    fn look_from_matches_the_standard_views() {
        let mut c = Camera::default();
        look_from(&mut c, Vec3::NEG_Y); // front: camera at -Y looks along +Y
        assert!((c.forward() - Vec3::Y).length() < 1e-4);
        look_from(&mut c, Vec3::X); // right: camera at +X looks along -X
        assert!((c.forward() - Vec3::NEG_X).length() < 1e-4);
        look_from(&mut c, Vec3::Z); // top
        assert!(c.forward().z < -0.999);
        look_from(&mut c, Vec3::new(1.0, -1.0, 1.0)); // a corner: iso from front-right-top
        let f = c.forward();
        assert!(f.x < 0.0 && f.y > 0.0 && f.z < 0.0);
    }
}

