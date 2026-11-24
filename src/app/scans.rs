//! Scans in the document: each scan object of the tree is read on a thread of its own (from the
//! scan cache when it can be), then handed to the renderer, which draws it at the object's
//! placement. A scan whose object goes is let go; one whose file changes (another path, size or
//! time, or a file found that was missing) is read again. Its deleted points come from the mask
//! file the document holds for it (see scan_edit.rs).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use glam::{DMat4, DVec3, Vec3};
use serde_json::{Value, json};

use super::{App, Pending};
use crate::cloud::marks::{self, Marks};
use crate::cloud::{self, Cloud, formats};
use crate::render::{CloudDraw, ScanDisplay};

type Stamp = Option<(u64, u64)>;

/// A scan object as the tree gives it.
#[derive(Clone, PartialEq)]
pub(super) struct ScanRef {
    pub file: PathBuf,
    /// The file is there.
    pub found: bool,
    /// The file's size and modification time (ns), when found.
    pub stamp: Stamp,
    /// The object and every group holding it are visible.
    pub shown: bool,
    /// The object's placement, with its groups', document coordinates.
    pub placement: DMat4,
    /// The document's copy of the mask of deleted points, and its size and time.
    pub mask: Option<(PathBuf, Stamp)>,
}

fn stamp(v: &Value) -> Stamp {
    let a = v.as_array()?;
    Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))
}

impl ScanRef {
    pub fn parse(v: &Value) -> Option<ScanRef> {
        let m: Vec<f64> = v["matrix"].as_array()?.iter().filter_map(Value::as_f64).collect();
        let m: [f64; 16] = m.try_into().ok()?;
        Some(ScanRef {
            file: PathBuf::from(v["file"].as_str()?),
            found: v["found"].as_bool().unwrap_or(false),
            stamp: stamp(&v["stamp"]),
            shown: v["shown"].as_bool().unwrap_or(true),
            // FreeCAD's Matrix.A is row-major.
            placement: DMat4::from_cols_array(&m).transpose(),
            mask: v["mask"].as_str().map(|p| (PathBuf::from(p), stamp(&v["mask_stamp"]))),
        })
    }

    /// What tells one file from another, as far as reading it goes.
    fn identity(&self) -> (PathBuf, bool, Stamp) {
        (self.file.clone(), self.found, self.stamp)
    }
}

pub(super) enum State {
    Loading { progress: Arc<cloud::Progress>, rx: Receiver<Result<Cloud, String>>, started: Instant },
    Ready(Arc<Cloud>),
    Failed(String),
}

