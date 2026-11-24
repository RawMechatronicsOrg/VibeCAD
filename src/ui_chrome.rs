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
                    button(ui, &mut out, Action::ToggleGrid, Icon::Grid, "Grid", true, s.grid, "A faint grid on the surfaces; on curved faces it reads like contour lines");
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
    Grid,
    Palette,
    Log,
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
        Icon::Grid => {
            // A patch of surface, bent, with its grid lines.
            poly(&[(0.05, 0.3), (0.95, 0.1), (0.95, 0.7), (0.05, 0.9)], true);
            for t in [0.33, 0.67] {
                p.line_segment([at(0.05 + 0.9 * t, 0.3 - 0.2 * t), at(0.05 + 0.9 * t, 0.9 - 0.2 * t)], Stroke::new(1.0, c));
                p.line_segment([at(0.05, 0.3 + 0.6 * t), at(0.95, 0.1 + 0.6 * t)], Stroke::new(1.0, c));
            }
        }
        Icon::Palette => {
            for (x, y, rgb) in [(0.32, 0.38, [158, 188, 222]), (0.68, 0.38, [228, 178, 156]), (0.5, 0.7, [166, 206, 166])] {
                p.circle(at(x, y), r.width() * 0.22, Color32::from_rgb(rgb[0], rgb[1], rgb[2]), s);
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

/// A translucent panel floating over the 3D view, with a header that folds it. It is an egui
/// window without a title bar: unlike a bare area, a window grows and shrinks with its content.
#[allow(clippy::too_many_arguments)]
pub fn overlay(
    ctx: &egui::Context,
    id: &str,
    anchor: Pos2,
    pivot: Align2,
    width: f32,
    max_height: f32,
    title: &str,
    add: impl FnOnce(&mut egui::Ui),
) {
    let open_id = Id::unique(("overlay-open", id));
    let mut open = ctx.data_mut(|d| *d.get_persisted_mut_or(open_id, true));
    let v = ctx.global_style().visuals.clone();
    let frame = egui::Frame::new()
        .fill(v.panel_fill.gamma_multiply(0.82))
        .stroke(Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6)))
        .corner_radius(6.0)
        .inner_margin(8.0);
    egui::Window::new(title)
        .id(Id::unique(("overlay", id)))
        .title_bar(false)
        .resizable(false)
        .movable(false)
        .frame(frame)
        .fixed_pos(anchor)
        .pivot(pivot)
        .min_width(width)
        .max_width(width)
        .show(ctx, |ui| {
            let head = ui.horizontal(|ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::click());
                paint_fold(ui.painter(), r, open, ui.visuals().text_color());
                let t = ui.add(egui::Label::new(RichText::new(title).strong()).sense(Sense::click()));
                resp.clicked() || t.clicked()
            });
            if head.inner {
                open = !open;
            }
            if open {
                ui.separator();
                egui::ScrollArea::vertical().max_height(max_height.max(60.0)).auto_shrink([false, true]).show(ui, add);
            }
        });
    ctx.data_mut(|d| d.insert_persisted(open_id, open));
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

/// The view cube in the top-right corner of `view`: shows the orientation, a click on a face,
/// edge or corner turns the camera there, a drag orbits.
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
    let hot = Color32::from_rgb(90, 160, 230);
    let line = if dark { Color32::from_gray(150) } else { Color32::from_gray(120) };
    let hover = resp.hover_pos();

    let mut action = CubeAction::None;
    let mut hit_dir: Option<Vec3> = None;
    for (n, u, w, label) in FACES {
        let facing = -n.dot(f);
        if facing <= 0.02 {
            continue;
        }
        let corners = [n - u - w, n + u - w, n + u + w, n - u + w].map(project);
        let local = hover.and_then(|h| face_coords(h, &corners));
        let mut fill = base;
        if let Some((a, b)) = local {
            // Outer bands select an edge or corner of the cube, the middle the face.
            let dir = n + if a.abs() > 0.6 { u * a.signum() } else { Vec3::ZERO } + if b.abs() > 0.6 { w * b.signum() } else { Vec3::ZERO };
            hit_dir = Some(dir);
            fill = base.lerp_to_gamma(hot, 0.45);
        }
        painter.add(egui::Shape::convex_polygon(corners.to_vec(), fill, Stroke::new(1.0, line)));
        if facing > 0.35 {
            let c = project(n * 1.01);
            let color = v.text_color().gamma_multiply(facing.clamp(0.0, 1.0));
            painter.text(c, Align2::CENTER_CENTER, label, egui::FontId::proportional(11.0), color);
        }
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

/// Where `p` lies on the quad `c` (corners in order: -u-v, +u-v, +u+v, -u+v), as (u, v) in
/// -1..1, or None if outside. The quad is a parallelogram under the orthographic cube projection.
fn face_coords(p: Pos2, c: &[Pos2; 4]) -> Option<(f32, f32)> {
    let o = c[0];
    let eu = c[1] - o;
    let ev = c[3] - o;
    let det = eu.x * ev.y - eu.y * ev.x;
    if det.abs() < 1e-3 {
        return None;
    }
    let d = p - o;
    let a = (d.x * ev.y - d.y * ev.x) / det;
    let b = (eu.x * d.y - eu.y * d.x) / det;
    ((0.0..=1.0).contains(&a) && (0.0..=1.0).contains(&b)).then(|| (a * 2.0 - 1.0, b * 2.0 - 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_coords_map_the_quad_to_minus_one_one() {
        let c = [pos2(0.0, 10.0), pos2(10.0, 10.0), pos2(10.0, 0.0), pos2(0.0, 0.0)];
        let (a, b) = face_coords(pos2(5.0, 5.0), &c).unwrap();
        assert!(a.abs() < 1e-5 && b.abs() < 1e-5);
        let (a, b) = face_coords(pos2(9.9, 0.1), &c).unwrap();
        assert!(a > 0.9 && b > 0.9);
        assert!(face_coords(pos2(11.0, 5.0), &c).is_none());
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
