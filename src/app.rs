//! The window: menu, model tree, 3D view and property panel around one FreeCAD worker.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Color32, KeyboardShortcut, Modifiers, PointerButton, Sense};
use glam::{Vec2, Vec3};
use serde_json::{Value, json};

use crate::camera::{Camera, View};
use crate::navigation::{Gesture, Held, Scheme};
use crate::profile::{Profile, Settings};
use crate::render::{Frame, GroundGrid, Renderer};
use crate::scene::SceneData;
use crate::selftest::{SelfTest, Stage, write_bmp};
use crate::ui_chrome::{self, Action, CubeAction, RibbonState, Tab};
use crate::worker::{Body, Event, Part, Progress, Worker};

#[derive(Clone)]
enum Pending {
    Open(PathBuf),
    Tree,
    Scene { fit: bool },
    Props,
    Edit,
    Save,
    Export,
}

struct Node {
    label: String,
    kind: String,
    children: Vec<String>,
    visible: bool,
    error: bool,
}

#[derive(Default)]
struct Tree {
    nodes: HashMap<String, Node>,
    roots: Vec<String>,
    /// The bodies' features in the order they are built, body by body: the timeline.
    history: Vec<String>,
    /// The body each feature of the timeline belongs to.
    owner: HashMap<String, String>,
    undo: u64,
    redo: u64,
}

const BODY: &str = "PartDesign::Body";

impl Tree {
    /// The tree lists parts; what a body is built from goes to the timeline. A feature holds
    /// the sketches it is made from, so they come just before it.
    fn build_history(&mut self) {
        fn steps(t: &Tree, name: &str, body: &str, depth: usize, out: &mut Vec<(String, String)>) {
            let Some(n) = t.nodes.get(name) else { return };
            if depth > 32 || out.iter().any(|(o, _)| o == name) {
                return;
            }
            for c in &n.children {
                steps(t, c, body, depth + 1, out);
            }
            out.push((name.to_string(), body.to_string()));
        }
        fn bodies<'a>(t: &'a Tree, name: &'a str, depth: usize, seen: &mut HashSet<&'a str>, out: &mut Vec<&'a str>) {
            let Some(n) = t.nodes.get(name) else { return };
            if depth > 32 || !seen.insert(name) {
                return;
            }
            if n.kind == BODY {
                out.push(name);
                return;
            }
            for c in &n.children {
                bodies(t, c, depth + 1, seen, out);
            }
        }
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for r in &self.roots {
            bodies(self, r, 0, &mut seen, &mut found);
        }
        let mut out = Vec::new();
        for b in found {
            for c in &self.nodes[b].children {
                steps(self, c, b, 1, &mut out);
            }
        }
        self.history = out.iter().map(|(f, _)| f.clone()).collect();
        self.owner = out.into_iter().collect();
    }
}

/// What picking needs from the scene shown; the vertices themselves live on the GPU. It grows
/// as the meshes of a stream arrive.
struct SceneIndex {
    data: SceneData,
    by_obj: HashMap<String, u32>,
    /// Time spent decoding the meshes on the worker thread, in milliseconds.
    build_ms: f64,
}

/// A scene stream being received.
struct Loading {
    id: u64,
    /// Fit the camera to it (a document was opened).
    fit: bool,
    started: Instant,
    progress: Option<Progress>,
    /// When tessellation progress started counting, for the time left.
    counting_since: Option<(Instant, u64)>,
    /// Boxes of a refresh, held back until its first meshes so the old scene does not flash
    /// into boxes on every edit.
    held: Option<Box<SceneData>>,
    boxes_ms: Option<f64>,
    first_meshes_ms: Option<f64>,
    batches: u32,
    /// The camera as the automatic fit left it; if the user has not moved it when the last
    /// mesh arrives, it is fitted again to the exact bounds.
    fitted: Option<[f32; 8]>,
}

#[derive(Clone, PartialEq)]
struct Selection {
    obj: String,
    /// "Face6" when a face was picked in the view.
    sub: Option<String>,
    face_id: u32,
}

/// An edit in progress in the property panel, committed to the worker when finished.
enum EditBuf {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(String),
    Enum(String),
}

