//! Working on a scan's points (the SCAN tab): select them with a lasso or a click, delete the
//! selection, bring deleted points back, write what is left to a PLY.
//!
//! The selection lives in the window only. Deleting writes the scan's mask of deleted points to
//! a file the worker copies into the document (`scan_mask`): one undo step, saved with the
//! document, and taken back in from the tree like any other change.

use std::path::PathBuf;
use std::sync::mpsc::channel;

use eframe::egui::{self, Color32, PointerButton, Pos2, Rect, Stroke};
use glam::Vec2;
use serde_json::json;

use super::scans::State;
use super::{App, BACKGROUND, Pending};
use crate::cloud::formats;
use crate::cloud::marks::{self, Lasso, Op, Screen};

/// Pixels from the click a picked point may be.
const PICK_REACH: f32 = 8.0;

impl App {
    pub(super) fn toggle_point_tool(&mut self) {
        self.scans.tool = !self.scans.tool;
        self.scans.lasso.clear();
    }

    /// The worker answered a change of a scan's mask (or refused it): the deleted points shown
    /// are taken again from the document's mask, with the next tree.
    pub(super) fn scan_mask_done(&mut self, obj: &str, file: Option<&std::path::Path>) {
        if let Some(f) = file {
            let _ = std::fs::remove_file(f); // the document holds its own copy
        }
        if let Some(s) = self.scans.slots.get_mut(obj) {
            s.pending = s.pending.saturating_sub(1);
            s.mask = None;
        }
    }

    /// Names of the scans read and shown.
    fn shown_scans(&self) -> Vec<String> {
        self.ready_scans().map(|(n, ..)| n.clone()).collect()
    }

    /// Points selected, over all scans.
    pub(super) fn points_selected(&self) -> usize {
        self.scans.slots.values().map(|s| s.marks.selected()).sum()
    }

    pub(super) fn points_deleted(&self) -> usize {
        self.scans.slots.values().map(|s| s.marks.deleted()).sum()
    }

    /// The selected object, if it is a scan.
    fn selected_scan(&self) -> Option<String> {
        self.selection.as_ref().map(|s| s.obj.clone()).filter(|o| self.scans.slots.contains_key(o))
    }

    /// How the modifiers held change the selection: Shift adds, Ctrl removes.
    fn select_op(ui: &egui::Ui) -> Op {
        ui.input(|i| if i.modifiers.command { Op::Remove } else if i.modifiers.shift { Op::Add } else { Op::Replace })
    }

    /// Each scan shown and read, with where its points fall on a view of `size` pixels.
    fn scan_screens(&self, size: (u32, u32)) -> Vec<(String, Screen)> {
        let f = self.frame(size, BACKGROUND);
        self.ready_scans()
            .map(|(name, _, c, r)| {
                let mvp = (f.view_proj.as_dmat4() * Self::scan_model(r.placement, c)).as_mat4();
                (name.clone(), Screen { mvp, size: Vec2::new(size.0 as f32, size.1 as f32) })
            })
            .collect()
    }

    fn clear_all_selections(&mut self) {
        for s in self.scans.slots.values_mut() {
            if s.marks.selected() > 0 {
                s.marks.clear_selection();
                self.renderer.set_cloud_marks(s.key, s.marks.words());
            }
        }
    }

    /// A drag with the point tool on draws a lasso; it selects when let go. True while the drag
    /// is the lasso's, so the camera does not move.
    pub(super) fn lasso_drag(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: Rect, ppp: f32) -> bool {
        if !self.scans.tool || self.sketch.is_some() {
            return false;
        }
        if resp.drag_started_by(PointerButton::Primary) {
            // From where the button went down, not where the drag was first seen.
            self.scans.lasso = ui.input(|i| i.pointer.press_origin()).into_iter().collect();
        }
        if resp.dragged_by(PointerButton::Primary) {
            if let Some(p) = resp.interact_pointer_pos()
                && self.scans.lasso.last().is_none_or(|q| q.distance(p) > 2.0)
            {
                self.scans.lasso.push(p);
            }
            return true;
        }
        if resp.drag_stopped_by(PointerButton::Primary) && !self.scans.lasso.is_empty() {
            let path: Vec<Vec2> = std::mem::take(&mut self.scans.lasso).iter().map(|p| Vec2::new((p.x - rect.min.x) * ppp, (p.y - rect.min.y) * ppp)).collect();
            let size = ((rect.width() * ppp).round() as u32, (rect.height() * ppp).round() as u32);
            self.select_in_lasso(&path, size, Self::select_op(ui));
            return true;
        }
        false
    }

