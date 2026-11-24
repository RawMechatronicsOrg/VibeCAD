//! Scans in the document: each scan object of the tree is read on a thread of its own (from the
//! scan cache when it can be), then handed to the renderer, which draws it at the object's
//! placement. A scan whose object goes is let go; one whose file changes (another path, size or
//! time, or a file found that was missing) is read again.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use glam::{DMat4, DVec3, Vec3};
use serde_json::{Value, json};

use super::{App, Pending};
use crate::cloud::{self, Cloud, formats};
use crate::render::CloudDraw;

/// A scan object as the tree gives it.
#[derive(Clone, PartialEq)]
pub(super) struct ScanRef {
    pub file: PathBuf,
    /// The file is there.
    pub found: bool,
    /// The file's size and modification time (ns), when found.
    pub stamp: Option<(u64, u64)>,
    /// The object and every group holding it are visible.
    pub shown: bool,
    /// The object's placement, with its groups', document coordinates.
    pub placement: DMat4,
}

impl ScanRef {
    /// What tells one file from another, as far as reading it goes.
    fn identity(&self) -> (PathBuf, bool, Option<(u64, u64)>) {
        (self.file.clone(), self.found, self.stamp)
    }
}

impl ScanRef {
    pub fn parse(v: &Value) -> Option<ScanRef> {
        let m: Vec<f64> = v["matrix"].as_array()?.iter().filter_map(Value::as_f64).collect();
        let m: [f64; 16] = m.try_into().ok()?;
        Some(ScanRef {
            file: PathBuf::from(v["file"].as_str()?),
            found: v["found"].as_bool().unwrap_or(false),
            stamp: v["stamp"].as_array().and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))),
            shown: v["shown"].as_bool().unwrap_or(true),
            // FreeCAD's Matrix.A is row-major.
            placement: DMat4::from_cols_array(&m).transpose(),
        })
    }
}

enum State {
    Loading { progress: Arc<cloud::Progress>, rx: Receiver<Result<Cloud, String>>, started: Instant },
    Ready(Arc<Cloud>),
    Failed(String),
}

struct Slot {
    file: PathBuf,
    /// The file as it was when read (`ScanRef::identity`).
    identity: (PathBuf, bool, Option<(u64, u64)>),
    /// The renderer's name for it.
    key: u64,
    state: State,
}

#[derive(Default)]
pub(super) struct Scans {
    /// By object name.
    slots: HashMap<String, Slot>,
    next_key: u64,
    /// The camera as the last automatic fit left it: a scan that arrives while it is still so
    /// fits the view again, to the scans too.
    pub auto_fit: Option<[f32; 8]>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let State::Loading { progress, .. } = &self.state {
            progress.cancel.store(true, Ordering::Relaxed);
        }
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

    /// After a new tree: read the scans that are new or whose file changed, let go of the rest.
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
            self.scans.slots.insert(name, Slot { identity: r.identity(), file: r.file, key, state });
        }
    }

    /// Scans read since the last frame go to the GPU; loading ones ask for the next frame.
    pub(super) fn poll_scans(&mut self) {
        let mut loading = false;
        let mut arrived = false;
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
                    s.state = State::Ready(c);
                    arrived = true;
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("{name}: {e}"));
                    s.state = State::Failed(e);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => loading = true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => s.state = State::Failed("the reader stopped".into()),
            }
        }
        if arrived && self.scans.auto_fit == Some(self.camera_state()) {
            self.fit();
            self.scans.auto_fit = Some(self.camera_state());
        }
        let uploading = self.scans.slots.values().any(|s| matches!(s.state, State::Ready(_)) && !self.renderer.cloud_uploaded(s.key));
        if uploading {
            self.ctx.request_repaint();
        } else if loading {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Scan coordinates to document coordinates: the object's placement after the scan's centre.
    fn scan_model(placement: DMat4, c: &Cloud) -> DMat4 {
        placement * DMat4::from_translation(c.origin())
    }

    pub(super) fn scan_draws(&self) -> Vec<CloudDraw> {
        self.ready_scans()
            .map(|(name, s, c, r)| CloudDraw {
                key: s.key,
                model: Self::scan_model(r.placement, c),
                selected: self.selection.as_ref().is_some_and(|sel| &sel.obj == name),
            })
            .collect()
    }

    /// The box of the scans shown, in document coordinates.
    pub(super) fn scans_box(&self) -> Option<(Vec3, Vec3)> {
        self.ready_scans()
            .map(|(_, _, c, r)| {
                let m = Self::scan_model(r.placement, c);
                let (lo, hi) = c.local_box();
                let corners = (0..8).map(|i| m.transform_point3(DVec3::new(if i & 1 == 0 { lo.x } else { hi.x } as f64, if i & 2 == 0 { lo.y } else { hi.y } as f64, if i & 4 == 0 { lo.z } else { hi.z } as f64)));
                corners.fold((DVec3::MAX, DVec3::MIN), |(a, b), p| (a.min(p), b.max(p)))
            })
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
            .map(|(lo, hi)| (lo.as_vec3(), hi.as_vec3()))
    }

    fn ready_scans(&self) -> impl Iterator<Item = (&String, &Slot, &Arc<Cloud>, &ScanRef)> {
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
                format!(
                    "{} points · {} · {:.1} × {:.1} × {:.1} mm{}{}",
                    c.points.len(),
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