pub struct App {
    ctx: egui::Context,
    worker: Worker,
    freecad: Option<String>,
    worker_error: Option<String>,
    renderer: Renderer,
    camera: Camera,
    pending: HashMap<u64, Pending>,
    doc_label: Option<String>,
    doc_path: Option<PathBuf>,
    tree: Tree,
    scene: Option<SceneIndex>,
    selection: Option<Selection>,
    props: Option<Value>,
    edits: HashMap<String, EditBuf>,
    error: Option<String>,
    info: Option<String>,
    show_edges: bool,
    /// Edge width in pixels.
    edge_width: f32,
    /// Default-grey objects get a palette colour each.
    palette: bool,
    show_log: bool,
    show_grid: bool,
    /// How mouse drags move the camera.
    navigation: Scheme,
    open_on_ready: Option<PathBuf>,
    /// The document has edits that are not saved.
    modified: bool,
    tab: Tab,
    /// Where the 3D view was drawn last frame; the floating panels sit over it.
    view_rect: Option<egui::Rect>,
    loading: Option<Loading>,
    /// When the document shown was asked for, for the load timeline.
    opened_at: Option<Instant>,
    /// How the last scene stream went, in milliseconds since it was asked for.
    last_load: Option<Value>,
    selftest: Option<SelfTest>,
    /// The user's settings, kept between runs.
    profile: Profile,
    /// The window's inner size in points when last not maximized, and whether it is now.
    window_size: [f32; 2],
    maximized: bool,
    /// The box the ground grid is sized for: the document's last drawn extent, so the grid
    /// stays put when everything is hidden.
    ground: Option<(Vec3, Vec3)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>, profile: Profile, s: Settings, problem: Option<String>) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("VibeCAD needs the wgpu renderer");
        let ctx = cc.egui_ctx.clone();
        App {
            worker: start_worker(&ctx),
            ctx,
            freecad: None,
            worker_error: None,
            renderer: Renderer::new(rs),
            camera: Camera { ortho: s.ortho, ..Camera::default() },
            pending: HashMap::new(),
            doc_label: None,
            doc_path: None,
            tree: Tree::default(),
            scene: None,
            selection: None,
            props: None,
            edits: HashMap::new(),
            error: problem,
            info: None,
            show_edges: s.show_edges,
            edge_width: s.edge_width,
            palette: s.palette,
            show_log: s.show_log,
            show_grid: s.show_grid,
            navigation: s.navigation,
            open_on_ready: open,
            modified: false,
            tab: s.tab,
            view_rect: None,
            loading: None,
            opened_at: None,
            last_load: None,
            selftest: SelfTest::from_env(),
            profile,
            window_size: s.window_size,
            maximized: s.maximized,
            ground: None,
        }
    }

    fn request(&mut self, cmd: &str, args: Value, kind: Pending) {
        if self.worker_error.is_some() {
            self.error = Some("FreeCAD is not running: File → Restart FreeCAD".into());
            return;
        }
        let id = self.worker.send(cmd, args);
        if let Pending::Scene { fit } = kind {
            // A newer scene replaces the one streaming; the worker ends that one early, and
            // whatever of it still arrives is ignored.
            self.loading = Some(Loading {
                id,
                fit,
                started: Instant::now(),
                progress: None,
                counting_since: None,
                held: None,
                boxes_ms: None,
                first_meshes_ms: None,
                batches: 0,
                fitted: None,
            });
        }
        self.pending.insert(id, kind);
    }

    /// True when there is nothing unsaved, or the user agrees to lose it.
    fn may_discard(&self) -> bool {
        !self.modified
            || rfd::MessageDialog::new()
                .set_title("VibeCAD")
                .set_description("The document has unsaved changes. Discard them?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .set_level(rfd::MessageLevel::Warning)
                .show()
                == rfd::MessageDialogResult::Yes
    }

    /// A fresh FreeCAD process, reopening the document from its file.
    fn restart(&mut self) {
        if !self.may_discard() {
            return;
        }
        self.worker = start_worker(&self.ctx);
        self.freecad = None;
        self.worker_error = None;
        self.pending.clear();
        self.loading = None;
        self.open_on_ready = self.doc_path.clone();
        self.modified = false;
        self.error = None;
    }

    fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    // ---- actions ---------------------------------------------------------------------

    fn open(&mut self, path: PathBuf) {
        self.opened_at = Some(Instant::now());
        let arg = json!({ "path": path.to_string_lossy() });
        self.request("open", arg, Pending::Open(path));
    }

    fn open_dialog(&mut self) {
        if !self.may_discard() {
            return;
        }
        if let Some(p) = rfd::FileDialog::new().add_filter("FreeCAD document", &["FCStd", "fcstd", "FCSTD"]).pick_file() {
            self.open(p);
        }
    }

    fn save(&mut self, choose: bool) {
        if self.doc_label.is_none() {
            return;
        }
        let path = match (&self.doc_path, choose) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new()
                .add_filter("FreeCAD document", &["FCStd"])
                .set_file_name(format!("{}.FCStd", self.doc_label.as_deref().unwrap_or("model")))
                .save_file()
                .map(|p| with_default_extension(p, "FCStd")),
        };
        if let Some(p) = path {
            self.request("save_as", json!({ "path": p.to_string_lossy() }), Pending::Save);
            self.doc_path = Some(p);
        }
    }

    fn export_dialog(&mut self) {
        if self.doc_label.is_none() {
            return;
        }
        let stem = self.doc_label.clone().unwrap_or_else(|| "model".into());
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("STEP", &["step", "stp"])
            .add_filter("STL mesh", &["stl"])
            .add_filter("3MF", &["3mf"])
            .add_filter("OBJ", &["obj"])
            .set_file_name(format!("{stem}.step"))
            .save_file()
            .map(|p| with_default_extension(p, "step"))
        {
            self.request("export", json!({ "path": p.to_string_lossy() }), Pending::Export);
        }
    }

    /// After anything that may change the model: reload tree, shapes and the open property list.
    fn refresh(&mut self) {
        self.request("tree", json!({}), Pending::Tree);
        self.request("scene", json!({}), Pending::Scene { fit: false });
        if let Some(sel) = &self.selection {
            let obj = sel.obj.clone();
            self.request("props", json!({ "obj": obj }), Pending::Props);
        }
    }

    fn undo(&mut self, redo: bool) {
        if self.doc_label.is_some() {
            self.request(if redo { "redo" } else { "undo" }, json!({}), Pending::Edit);
        }
    }

    fn select(&mut self, sel: Option<Selection>) {
        let changed_obj = self.selection.as_ref().map(|s| &s.obj) != sel.as_ref().map(|s| &s.obj);
        self.selection = sel;
        if changed_obj {
            self.props = None;
            self.edits.clear();
            if let Some(s) = &self.selection {
                let obj = s.obj.clone();
                self.request("props", json!({ "obj": obj }), Pending::Props);
            }
        }
    }

    fn set_prop(&mut self, obj: &str, prop: &str, value: Value) {
        self.request("set_prop", json!({ "obj": obj, "prop": prop, "value": value }), Pending::Edit);
    }

    /// Select what is under a pixel of the 3D view (or clear the selection on empty space).
    fn pick_at(&mut self, px: u32, py: u32) -> Option<String> {
        let id = self.renderer.pick(px, py);
        let picked = self.scene.as_ref().and_then(|s| {
            let f = s.data.faces.get((id as usize).checked_sub(1)?)?;
            let inst = &s.data.instances[f.instance as usize];
            let sub = format!("Face{}", f.face + 1);
            Some((Selection { obj: inst.obj.clone(), sub: Some(sub.clone()), face_id: id }, format!("{}.{sub}", inst.label)))
        });
        let text = picked.as_ref().map(|(_, t)| t.clone());
        self.info = text.as_ref().map(|t| format!("picked {t}"));
        self.select(picked.map(|(sel, _)| sel));
        text
    }

    fn fit(&mut self) {
        if let Some((lo, hi)) = self.scene.as_ref().and_then(|s| s.data.bbox) {
            self.camera.fit(lo, hi);
        }
    }

    /// The camera, to tell whether the user moved it.
    fn camera_state(&self) -> [f32; 8] {
        let c = &self.camera;
        [c.target.x, c.target.y, c.target.z, c.distance, c.yaw, c.pitch, c.ortho as u8 as f32, c.aspect]
    }

    // ---- worker replies ----------------------------------------------------------------

    fn handle_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.worker.events.try_recv() {
            // New content changes the size of the floating panels; they settle one frame later.
            ctx.request_repaint();
            match ev {
                Event::Ready { freecad } => {
                    self.freecad = Some(freecad);
                    if let Some(p) = self.open_on_ready.take() {
                        self.open(p);
                    }
                }
                Event::Died(e) => {
                    self.worker_error = Some(e);
                    self.pending.clear();
                    self.loading = None;
                }
                Event::Stream { id, part } => self.on_stream(id, part),
                Event::Reply(r) => {
                    let Some(kind) = self.pending.remove(&r.id) else { continue };
                    match r.result {
                        Err(e) => {
                            self.error = Some(e);
                            if self.loading.as_ref().is_some_and(|l| l.id == r.id) {
                                self.loading = None;
                            }
                            // A failed edit may still have touched the model.
                            if matches!(kind, Pending::Edit) {
                                self.refresh();
                            }
                        }
                        Ok(body) => self.on_reply(ctx, r.id, kind, body),
                    }
                }
            }
        }
    }

    fn on_reply(&mut self, ctx: &egui::Context, id: u64, kind: Pending, body: Body) {
        let Body::Json(v) = body;
        {
            match kind {
                Pending::Open(path) => {
                    self.doc_label = v["label"].as_str().map(str::to_string);
                    self.doc_path = Some(path);
                    self.selection = None;
                    self.props = None;
                    self.scene = None;
                    self.modified = false;
                    self.error = None;
                    let title = format!("VibeCAD — {}", self.doc_label.as_deref().unwrap_or(""));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
                    self.request("tree", json!({}), Pending::Tree);
                    self.request("scene", json!({}), Pending::Scene { fit: true });
                }
                Pending::Tree => self.on_tree(&v),
                Pending::Props => {
                    if self.selection.as_ref().is_some_and(|s| Some(s.obj.as_str()) == v["obj"].as_str()) {
                        self.edits.clear();
                        self.props = Some(v);
                    }
                }
                Pending::Edit => {
                    if let Some(broken) = v["broken"].as_array().filter(|b| !b.is_empty()) {
                        let names: Vec<&str> = broken.iter().filter_map(Value::as_str).collect();
                        self.error = Some(format!("recompute left these objects invalid: {}", names.join(", ")));
                    }
                    self.modified = true;
                    self.refresh();
                }
                Pending::Save => {
                    self.modified = false;
                    let restored = v["gui_entries_restored"].as_array().map_or(0, Vec::len);
                    self.info = Some(format!(
                        "saved {}{}",
                        v["path"].as_str().unwrap_or(""),
                        if restored > 0 { " (colours kept)" } else { "" }
                    ));
                }
                Pending::Export => {
                    self.info = Some(format!("exported {} objects to {}", v["objects"], v["path"].as_str().unwrap_or("")));
                }
                Pending::Scene { .. } => self.on_scene_done(id, &v),
            }
        }
    }

    fn on_tree(&mut self, v: &Value) {
        let mut t = Tree {
            roots: v["roots"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            undo: v["undo"].as_u64().unwrap_or(0),
            redo: v["redo"].as_u64().unwrap_or(0),
            ..Default::default()
        };
        for n in v["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let name = n["name"].as_str().unwrap_or("").to_string();
            t.nodes.insert(name, Node {
                label: n["label"].as_str().unwrap_or("").to_string(),
                kind: n["type"].as_str().unwrap_or("").to_string(),
                children: n["children"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                visible: n["visible"].as_bool().unwrap_or(true),
                error: n["error"].as_bool().unwrap_or(false),
            });
        }
        t.build_history();
        if self.selection.as_ref().is_some_and(|s| !t.nodes.contains_key(&s.obj)) {
            self.selection = None;
            self.props = None;
        }
        self.tree = t;
    }

    /// A frame of the scene stream being received; frames of a replaced stream are dropped.
    fn on_stream(&mut self, id: u64, part: Result<Part, String>) {
        let Some(l) = self.loading.as_mut().filter(|l| l.id == id) else { return };
        let since = l.started.elapsed().as_secs_f64() * 1e3;
        match part {
            Err(e) => self.error = Some(e),
            Ok(Part::Boxes(boxes)) => {
                l.boxes_ms = Some(since);
                // Opening shows the boxes at once; a refresh keeps the old scene until meshes come.
                if l.fit || self.scene.is_none() {
                    self.show_layout(*boxes);
                } else {
                    l.held = Some(boxes);
                }
            }
            Ok(Part::Progress(p)) => self.note_progress(p),
            Ok(Part::Meshes(batch, p)) => {
                if l.first_meshes_ms.is_none() {
                    l.first_meshes_ms = Some(since);
                }
                l.batches += 1;
                if let Some(boxes) = l.held.take() {
                    self.show_layout(*boxes);
                }
                self.note_progress(p);
                let Some(s) = self.scene.as_mut() else { return };
                s.build_ms += batch.decode_ms;
                match s.data.add_meshes(&batch) {
                    Ok(placed) => {
                        if let Err(e) = self.renderer.add_meshes(&s.data, &batch, &placed) {
                            self.error = Some(e);
                        }
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
                self.find_selected_face();
            }
        }
    }

    fn note_progress(&mut self, p: Progress) {
        if let Some(l) = self.loading.as_mut() {
            if l.counting_since.is_none() {
                l.counting_since = Some((Instant::now(), p.done_faces));
            }
            l.progress = Some(p);
        }
    }

    /// Show a new scene as boxes: every instance in its place, before any mesh.
    fn show_layout(&mut self, data: SceneData) {
        if let Err(e) = self.renderer.set_layout(&data) {
            self.error = Some(e);
        }
        let by_obj = data.instances.iter().enumerate().map(|(i, inst)| (inst.obj.clone(), i as u32 + 1)).collect();
        self.scene = Some(SceneIndex { data, by_obj, build_ms: 0.0 });
        // Face ids are given out as meshes arrive; find the picked face again by name then.
        if let Some(sel) = self.selection.as_mut() {
            sel.face_id = 0;
        }
        if let Some(l) = self.loading.as_mut()
            && l.fit
        {
            // Another document: its own extent, not the last one's.
            self.ground = None;
            if let Some((lo, hi)) = self.scene.as_ref().and_then(|s| s.data.bbox) {
                self.camera.fit(lo, hi);
            }
            let state = self.camera_state();
            if let Some(l) = self.loading.as_mut() {
                l.fitted = Some(state);
            }
        }
    }

    /// The selected face's id, once its mesh is here (ids are numbered in arrival order).
    fn find_selected_face(&mut self) {
        let (Some(sel), Some(s)) = (self.selection.as_mut(), self.scene.as_ref()) else { return };
        if sel.face_id != 0 {
            return;
        }
        let Some(face) = sel.sub.as_deref().and_then(|t| t.strip_prefix("Face")?.parse::<u32>().ok()?.checked_sub(1)) else { return };
        let Some(&inst) = s.by_obj.get(&sel.obj) else { return };
        if let Some(base) = s.data.instances[inst as usize - 1].face_base {
            sel.face_id = base + face + 1;
        }
    }

    /// The scene stream ended: the worker's final reply.
    fn on_scene_done(&mut self, id: u64, v: &Value) {
        let Some(mut l) = self.loading.take_if(|l| l.id == id) else { return };
        if v["cancelled"].as_bool() == Some(true) {
            return; // a newer scene is on its way
        }
        if let Some(boxes) = l.held.take() {
            self.show_layout(*boxes); // a scene without meshes
        }
        let failed: Vec<u32> = v["failed_meshes"].as_array().map(|a| a.iter().filter_map(|x| x.as_u64()).map(|x| x as u32).collect()).unwrap_or_default();
        if let Some(s) = self.scene.as_mut()
            && !failed.is_empty()
        {
            let dropped = s.data.drop_meshes(&failed);
            self.renderer.drop_boxes(&dropped);
        }
        let skipped: Vec<String> = v["skipped"]
            .as_array()
            .map(|a| a.iter().map(|x| format!("{}: {}", x["obj"].as_str().unwrap_or("?"), x["error"].as_str().unwrap_or(""))).collect())
            .unwrap_or_default();
        if !skipped.is_empty() {
            self.error = Some(format!("not drawn: {}", skipped.join("; ")));
        }
        // Fitted to the boxes while loading: fit to the exact bounds now, if the view was left alone.
        if l.fitted.is_some_and(|f| f == self.camera_state()) {
            self.fit();
        }
        // Times count from the scene request; the open before it took `scene_after_open_ms`.
        let after_open = self.opened_at.filter(|_| l.fit).map(|t| l.started.duration_since(t).as_secs_f64() * 1e3);
        self.last_load = Some(json!({
            "scene_after_open_ms": after_open,
            "boxes_ms": l.boxes_ms,
            "first_meshes_ms": l.first_meshes_ms,
            "done_ms": l.started.elapsed().as_secs_f64() * 1e3,
            "batches": l.batches,
            "worker": v["stats"],
        }));
    }

    // ---- UI ----------------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let sc = |m, k| KeyboardShortcut::new(m, k);
        let fire = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        if fire(sc(cmd, egui::Key::O)) {
            self.open_dialog();
        }
        if fire(sc(cmd | Modifiers::SHIFT, egui::Key::S)) {
            self.save(true);
        } else if fire(sc(cmd, egui::Key::S)) {
            self.save(false);
        }
        if fire(sc(cmd | Modifiers::SHIFT, egui::Key::Z)) || fire(sc(cmd, egui::Key::Y)) {
            self.undo(true);
        } else if fire(sc(cmd, egui::Key::Z)) {
            self.undo(false);
        }
        if fire(sc(cmd, egui::Key::E)) {
            self.export_dialog();
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |k| ctx.input(|i| i.key_pressed(k) && i.modifiers.is_none());
        if pressed(egui::Key::F) {
            self.fit();
        }
        for (k, v) in [(egui::Key::Num0, View::Iso), (egui::Key::Num1, View::Front), (egui::Key::Num2, View::Top), (egui::Key::Num3, View::Right)] {
            if pressed(k) {
                self.camera.set_view(v);
            }
        }
        if pressed(egui::Key::O) {
            self.camera.ortho = !self.camera.ortho;
        }
        if pressed(egui::Key::Escape) {
            self.select(None);
        }
    }

    fn ribbon(&mut self, ui: &mut egui::Ui) {
        let state = RibbonState {
            has_doc: self.doc_label.is_some(),
            can_open: self.freecad.is_some() && self.worker_error.is_none(),
            undo: self.tree.undo,
            redo: self.tree.redo,
            ortho: self.camera.ortho,
            edges: self.show_edges,
            log: self.show_log,
            grid: self.show_grid,
            palette: self.palette,
            edge_width: self.edge_width,
            navigation: self.navigation,
        };
        let (busy, freecad, dead) = (self.busy(), self.freecad.clone(), self.worker_error.is_some());
        let actions = ui_chrome::ribbon(ui, &mut self.tab, &state, |ui| match (dead, freecad) {
            (true, _) => {
                ui.colored_label(Color32::LIGHT_RED, "FreeCAD stopped");
            }
            (false, None) => {
                ui.spinner();
                ui.label("starting FreeCAD…");
            }
            (false, Some(v)) => {
                if busy {
                    ui.spinner();
                }
                ui.weak(format!("FreeCAD {v}"));
            }
        });
        for a in actions {
            match a {
                Action::Open => self.open_dialog(),
                Action::Save => self.save(false),
                Action::SaveAs => self.save(true),
                Action::Export => self.export_dialog(),
                Action::Restart => self.restart(),
                Action::Quit => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                Action::Undo => self.undo(false),
                Action::Redo => self.undo(true),
                Action::Fit => self.fit(),
                Action::Projection(ortho) => self.camera.ortho = ortho,
                Action::ToggleEdges => self.show_edges = !self.show_edges,
                Action::ToggleLog => self.show_log = !self.show_log,
                Action::ToggleGrid => self.show_grid = !self.show_grid,
                Action::TogglePalette => self.palette = !self.palette,
                Action::EdgeWidth(w) => self.edge_width = w,
                Action::Navigation(n) => self.navigation = n,
            }
        }
    }

    /// The model tree and the property list, floating translucent over the 3D view.
    fn overlays(&mut self, ctx: &egui::Context) {
        let Some(view) = self.view_rect else { return };
        let title = self.doc_label.clone().unwrap_or_else(|| "No document".into());
        ui_chrome::overlay(ctx, "tree", view, view.left_top() + egui::vec2(10.0, 10.0), egui::Align2::LEFT_TOP, 260.0, view.height() - 20.0, &title, |ui| {
            self.tree_panel(ui)
        });
        if self.selection.is_some() {
            let top = view.right_top() + egui::vec2(-10.0, 150.0);
            ui_chrome::overlay(ctx, "props", view, top, egui::Align2::RIGHT_TOP, 290.0, view.height() - 200.0, "Properties", |ui| {
                self.props_panel(ui)
            });
        }
    }

    fn tree_panel(&mut self, ui: &mut egui::Ui) {
        let roots = self.tree.roots.clone();
        let mut actions = Vec::new();
        let mut seen = HashSet::new();
        for r in &roots {
            self.tree_node(ui, r, 0, &mut seen, &mut actions);
        }
        for a in actions {
            match a {
                TreeAction::Select(obj) => self.select(Some(Selection { obj, sub: None, face_id: 0 })),
                TreeAction::Visible(obj, v) => {
                    if let Some(n) = self.tree.nodes.get_mut(&obj) {
                        n.visible = v;
                    }
                    self.request("set_visible", json!({ "obj": obj, "visible": v }), Pending::Edit);
                }
            }
        }
    }

    fn tree_node(&self, ui: &mut egui::Ui, name: &str, depth: usize, seen: &mut HashSet<String>, actions: &mut Vec<TreeAction>) {
        let Some(node) = self.tree.nodes.get(name) else { return };
        if !seen.insert(name.to_string()) || depth > 32 {
            return;
        }
        // A body is lit while one of its features is selected in the timeline or the view.
        let selected = self.selection.as_ref().is_some_and(|s| s.obj == name || self.tree.owner.get(&s.obj).is_some_and(|b| b == name));
        let mut row = |ui: &mut egui::Ui| {
            let mut vis = node.visible;
            if ui.checkbox(&mut vis, "").on_hover_text("Show in the 3D view").changed() {
                actions.push(TreeAction::Visible(name.to_string(), vis));
            }
            let kind = ui_chrome::object_kind(&node.kind);
            let tip = format!("{}\n{}\n\n{name} · {}", node.label, kind.about, node.kind);
            ui_chrome::kind_badge(ui, kind.family, !node.visible).on_hover_text(&tip);
            // The label, then what the object is, smaller and greyed.
            let visuals = &ui.style().visuals;
            let label_color = if node.error {
                Color32::LIGHT_RED
            } else if node.visible {
                visuals.text_color()
            } else {
                visuals.weak_text_color()
            };
            let body = egui::TextStyle::Body.resolve(ui.style());
            let mut text = egui::text::LayoutJob::default();
            text.append(&node.label, 0.0, egui::TextFormat::simple(body.clone(), label_color));
            if !kind.name.is_empty() {
                let small = egui::FontId::new(body.size * 0.8, body.family);
                text.append(kind.name, 6.0, egui::TextFormat::simple(small, visuals.weak_text_color()));
            }
            // Cut long names to the panel's width; the full one is in the tooltip.
            if ui.add(egui::Button::selectable(selected, text).truncate()).on_hover_text(tip).clicked() {
                actions.push(TreeAction::Select(name.to_string()));
            }
        };
        if node.children.is_empty() || node.kind == BODY {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                row(ui);
            });
        } else {
            let id = ui.make_persistent_id(("tree", name));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, depth == 0)
                .show_header(ui, row)
                .body(|ui| {
                    for c in &node.children {
                        self.tree_node(ui, c, depth + 1, seen, actions);
                    }
                });
        }
    }

    /// Along the bottom, as in Fusion: every body's features in the order they are built.
    fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        let mut chosen = None;
        egui::ScrollArea::horizontal().auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let several = self.tree.history.first().map(|f| &self.tree.owner[f]) != self.tree.history.last().map(|f| &self.tree.owner[f]);
                let mut body: Option<&String> = None;
                for f in &self.tree.history {
                    let Some(node) = self.tree.nodes.get(f) else { continue };
                    let owner = &self.tree.owner[f];
                    if several && body != Some(owner) {
                        if body.is_some() {
                            ui.add_space(6.0);
                            ui.separator();
                        }
                        let label = self.tree.nodes.get(owner).map_or(owner.as_str(), |n| n.label.as_str());
                        ui.weak(label);
                        body = Some(owner);
                    }
                    let kind = ui_chrome::object_kind(&node.kind);
                    let selected = self.selection.as_ref().is_some_and(|s| &s.obj == f);
                    let tip = format!("{}  ({})
{}", node.label, if kind.name.is_empty() { &node.kind } else { kind.name }, kind.about);
                    if ui_chrome::timeline_step(ui, kind.family, selected, node.error).on_hover_text(tip).clicked() {
                        chosen = Some(f.clone());
                    }
                }
            });
        });
        if let Some(obj) = chosen {
            self.select(Some(Selection { obj, sub: None, face_id: 0 }));
        }
    }

    fn props_panel(&mut self, ui: &mut egui::Ui) {
        let Some(props) = self.props.clone() else {
            ui.weak(if self.selection.is_some() { "loading…" } else { "Select an object in the tree or the view." });
            return;
        };
        let obj = props["obj"].as_str().unwrap_or("").to_string();
        ui.strong(props["label"].as_str().unwrap_or(""));
        ui.weak(format!("{obj} · {}", props["type"].as_str().unwrap_or("")));
        if let Some(sub) = self.selection.as_ref().and_then(|s| s.sub.clone()) {
            ui.label(format!("picked {sub}"));
        }
        for e in props["errors"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            ui.colored_label(Color32::LIGHT_RED, e.as_str().unwrap_or(""));
        }
        ui.separator();
        let list = props["props"].as_array().cloned().unwrap_or_default();
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for p in list {
            let g = p["group"].as_str().unwrap_or("Base").to_string();
            match groups.iter_mut().find(|(n, _)| *n == g) {
                Some((_, v)) => v.push(p),
                None => groups.push((g, vec![p])),
            }
        }
        let mut commits: Vec<(String, Value)> = Vec::new();
        for (group, items) in &groups {
            egui::CollapsingHeader::new(group).default_open(true).show(ui, |ui| {
                egui::Grid::new(("props", group)).num_columns(2).striped(true).show(ui, |ui| {
                    for p in items {
                        self.prop_row(ui, p, &mut commits);
                        ui.end_row();
                    }
                });
            });
        }
        for (name, value) in commits {
            self.set_prop(&obj, &name, value);
        }
    }

    fn prop_row(&mut self, ui: &mut egui::Ui, p: &Value, commits: &mut Vec<(String, Value)>) {
        let name = p["name"].as_str().unwrap_or("").to_string();
        ui.label(&name).on_hover_text(p["type"].as_str().unwrap_or(""));
        let kind = p["kind"].as_str().unwrap_or("text");
        if p["readonly"].as_bool() == Some(true) || kind == "text" {
            let text = p["text"].as_str().unwrap_or("");
            ui.add(egui::Label::new(text).truncate()).on_hover_text(text);
            return;
        }
        let buf = self.edits.entry(name.clone()).or_insert_with(|| match kind {
            "float" => EditBuf::Float(p["value"].as_f64().unwrap_or(0.0)),
            "int" => EditBuf::Int(p["value"].as_i64().unwrap_or(0)),
            "bool" => EditBuf::Bool(p["value"].as_bool().unwrap_or(false)),
            "enum" => EditBuf::Enum(p["value"].as_str().unwrap_or("").to_string()),
            _ => EditBuf::Text(p["value"].as_str().unwrap_or("").to_string()),
        });
        match buf {
            EditBuf::Float(v) => {
                let unit = p["unit"].as_str().unwrap_or("");
                let speed = (v.abs() * 0.01).clamp(0.01, 10.0);
                // Typed values apply on Enter or when the field loses focus, not per keystroke.
                let mut dv = egui::DragValue::new(v).speed(speed).max_decimals(4).update_while_editing(false);
                if !unit.is_empty() {
                    dv = dv.suffix(format!(" {unit}"));
                }
                let r = ui.add(dv);
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    commits.push((name, json!(*v)));
                }
            }
            EditBuf::Int(v) => {
                let r = ui.add(egui::DragValue::new(v).speed(0.2).update_while_editing(false));
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    commits.push((name, json!(*v)));
                }
            }
            EditBuf::Bool(v) => {
                if ui.checkbox(v, "").changed() {
                    commits.push((name, json!(*v)));
                }
            }
            EditBuf::Enum(v) => {
                let options: Vec<String> = p["options"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                let before = v.clone();
                egui::ComboBox::from_id_salt(("enum", &name)).selected_text(v.as_str()).show_ui(ui, |ui| {
                    for o in &options {
                        ui.selectable_value(v, o.clone(), o);
                    }
                });
                if *v != before {
                    commits.push((name, json!(v.clone())));
                }
            }
            EditBuf::Text(v) => {
                let r = ui.text_edit_singleline(v);
                if r.lost_focus() && v.as_str() != p["value"].as_str().unwrap_or("") {
                    commits.push((name, json!(v.clone())));
                }
            }
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = self.worker_error.clone() {
            ui.colored_label(Color32::LIGHT_RED, format!("FreeCAD worker: {e}"));
        }
        ui.horizontal(|ui| {
            if let Some(e) = self.error.clone() {
                if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                    self.error = None;
                }
                ui.colored_label(Color32::LIGHT_RED, e);
            } else if let Some(i) = &self.info {
                ui.label(i);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(s) = &self.scene {
                    // Drawn: what the last frame kept after culling the objects out of view.
                    let r = self.renderer.stats;
                    let frame_ms = ui.input(|i| i.stable_dt) * 1e3;
                    ui.weak(format!("drawn {}/{} objects, {} triangles · frame {frame_ms:.1} ms", r.drawn_instances, s.data.instances.len(), r.drawn_triangles))
                        .on_hover_text("Objects whose box is outside the view are not drawn. Frame: time of the last window frame.");
                    let (loaded, meshes) = s.data.loaded();
                    ui.weak(format!(
                        "{} objects · {} faces · {} triangles ({loaded} of {meshes} unique shapes) ·",
                        s.data.instances.len(),
                        s.data.faces.len(),
                        s.data.triangles(),
                    ));
                }
            });
        });
        if self.show_log {
            ui.separator();
            let lines: Vec<String> = self.worker.log.lock().unwrap().iter().cloned().collect();
            egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                for l in lines {
                    ui.monospace(l);
                }
            });
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui) {
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let ppp = ui.ctx().pixels_per_point();
        let size = ((rect.width() * ppp).round().max(1.0) as u32, (rect.height() * ppp).round().max(1.0) as u32);
        self.camera.aspect = rect.width() / rect.height().max(1.0);
        if let Some(b) = self.scene.as_ref().and_then(|s| s.data.bbox) {
            self.ground = Some(b);
        }

        let d = resp.drag_delta();
        let delta = Vec2::new(d.x, d.y);
        if resp.dragged() {
            let held = ui.input(|i| Held {
                left: i.pointer.button_down(PointerButton::Primary),
                middle: i.pointer.button_down(PointerButton::Middle),
                right: i.pointer.button_down(PointerButton::Secondary),
                shift: i.modifiers.shift,
                ctrl: i.modifiers.command,
            });
            match self.navigation.gesture(held) {
                Some(Gesture::Orbit) => self.camera.orbit(delta),
                Some(Gesture::Look) => self.camera.look(delta),
                Some(Gesture::Pan) => self.camera.pan(delta, rect.height()),
                // Up zooms in, about twice per 100 points.
                Some(Gesture::Zoom) => self.camera.zoom(-delta.y * 4.6),
                None => {}
            }
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.camera.zoom(scroll);
            }
        }
        if resp.double_clicked() {
            self.fit();
        } else if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.pick_at(((pos.x - rect.min.x) * ppp) as u32, ((pos.y - rect.min.y) * ppp) as u32);
        }

        let dark = ui.visuals().dark_mode;
        let f = self.frame(size, if dark { [0.035, 0.04, 0.05] } else { BACKGROUND });
        self.renderer.render(size, &f);
        if let Some(tex) = self.renderer.texture_id {
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            ui.painter().image(tex, rect, uv, Color32::WHITE);
        }
        if let Some(grid) = f.grid {
            ui_chrome::ground_labels(&ui.painter_at(rect), grid, rect, f.view_proj, f.forward);
        }
        self.view_rect = Some(rect);
        if let CubeAction::Home = ui_chrome::view_cube(ui, rect, &mut self.camera) {
            self.camera.set_view(View::Iso);
            self.fit();
        }

        let painter = ui.painter_at(rect);
        let hint = if self.worker_error.is_some() {
            Some("FreeCAD worker stopped — see the status line, or FILE → Restart")
        } else if self.freecad.is_none() {
            Some("Starting FreeCAD…")
        } else if self.doc_label.is_none() {
            Some("Open a FreeCAD document: FILE → Open (Ctrl+O)")
        } else if self.scene.is_none() {
            Some("Reading shapes…")
        } else {
            None
        };
        if let Some(h) = hint {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, h, egui::FontId::proportional(16.0), ui.visuals().weak_text_color());
        }
        self.progress_panel(ui, rect);
        painter.text(
            rect.left_bottom() + egui::vec2(8.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{} · double-click/F: fit · 0-3: views · cube: click a face, edge or corner", self.navigation.hint()),
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
    }
}

