//! `VIBECAD_SELFTEST=<dir>`: drive the window through zoom, pick, select, edit, undo, an extrude,
//! a construction plane, a sketch drawn in sketch mode (with a picture of the whole window),
//! clicks on an edge and a corner, and a fillet made from the marking menu, with the same code
//! the mouse and the property panel use (the marking menu with Alt, pointer and keys given to
//! the window as input), write frames as BMP and a `report.json`, then quit. The edit steps are
//! meant for the sample `examples/plate.FCStd` (a Pad under a Pocket); the view and performance
//! steps work on any document.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Stage {
    WaitScene,
    Zoom,
    Pick,
    SelectPad,
    Edit,
    Undo,
    Extrude,
    Plane,
    /// A sketch on the plate's top face, drawn with the tools as clicks would draw it.
    Sketch,
    SketchDimension,
    SketchDraw,
    SketchShot,
    SketchSaved,
    /// An edge and a corner picked by clicks on them, then the marking menu: Alt held over the
    /// picked edge, the pointer moved left to Fillet, Alt let go, "2" and Enter typed in the
    /// size box, as input the window receives; then undone.
    EdgeAsk,
    EdgePick,
    VertexPick,
    WheelOpen,
    WheelPoint,
    WheelChoose,
    SizeType,
    Filleted,
    FilletUndo,
    Done,
}

/// Input the self-test gives the window, in the next frames, as if it came from the keyboard
/// and the mouse (see `App::raw_input_hook`).
#[derive(Default)]
pub struct Inject {
    /// The window taken as focused, as it is when a person holds Alt and types in it.
    pub focus: bool,
    pub events: Vec<eframe::egui::Event>,
}

/// A screenshot of the window: width, height, RGBA rows.
pub type Window = std::sync::Arc<std::sync::Mutex<Option<(u32, u32, Vec<u8>)>>>;

pub struct SelfTest {
    pub dir: PathBuf,
    pub stage: Stage,
    pub report: Value,
    pub frames_idle: u32,
    /// Frames kept while the first scene streamed in: boxes only, then some meshes.
    pub seen_boxes: bool,
    pub seen_partial: bool,
    /// The whole window as egui drew it (overlays included), once a screenshot came back.
    pub window: Window,
    /// A screenshot of the window was asked for and has not come yet.
    pub shot_asked: bool,
    pub inject: Inject,
    /// Where the picked edge is on screen, for the marking menu.
    pub edge_at: Option<eframe::egui::Pos2>,
}

impl SelfTest {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os("VIBECAD_SELFTEST")?);
        std::fs::create_dir_all(&dir).ok()?;
        Some(SelfTest { dir, stage: Stage::WaitScene, report: json!({ "steps": [] }), frames_idle: 0, seen_boxes: false, seen_partial: false, window: Window::default(), shot_asked: false, inject: Inject::default(), edge_at: None })
    }

    pub fn log(&mut self, step: &str, data: Value) {
        self.report["steps"].as_array_mut().unwrap().push(json!({ "step": step, "data": data }));
    }

    pub fn finish(&self) {
        let _ = std::fs::write(self.dir.join("report.json"), serde_json::to_vec_pretty(&self.report).unwrap());
    }
}

/// 32-bit BMP from RGBA rows, top row first.
pub fn write_bmp(path: &Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let size = 54 + w * h * 4;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up rows
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..h).rev() {
        let row = &rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    std::fs::write(path, out)
}