pub(super) struct Slot {
    pub file: PathBuf,
    /// The file as it was when read (`ScanRef::identity`).
    identity: (PathBuf, bool, Stamp),
    /// The renderer's name for it.
    pub key: u64,
    pub state: State,
    /// Selected and deleted points, once read.
    pub marks: Marks,
    /// The mask the deleted points were last taken from; None when they were changed here
    /// since, so the document's is read again once the worker has answered.
    pub mask: Option<Option<(PathBuf, Stamp)>>,
    /// Changes of the mask sent to the worker and not answered yet: the tree's mask may be
    /// older than what is shown until then.
    pub pending: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let State::Loading { progress, .. } = &self.state {
            progress.cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
pub(super) struct Scans {
    /// By object name.
    pub slots: HashMap<String, Slot>,
    next_key: u64,
    /// The camera as the last automatic fit left it: a scan that arrives while it is still so
    /// fits the view again, to the scans too.
    pub auto_fit: Option<[f32; 8]>,
    /// The point selection tool is on: a drag draws a lasso, a click picks a point.
    pub tool: bool,
    /// The lasso being drawn, in points of the window.
    pub lasso: Vec<eframe::egui::Pos2>,
    pub display: ScanDisplay,
    /// Scans being written out: what to say when each is done.
    pub exports: Vec<Receiver<Result<String, String>>>,
    /// Mask files written for the worker, removed once it has taken them in.
    pub mask_files: u64,
}

impl Scans {
    pub fn new(display: ScanDisplay) -> Scans {
        Scans { display, ..Default::default() }
    }
}

impl App {
    pub(super) fn import_scan_dialog(&mut self) {
        if self.doc_label.is_none() {
            return;
        }
        let picked = rfd::FileDialog::new()
            .add_filter("Scans (points or meshes)", formats::EXTENSIONS)
            .add_filter("PLY", &["ply"])
            .add_filter("STL", &["stl"])
            .add_filter("E57 (metres)", &["e57"])
            .add_filter("Points as text", &["xyz", "asc", "pts", "txt", "csv"])
            .pick_file();
        if let Some(p) = picked {
            self.scans.auto_fit = Some(self.camera_state());
            self.request("add_scan", json!({ "path": p.to_string_lossy() }), Pending::Create);
        }
    }

    /// After a new tree: read the scans that are new or whose file changed, let go of the rest,
    /// and take in masks of deleted points that changed.
    pub(super) fn sync_scans(&mut self) {
        let wanted: HashMap<String, ScanRef> = self.tree.nodes.iter().filter_map(|(name, n)| Some((name.clone(), n.scan.clone()?))).collect();
        let gone: Vec<String> = self.scans.slots.iter().filter(|(name, s)| wanted.get(*name).is_none_or(|r| r.identity() != s.identity)).map(|(n, _)| n.clone()).collect();
        for name in gone {
            if let Some(s) = self.scans.slots.remove(&name) {
                self.renderer.remove_cloud(s.key);
            }
        }
        for (name, r) in wanted {
            if self.scans.slots.contains_key(&name) {
                self.apply_mask(&name);
                continue;
            }
            self.scans.next_key += 1;
            let key = self.scans.next_key;
            let state = if !r.found {
                State::Failed(format!("{} not found", r.file.display()))
            } else {
                let progress = Arc::new(cloud::Progress::default());
                let (tx, rx) = channel();
                let (p, file, ctx) = (progress.clone(), r.file.clone(), self.ctx.clone());
                std::thread::Builder::new()
                    .name("scan reader".into())
                    .spawn(move || {
                        let result = cloud::load(&file, cloud::cache_dir().as_deref(), &p).map_err(|e| format!("{e:#}"));
                        let _ = tx.send(result);
                        ctx.request_repaint();
                    })
                    .ok();
                State::Loading { progress, rx, started: Instant::now() }
            };
            self.scans.slots.insert(name, Slot { identity: r.identity(), file: r.file, key, state, marks: Marks::default(), mask: None, pending: 0 });
        }
    }

    /// The deleted points of a scan that is read, from the document's mask, when that changed.
    pub(super) fn apply_mask(&mut self, name: &str) {
        let Some(want) = self.tree.nodes.get(name).and_then(|n| n.scan.as_ref()).map(|r| r.mask.clone()) else { return };
        let Some(s) = self.scans.slots.get_mut(name) else { return };
        let State::Ready(c) = &s.state else { return };
        if s.pending > 0 || s.mask.as_ref() == Some(&want) {
            return;
        }
        let bits = match &want {
            None => Ok(Vec::new()),
            Some((path, _)) => marks::read_mask(path, c.points.len(), c.fingerprint()).map_err(|e| format!("{e:#}")),
        };
        match bits {
            Ok(bits) => s.marks.set_deleted(&bits),
            Err(e) => {
                s.marks.set_deleted(&[]);
                let label = self.tree.nodes.get(name).map_or(name, |n| n.label.as_str());
                self.error = Some(format!("{label}: {e}"));
            }
        }
        s.mask = Some(want);
        self.renderer.set_cloud_marks(s.key, s.marks.words());
    }

    /// Scans read since the last frame go to the GPU; loading ones ask for the next frame.
    pub(super) fn poll_scans(&mut self) {
        let mut loading = false;
        let mut arrived = Vec::new();
        for (name, s) in &mut self.scans.slots {
            let State::Loading { rx, .. } = &s.state else { continue };
            match rx.try_recv() {
                Ok(Ok(c)) => {
                    let c = Arc::new(c);
                    if let Err(e) = self.renderer.add_cloud(s.key, c.clone()) {
                        self.error = Some(format!("{name}: {e}"));
                        s.state = State::Failed(e);
                        continue;
                    }
                    self.info = Some(format!(
                        "{}: {} points{} in {:.1} s",
                        s.file.file_name().map_or_else(|| name.clone(), |f| f.to_string_lossy().into_owned()),
                        c.points.len(),
                        if c.from_cache { " from the cache" } else { "" },
                        c.load_ms / 1e3
                    ));
                    s.marks = Marks::new(c.points.len());
                    s.state = State::Ready(c);
                    arrived.push(name.clone());
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("{name}: {e}"));
                    s.state = State::Failed(e);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => loading = true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => s.state = State::Failed("the reader stopped".into()),
            }
        }
        for name in &arrived {
            self.apply_mask(name);
        }
        if !arrived.is_empty() && self.scans.auto_fit == Some(self.camera_state()) {
            self.fit();
            self.scans.auto_fit = Some(self.camera_state());
        }
        self.poll_exports();
        let uploading = self.scans.slots.values().any(|s| matches!(s.state, State::Ready(_)) && !self.renderer.cloud_uploaded(s.key));
        if uploading {
            self.ctx.request_repaint();
        } else if loading || !self.scans.exports.is_empty() {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Scan coordinates to document coordinates: the object's placement after the scan's centre.
    pub(super) fn scan_model(placement: DMat4, c: &Cloud) -> DMat4 {
        placement * DMat4::from_translation(c.origin())
    }

    pub(super) fn scan_draws(&self) -> Vec<CloudDraw> {
        self.ready_scans()
            .map(|(name, s, c, r)| {
                let model = Self::scan_model(r.placement, c);
                let (lo, hi) = Self::doc_box(model, c);
                CloudDraw { key: s.key, model, selected: self.selection.as_ref().is_some_and(|sel| &sel.obj == name), heights: (lo.z as f32, hi.z as f32) }
            })
            .collect()
    }

    /// A scan's box in document coordinates.
    fn doc_box(m: DMat4, c: &Cloud) -> (DVec3, DVec3) {
        let (lo, hi) = c.local_box();
        let corners = (0..8).map(|i| m.transform_point3(DVec3::new(if i & 1 == 0 { lo.x } else { hi.x } as f64, if i & 2 == 0 { lo.y } else { hi.y } as f64, if i & 4 == 0 { lo.z } else { hi.z } as f64)));
        corners.fold((DVec3::MAX, DVec3::MIN), |(a, b), p| (a.min(p), b.max(p)))
    }

    /// The box of the scans shown, in document coordinates.
    pub(super) fn scans_box(&self) -> Option<(Vec3, Vec3)> {
        self.ready_scans()
            .map(|(_, _, c, r)| Self::doc_box(Self::scan_model(r.placement, c), c))
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
            .map(|(lo, hi)| (lo.as_vec3(), hi.as_vec3()))
    }

    /// The scans read and shown: name, slot, points, object.
    pub(super) fn ready_scans(&self) -> impl Iterator<Item = (&String, &Slot, &Arc<Cloud>, &ScanRef)> {
        self.scans.slots.iter().filter_map(|(name, s)| {
            let State::Ready(c) = &s.state else { return None };
            let node = self.tree.nodes.get(name).filter(|n| n.visible)?;
            let r = node.scan.as_ref().filter(|r| r.shown)?;
            Some((name, s, c, r))
        })
    }

    /// While scans are read: which, how far.
    pub(super) fn scan_status(&self) -> Option<String> {
        let parts: Vec<String> = self
            .scans
            .slots
            .iter()
            .filter_map(|(name, s)| {
                let State::Loading { progress, started, .. } = &s.state else { return None };
                let label = self.tree.nodes.get(name).map_or(name.as_str(), |n| n.label.as_str());
                let (read, total) = (progress.read.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed).max(1));
                let what = match progress.stage.load(Ordering::Relaxed) {
                    0 => format!("reading {}%", read * 100 / total),
                    1 => "sorting".into(),
                    2 => "fitting normals".into(),
                    _ => "caching".into(),
                };
                Some(format!("{label}: {what} ({:.0} s)", started.elapsed().as_secs_f64()))
            })
            .collect();
        (!parts.is_empty()).then(|| format!("Scan {}", parts.join(", ")))
    }

    /// What the property panel shows about a scan besides its properties.
    pub(super) fn scan_summary(&self, obj: &str) -> Option<String> {
        let s = self.scans.slots.get(obj)?;
        Some(match &s.state {
            State::Loading { .. } => "reading…".into(),
            State::Failed(e) => e.clone(),
            State::Ready(c) => {
                let h = &c.header;
                let size = Vec3::from(h.hi) - Vec3::from(h.lo);
                let deleted = s.marks.deleted();
                format!(
                    "{} points{} · {} · {:.1} × {:.1} × {:.1} mm{}{}",
                    c.points.len(),
                    if deleted > 0 { format!(" ({deleted} deleted)") } else { String::new() },
                    h.format,
                    size.x,
                    size.y,
                    size.z,
                    if h.has_color { " · colours" } else { "" },
                    if h.normals_estimated { " · normals fitted" } else { " · normals from the file" },
                )
            }
        })
    }
}