impl App {
    /// Over the bottom of the 3D view while a scene loads: how far, which part, time left.
    fn progress_panel(&self, ui: &egui::Ui, view: egui::Rect) {
        let Some(l) = &self.loading else { return };
        let Some(p) = &l.progress else { return };
        if p.total_faces == 0 || self.scene.is_none() {
            return;
        }
        let frac = (p.done_faces as f32 / p.total_faces as f32).clamp(0.0, 1.0);
        // Time left from the pace since counting started; cached shapes make it jump early on.
        let eta = l.counting_since.and_then(|(t0, d0)| {
            let (secs, done) = (t0.elapsed().as_secs_f64(), p.done_faces.saturating_sub(d0));
            (secs > 1.0 && done > 0).then(|| secs * (p.total_faces - p.done_faces) as f64 / done as f64)
        });
        let v = ui.visuals();
        let panel = egui::Rect::from_center_size(egui::pos2(view.center().x, view.bottom() - 64.0), egui::vec2(400.0_f32.min(view.width() - 20.0), 62.0));
        let painter = ui.painter_at(view);
        painter.rect(panel, 6.0, v.panel_fill.gamma_multiply(0.82), egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        let font = egui::FontId::proportional(12.0);
        let left = panel.left_top() + egui::vec2(10.0, 8.0);
        painter.text(left, egui::Align2::LEFT_TOP, format!("Loading shapes {:.0}%", frac * 100.0), font.clone(), v.text_color());
        if let Some(obj) = &p.obj {
            // Link paths run long; their end names the part.
            let chars: Vec<char> = obj.chars().collect();
            let shown: String = if chars.len() > 58 { std::iter::once('…').chain(chars[chars.len() - 57..].iter().copied()).collect() } else { obj.clone() };
            painter.text(left + egui::vec2(0.0, 17.0), egui::Align2::LEFT_TOP, shown, font.clone(), v.weak_text_color());
        }
        if let Some(eta) = eta {
            let text = if eta >= 60.0 { format!("about {} min left", (eta / 60.0).ceil()) } else { format!("about {} s left", eta.ceil().max(1.0)) };
            painter.text(panel.right_top() + egui::vec2(-10.0, 8.0), egui::Align2::RIGHT_TOP, text, font, v.weak_text_color());
        }
        let bar = egui::Rect::from_min_size(panel.left_bottom() + egui::vec2(10.0, -14.0), egui::vec2(panel.width() - 20.0, 6.0));
        painter.rect_filled(bar, 3.0, v.extreme_bg_color);
        painter.rect_filled(egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * frac, bar.height())), 3.0, v.selection.bg_fill);
    }

    fn capture(&self, t: &SelfTest, name: &str) -> Value {
        match self.renderer.read_color() {
            Some((w, h, px)) => {
                let path = t.dir.join(name);
                let ok = write_bmp(&path, w, h, &px).is_ok();
                json!({ "file": path.to_string_lossy(), "written": ok, "size": [w, h] })
            }
            None => json!({ "file": null }),
        }
    }

    fn top_z(&self) -> Option<f32> {
        self.scene.as_ref().and_then(|s| s.data.bbox).map(|(_, hi)| hi.z)
    }

    /// Points of the view, as fractions, inside the first drawn instance's box: its centre, and
    /// three quarters of the way to its far corner (the centre may be a hole).
    fn geometry_points(&self) -> Vec<(f32, f32)> {
        let Some(s) = self.scene.as_ref() else { return Vec::new() };
        let Some((lo, hi)) = s.data.instances.iter().find(|i| i.face_base.is_some()).and_then(|i| i.bbox) else { return Vec::new() };
        let Some((w, h)) = self.renderer.size() else { return Vec::new() };
        let vp = self.camera.view_proj(w as f32 / h.max(1) as f32, self.drawn_box());
        [0.5, 0.75]
            .into_iter()
            .map(|t| {
                let q = vp.project_point3(lo + (hi - lo) * t);
                ((q.x * 0.5 + 0.5).clamp(0.0, 1.0), (0.5 - q.y * 0.5).clamp(0.0, 1.0))
            })
            .collect()
    }

    fn selftest_step(&mut self, ctx: &egui::Context) {
        let Some(mut t) = self.selftest.take() else { return };
        if let Some(e) = &self.worker_error {
            t.log("worker", json!({ "error": e }));
            t.stage = Stage::Done;
        }
        // While the first scene streams in, keep a frame of the boxes and one of a partial scene.
        if t.stage == Stage::WaitScene
            && let Some(s) = &self.scene
        {
            let (loaded, meshes) = s.data.loaded();
            if loaded == 0 && meshes > 0 && !t.seen_boxes {
                t.seen_boxes = true;
                let shot = self.capture(&t, "0-boxes.bmp");
                t.log("boxes", json!({ "ms": self.loading.as_ref().map(|l| l.started.elapsed().as_secs_f64() * 1e3), "frame": shot }));
            } else if loaded > 0 && loaded < meshes && !t.seen_partial {
                t.seen_partial = true;
                let shot = self.capture(&t, "0-partial.bmp");
                let at = self.loading.as_ref().map(|l| l.started.elapsed().as_secs_f64() * 1e3);
                t.log("partial", json!({ "ms": at, "meshes": [loaded, meshes], "frame": shot }));
            }
        }
        // Each stage starts once the previous requests are answered and a few frames were drawn.
        if t.stage != Stage::Done && (self.busy() || self.scene.is_none()) {
            t.frames_idle = 0;
            self.selftest = Some(t);
            return;
        }
        t.frames_idle += 1;
        if t.frames_idle < 3 {
            self.selftest = Some(t);
            return;
        }
        t.frames_idle = 0;
        match t.stage {
            Stage::WaitScene => {
                let shot = self.capture(&t, "0-opened.bmp");
                let perf = self.perf();
                t.log("opened", json!({ "top_z": self.top_z(), "error": self.error, "frame": shot, "perf": perf, "load": self.last_load }));
                // Zoom into the middle of the model: most of it leaves the view.
                self.camera.distance *= 0.15;
                t.stage = Stage::Zoom;
            }
            Stage::Zoom => {
                self.render_now();
                let shot = self.capture(&t, "0-zoomed.bmp");
                let perf = self.perf();
                t.log("zoomed", json!({ "frame": shot, "perf": perf }));
                self.fit();
                t.stage = Stage::Pick;
            }
            Stage::Pick => {
                let (w, h) = self.renderer.size().unwrap_or((1, 1));
                let mut hits = Vec::new();
                // A grid of points, and points inside the first instance's box, which land on geometry.
                let mut points = vec![(0.5f32, 0.5f32), (0.62, 0.70), (0.02, 0.02)];
                points.extend(self.geometry_points());
                points.push((0.30, 0.55));
                for (fx, fy) in points {
                    let hit = self.pick_at((w as f32 * fx) as u32, (h as f32 * fy) as u32);
                    hits.push(json!({ "at": [fx, fy], "picked": hit, "face_id": self.selection.as_ref().map(|s| s.face_id) }));
                }
                // the last pick stays selected: draw it and keep the frame
                self.render_now();
                let shot = self.capture(&t, "1-picked.bmp");
                t.log("pick", json!({ "hits": hits, "frame": shot }));
                t.stage = Stage::SelectPad;
                self.select(Some(Selection { obj: "Pad".into(), sub: None, face_id: 0 }));
            }
            Stage::SelectPad => {
                let length = self
                    .props
                    .as_ref()
                    .and_then(|p| p["props"].as_array()?.iter().find(|q| q["name"] == "Length").cloned());
                t.log("props Pad", json!({ "Length": length }));
                self.set_prop("Pad", "Length", json!(12.0));
                t.stage = Stage::Edit;
            }
            Stage::Edit => {
                let shot = self.capture(&t, "2-length12.bmp");
                t.log("Pad.Length = 12", json!({ "top_z": self.top_z(), "undo": self.tree.undo, "error": self.error, "frame": shot }));
                self.undo(false);
                t.stage = Stage::Undo;
            }
            Stage::Undo => {
                t.log("undo", json!({ "top_z": self.top_z(), "undo": self.tree.undo, "redo": self.tree.redo }));
                t.stage = Stage::Done;
            }
            Stage::Done => {
                t.finish();
                self.modified = false; // nothing to ask about when the test closes the window
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        self.selftest = Some(t);
    }

    /// The frame the view draws now, at `size` pixels.
    fn frame(&self, size: (u32, u32), background: [f64; 3]) -> Frame {
        let selected_instance = self
            .selection
            .as_ref()
            .and_then(|s| self.scene.as_ref()?.by_obj.get(&s.obj).copied())
            .unwrap_or(0);
        let grid = self
            .show_grid
            // Nothing drawn yet in this document (all hidden, or empty): a 100 mm square.
            .then(|| self.ground.unwrap_or((Vec3::new(-50.0, -50.0, 0.0), Vec3::new(50.0, 50.0, 0.0))))
            .map(|(lo, hi)| GroundGrid::around(lo, hi).for_scale(self.camera.px_per_unit(size.1 as f32)));
        let mut drawn = self.drawn_box();
        if let Some(g) = grid {
            // The square is drawn a little past its outline.
            let h = g.half * 1.01;
            drawn = (drawn.0.min(Vec3::new(-h, -h, 0.0)), drawn.1.max(Vec3::new(h, h, 0.0)));
        }
        Frame {
            view_proj: self.camera.view_proj(size.0 as f32 / size.1.max(1) as f32, drawn),
            forward: self.camera.forward(),
            selected_face: self.selection.as_ref().map_or(0, |s| s.face_id),
            selected_instance,
            show_edges: self.show_edges,
            edge_width: self.edge_width,
            palette: self.palette,
            background,
            grid,
        }
    }

    /// The scene's box, or the camera's own sphere while there is no scene.
    fn drawn_box(&self) -> (Vec3, Vec3) {
        self.scene.as_ref().and_then(|s| s.data.bbox).unwrap_or_else(|| {
            let r = Vec3::splat(self.camera.radius);
            (self.camera.target - r, self.camera.target + r)
        })
    }

    /// Redraw the view with the current selection (the self-test captures between frames).
    fn render_now(&mut self) {
        if let Some(size) = self.renderer.size() {
            let f = self.frame(size, BACKGROUND);
            self.renderer.render(size, &f);
        }
    }

    /// What the scene and the current view cost, for the self-test report.
    fn perf(&mut self) -> Value {
        let Some(size) = self.renderer.size() else { return Value::Null };
        let f = self.frame(size, BACKGROUND);
        let frame_ms = self.renderer.bench(size, &f, 30);
        let s = self.renderer.stats;
        json!({
            "build_ms": self.scene.as_ref().map(|s| s.build_ms),
            "load": self.last_load,
            "upload_ms": s.upload_ms,
            "gpu_bytes": s.gpu_bytes,
            "drawn_instances": s.drawn_instances,
            "drawn_triangles": s.drawn_triangles,
            "triangles": self.scene.as_ref().map(|s| s.data.triangles()),
            "frame_ms": frame_ms,
            "size": [size.0, size.1],
        })
    }
}

/// The bottom of the view's light background; the top is near white.
const BACKGROUND: [f64; 3] = [0.84, 0.86, 0.89];

fn start_worker(ctx: &egui::Context) -> Worker {
    let ctx = ctx.clone();
    Worker::start(move || ctx.request_repaint())
}

/// Save dialogs on Linux do not add the extension of the chosen filter.
fn with_default_extension(p: PathBuf, ext: &str) -> PathBuf {
    if p.extension().is_some() { p } else { p.with_extension(ext) }
}

enum TreeAction {
    Select(String),
    Visible(String, bool),
}

impl App {
    /// The settings as they are now, for the profile.
    fn settings(&self) -> Settings {
        Settings {
            navigation: self.navigation,
            show_edges: self.show_edges,
            edge_width: self.edge_width,
            palette: self.palette,
            show_grid: self.show_grid,
            show_log: self.show_log,
            ortho: self.camera.ortho,
            tab: self.tab,
            window_size: self.window_size,
            maximized: self.maximized,
        }
    }

    /// Follow the window's size and keep the settings.
    fn keep_settings(&mut self, ctx: &egui::Context) {
        let (maximized, inner) = ctx.input(|i| (i.viewport().maximized, i.viewport().inner_rect));
        if let Some(m) = maximized {
            self.maximized = m;
        }
        // Minimized windows report a tiny or empty size; keep the last real one.
        if let Some(r) = inner.filter(|r| !self.maximized && r.width() >= 200.0 && r.height() >= 200.0) {
            self.window_size = [r.width().round(), r.height().round()];
        }
        let (wait, problem) = self.profile.keep(self.settings(), Instant::now());
        if let Some(w) = wait {
            ctx.request_repaint_after(w);
        }
        if problem.is_some() {
            self.error = problem;
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        self.profile.keep(self.settings(), Instant::now());
        if let Some(e) = self.profile.flush() {
            eprintln!("{e}");
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && self.selftest.is_none() && !self.may_discard() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.handle_events(&ctx);
        self.shortcuts(&ctx);
        egui::Panel::top("ribbon").show(ui, |ui| self.ribbon(ui));
        egui::Panel::bottom("status").resizable(false).show(ui, |ui| self.status_panel(ui));
        if !self.tree.history.is_empty() {
            egui::Panel::bottom("timeline").resizable(false).show(ui, |ui| self.timeline_panel(ui));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.viewport(ui));
        self.overlays(&ctx);
        self.keep_settings(&ctx);
        if self.selftest.is_some() {
            self.selftest_step(&ctx);
            ctx.request_repaint();
        }
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: &str, children: &[&str]) -> Node {
        Node { label: String::new(), kind: kind.into(), children: children.iter().map(|c| c.to_string()).collect(), visible: true, error: false }
    }

    #[test]
    fn the_timeline_holds_what_bodies_are_built_from_in_order() {
        let mut t = Tree { roots: vec!["Part".into(), "Box".into()], ..Default::default() };
        for (name, kind, children) in [
            ("Part", "App::Part", &["Body", "Body001"][..]),
            ("Body", BODY, &["Pad", "Pocket"]),
            ("Pad", "PartDesign::Pad", &["Sketch"]),
            ("Sketch", "Sketcher::SketchObject", &[]),
            ("Pocket", "PartDesign::Pocket", &["Sketch001"]),
            ("Sketch001", "Sketcher::SketchObject", &[]),
            ("Body001", BODY, &["Fillet"]),
            ("Fillet", "PartDesign::Fillet", &[]),
            ("Box", "Part::Box", &[]),
        ] {
            t.nodes.insert(name.into(), node(kind, children));
        }
        t.build_history();
        assert_eq!(t.history, ["Sketch", "Pad", "Sketch001", "Pocket", "Fillet"]);
        assert_eq!(t.owner["Pocket"], "Body");
        assert_eq!(t.owner["Fillet"], "Body001");
        assert!(!t.owner.contains_key("Box") && !t.owner.contains_key("Body"));
    }
}