    fn select_in_lasso(&mut self, path: &[Vec2], size: (u32, u32), op: Op) {
        let Some(lasso) = Lasso::new(path) else { return };
        if op == Op::Replace {
            self.clear_all_selections();
        }
        let op = if op == Op::Replace { Op::Add } else { op };
        for (name, screen) in self.scan_screens(size) {
            let Some(s) = self.scans.slots.get_mut(&name) else { continue };
            let State::Ready(c) = &s.state else { continue };
            let c = c.clone();
            s.marks.select(
                &c.points,
                &c.header.chunks,
                op,
                |ch| screen.box_rect(ch.lo.into(), ch.hi.into()).is_some_and(|r| lasso.meets(r)),
                |p| screen.project(p.pos.into()).is_some_and(|(px, _)| lasso.contains(px)),
            );
            self.renderer.set_cloud_marks(s.key, s.marks.words());
        }
        self.note_point_selection();
    }

    /// A click with the point tool on: the point nearest the eye under the cursor.
    pub(super) fn click_points(&mut self, ui: &egui::Ui, pos: Pos2, rect: Rect, ppp: f32) {
        let size = ((rect.width() * ppp).round() as u32, (rect.height() * ppp).round() as u32);
        let at = Vec2::new((pos.x - rect.min.x) * ppp, (pos.y - rect.min.y) * ppp);
        let mut best: Option<(String, usize, f32)> = None;
        for (name, screen) in self.scan_screens(size) {
            let Some(s) = self.scans.slots.get(&name) else { continue };
            let State::Ready(c) = &s.state else { continue };
            if let Some((i, depth)) = marks::nearest(&c.points, &c.header.chunks, &s.marks, &screen, at, PICK_REACH * ppp)
                && best.as_ref().is_none_or(|b| depth < b.2)
            {
                best = Some((name, i, depth));
            }
        }
        let op = Self::select_op(ui);
        if op == Op::Replace {
            self.clear_all_selections();
        }
        if let Some((name, i, _)) = best
            && let Some(s) = self.scans.slots.get_mut(&name)
        {
            s.marks.set_selected(i, op != Op::Remove);
            self.renderer.set_cloud_marks(s.key, s.marks.words());
        }
        self.note_point_selection();
    }

    fn note_point_selection(&mut self) {
        let n = self.points_selected();
        self.info = Some(if n > 0 { format!("{n} points selected: Del deletes them") } else { "no points selected".into() });
    }

    /// Invert the selection in the scans that have one; with none, in the selected scan, or
    /// in every scan shown.
    pub(super) fn invert_points(&mut self) {
        let shown = self.shown_scans();
        let mut names: Vec<String> = shown.iter().filter(|n| self.scans.slots[*n].marks.selected() > 0).cloned().collect();
        if names.is_empty() {
            names = self.selected_scan().filter(|n| shown.contains(n)).map_or(shown, |n| vec![n]);
        }
        for name in names {
            let Some(s) = self.scans.slots.get_mut(&name) else { continue };
            s.marks.invert_selection();
            self.renderer.set_cloud_marks(s.key, s.marks.words());
        }
        self.note_point_selection();
    }

    pub(super) fn clear_points(&mut self) {
        self.clear_all_selections();
        self.note_point_selection();
    }

    /// Delete the selected points: shown so at once, and saved in the document as each scan's
    /// mask, one undo step per scan.
    pub(super) fn delete_points(&mut self) {
        let dir = std::env::temp_dir().join("vibecad-masks");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.error = Some(format!("cannot write the deleted points: {e}"));
            return;
        }
        // Masks left by a window that closed before the worker took them in.
        if self.scans.mask_files == 0 {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 24 * 3600) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        // Only scans shown: a hidden scan's selection is not deleted unseen.
        let names: Vec<String> = self.shown_scans().into_iter().filter(|n| self.scans.slots[n].marks.selected() > 0).collect();
        for name in names {
            self.scans.mask_files += 1;
            let file = dir.join(format!("{name}-{}-{}.mask", std::process::id(), self.scans.mask_files));
            let Some(s) = self.scans.slots.get_mut(&name) else { continue };
            let State::Ready(c) = &s.state else { continue };
            let print = c.fingerprint();
            s.marks.delete_selected();
            self.renderer.set_cloud_marks(s.key, s.marks.words());
            match marks::write_mask(&file, &s.marks, print) {
                Ok(()) => {
                    s.pending += 1;
                    self.request("scan_mask", json!({ "obj": name, "path": file.to_string_lossy() }), Pending::ScanMask { obj: name.clone(), file: Some(file) });
                }
                Err(e) => {
                    // Not saved: show what the document holds again.
                    s.mask = None;
                    self.error = Some(format!("cannot write the deleted points of {name}: {e:#}"));
                    self.apply_mask(&name);
                }
            }
        }
    }

    /// Bring back the deleted points of the selected scan, or of every scan when none is selected.
    pub(super) fn restore_points(&mut self) {
        let with_deleted = |n: &String| self.scans.slots.get(n).is_some_and(|s| s.marks.deleted() > 0);
        let names: Vec<String> = match self.selected_scan() {
            Some(n) => vec![n].into_iter().filter(with_deleted).collect(),
            None => self.scans.slots.keys().filter(|n| with_deleted(n)).cloned().collect(),
        };
        for name in names {
            // Shown at once, so a deletion made before the worker answers starts from none.
            if let Some(s) = self.scans.slots.get_mut(&name) {
                s.marks.set_deleted(&[]);
                s.pending += 1;
                self.renderer.set_cloud_marks(s.key, s.marks.words());
            }
            self.request("scan_mask", json!({ "obj": name, "path": null }), Pending::ScanMask { obj: name.clone(), file: None });
        }
    }

    /// Write the selected scan's points (the only scan's, if there is one), less those deleted,
    /// to a PLY in document coordinates, on a thread.
    pub(super) fn save_points_dialog(&mut self) {
        let ready: Vec<String> = self.scans.slots.iter().filter(|(_, s)| matches!(s.state, State::Ready(_))).map(|(n, _)| n.clone()).collect();
        let Some(name) = self.selected_scan().or_else(|| (ready.len() == 1).then(|| ready[0].clone())) else {
            self.error = Some("select the scan to save in the tree".into());
            return;
        };
        let (Some(s), Some(r)) = (self.scans.slots.get(&name), self.tree.nodes.get(&name).and_then(|n| n.scan.clone())) else { return };
        let State::Ready(c) = &s.state else {
            self.error = Some(format!("{name} is not read yet"));
            return;
        };
        let label = self.tree.nodes.get(&name).map_or(name.clone(), |n| n.label.clone());
        let Some(path) = rfd::FileDialog::new().add_filter("PLY", &["ply"]).set_file_name(format!("{label}.ply")).save_file() else { return };
        let path: PathBuf = if path.extension().is_none() { path.with_extension("ply") } else { path };
        let (c, m, model, ctx) = (c.clone(), s.marks.clone(), Self::scan_model(r.placement, c), self.ctx.clone());
        let (tx, rx) = channel();
        self.scans.exports.push(rx);
        self.info = Some(format!("writing {}…", path.display()));
        std::thread::spawn(move || {
            let result = formats::write_ply(&path, &c, &m, model).map(|n| format!("saved {n} points to {}", path.display())).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub(super) fn poll_exports(&mut self) {
        let mut done = Vec::new();
        for (k, rx) in self.scans.exports.iter().enumerate() {
            match rx.try_recv() {
                Ok(Ok(msg)) => {
                    self.info = Some(msg);
                    done.push(k);
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    done.push(k);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => done.push(k),
            }
        }
        for k in done.into_iter().rev() {
            self.scans.exports.remove(k);
        }
    }

    /// Esc and Del while the point tool is on; true when they were used here.
    pub(super) fn point_keys(&mut self, escape: bool, delete: bool) -> bool {
        if !self.scans.tool {
            return false;
        }
        let selected = self.points_selected() > 0;
        if escape {
            if selected {
                self.clear_points();
            } else {
                self.toggle_point_tool();
            }
            return true;
        }
        if delete && selected {
            self.delete_points();
            return true;
        }
        false
    }

    pub(super) fn scan_prompt(&self) -> Option<&'static str> {
        self.scans.tool.then_some("Points: drag around them or click one · Shift adds, Ctrl removes · Del deletes · Esc")
    }

    pub(super) fn paint_lasso(&self, painter: &egui::Painter) {
        if self.scans.lasso.len() < 2 {
            return;
        }
        let mut pts = self.scans.lasso.clone();
        pts.push(pts[0]);
        let orange = Color32::from_rgb(255, 140, 20);
        painter.add(egui::Shape::line(pts, Stroke::new(1.5, orange)));
    }
}
