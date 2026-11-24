//! The agent bridge: how an MCP client drives this window.
//!
//! `vibecad --mcp` (see `mcp.rs`) is what the client starts. It finds the window through
//! `agent.json` in VibeCAD's per-user directory (the port and a random token, written when the
//! window starts and removed when it closes) and sends each tool call as one line of JSON
//! over localhost. The window runs the call against its own FreeCAD worker, so the agent sees
//! the same document as the person at the screen, and the AGENTIC tab shows every call in its
//! feed as it happens.
//!
//!     request   {"token": "...", "tool": "slice", "args": {...}}
//!     reply     {"ok": true, "text": "...", "images": [{"mime": "image/png", "data": base64}]}

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::worker::random_hex;

/// A tool call longer than this gets an error; the window keeps working on it.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_REQUEST: u64 = 4 << 20;

/// A tool call from the agent, for the UI thread to answer through `reply`.
pub struct Call {
    pub tool: String,
    pub args: Value,
    reply: Sender<ToolResult>,
}

impl Call {
    pub fn answer(self, result: ToolResult) {
        let _ = self.reply.send(result);
    }
}

#[derive(Clone)]
pub struct ToolResult {
    pub text: String,
    pub images: Vec<Picture>,
    pub error: bool,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        ToolResult { text: text.into(), images: Vec::new(), error: false }
    }
    pub fn err(text: impl Into<String>) -> Self {
        ToolResult { text: text.into(), images: Vec::new(), error: true }
    }
}

/// An RGB image the agent gets as a PNG and the feed shows.
#[derive(Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

impl Picture {
    fn new(width: usize, height: usize, fill: [u8; 3]) -> Self {
        Picture { width, height, rgb: fill.repeat(width * height) }
    }

    pub fn png(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.width as u32, self.height as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        let ok = enc.write_header().and_then(|mut w| w.write_image_data(&self.rgb));
        if ok.is_err() {
            out.clear();
        }
        out
    }

    fn set(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) * 3;
            self.rgb[i..i + 3].copy_from_slice(&c);
        }
    }

    /// Side by side, tops aligned, with a gap between them.
    fn row(pictures: &[Picture], gap: usize) -> Picture {
        let w = pictures.iter().map(|p| p.width).sum::<usize>() + gap * pictures.len().saturating_sub(1);
        let h = pictures.iter().map(|p| p.height).max().unwrap_or(1);
        let mut out = Picture::new(w.max(1), h, BACKGROUND);
        let mut x0 = 0;
        for p in pictures {
            for y in 0..p.height {
                let src = &p.rgb[y * p.width * 3..(y + 1) * p.width * 3];
                let at = (y * out.width + x0) * 3;
                out.rgb[at..at + src.len()].copy_from_slice(src);
            }
            x0 += p.width + gap;
        }
        out
    }
}

/// Listens for the MCP side; dropping it takes the discovery file away.
pub struct Bridge {
    pub calls: Receiver<Call>,
    pub port: u16,
    file: PathBuf,
    token: String,
}

impl Bridge {
    pub fn start(repaint: impl Fn() + Send + Sync + 'static) -> Result<Bridge> {
        let listener = TcpListener::bind("127.0.0.1:0").context("listening for the agent")?;
        let port = listener.local_addr()?.port();
        let token = random_hex(2);
        let file = discovery_file().context("no directory for agent.json")?;
        write_private(&file, &serde_json::to_vec(&json!({ "port": port, "token": token, "pid": std::process::id() }))?)?;
        let (tx, calls) = channel();
        let repaint = std::sync::Arc::new(repaint);
        let want = token.clone();
        std::thread::Builder::new().name("agent-bridge".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let (tx, want, repaint) = (tx.clone(), want.clone(), repaint.clone());
                std::thread::spawn(move || {
                    let _ = serve_one(stream, &want, &tx, &|| repaint());
                });
            }
        })?;
        Ok(Bridge { calls, port, file, token })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Another window may have taken the file over since; leave its file alone.
        if std::fs::read_to_string(&self.file).is_ok_and(|s| s.contains(&self.token)) {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

fn serve_one(stream: TcpStream, token: &str, calls: &Sender<Call>, repaint: &dyn Fn()) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line)?;
    let req: Value = serde_json::from_str(&line).context("request is not JSON")?;
    let result = if req["token"].as_str() != Some(token) {
        ToolResult::err("wrong token: agent.json belongs to another VibeCAD window")
    } else {
        let (reply, answer) = channel();
        let tool = req["tool"].as_str().unwrap_or("").to_string();
        calls.send(Call { tool, args: req["args"].clone(), reply })?;
        repaint();
        answer.recv_timeout(CALL_TIMEOUT).unwrap_or_else(|_| ToolResult::err("VibeCAD did not answer in time"))
    };
    let images: Vec<Value> = result
        .images
        .iter()
        .map(|p| json!({ "mime": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(p.png()) }))
        .collect();
    let mut out = serde_json::to_vec(&json!({ "ok": !result.error, "text": result.text, "images": images }))?;
    out.push(b'\n');
    (&stream).write_all(&out)?;
    Ok(())
}

/// Where the window says how to reach it: next to VibeCAD's cache directory.
pub fn discovery_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("VibeCAD")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/VibeCAD")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache")))?
            .join("vibecad")
    };
    Some(base.join("agent.json"))
}

/// Written so only this user can read it (the token is in it), replacing any older file.
fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("agent.json has no directory")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(&tmp)?.write_all(data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---- the feed --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    Running,
    Done,
    Failed,
    /// Paused by the person watching: the call was not run.
    Refused,
}

pub struct FeedEntry {
    pub tool: String,
    /// The arguments, as one line of JSON.
    pub args: String,
    pub started: Instant,
    /// Seconds since the window started, for the time column.
    pub at: f64,
    pub status: Status,
    pub took: Option<Duration>,
    pub text: String,
    pub images: Vec<eframe::egui::TextureHandle>,
}

// ---- slices as pictures ----------------------------------------------------------------

const BACKGROUND: [u8; 3] = [255, 255, 255];
const FRAME: [u8; 3] = [205, 210, 218];
/// Layers of a cut: a plain section, and what a comparison with an earlier state splits it into.
pub const LAYERS: [(&str, [u8; 3]); 4] =
    [("section", [96, 120, 156]), ("same", [168, 172, 180]), ("added", [38, 166, 84]), ("removed", [222, 64, 52])];

/// One picture of a `slice` reply: its cuts as tiles, left to right and then down, all at the
/// same scale and over the same (u, v) bounds so they compare at a glance.
pub fn render_slices(v: &Value, max_width: usize) -> Option<Picture> {
    let b: Vec<f64> = v["bounds"].as_array()?.iter().filter_map(Value::as_f64).collect();
    let slices = v["slices"].as_array()?;
    if b.len() != 4 || slices.is_empty() {
        return None;
    }
    let (du, dv) = ((b[2] - b[0]).max(1e-9), (b[3] - b[1]).max(1e-9));
    let n = slices.len();
    let cols = n.min(4);
    let rows = n.div_ceil(cols);
    let gap = 6;
    let tile_w = ((max_width - gap * (cols - 1)) / cols).clamp(96, 512);
    // Room around the outline, then as tall as the bounds are, within reason.
    let scale = (tile_w as f64 * 0.9) / du;
    let scale = scale.min(512.0 * 0.9 / dv);
    let (tw, th) = ((du * scale / 0.9).ceil() as usize, (dv * scale / 0.9).ceil() as usize);
    let (tw, th) = (tw.max(32), th.max(32));
    let mut out = Picture::new(cols * tw + gap * (cols - 1), rows * th + gap * (rows - 1), BACKGROUND);
    for (i, s) in slices.iter().enumerate() {
        let (x0, y0) = ((i % cols) * (tw + gap), (i / cols) * (th + gap));
        // Bounds centred in the tile, v pointing up.
        let (ou, ov) = (x0 as f64 + (tw as f64 - du * scale) / 2.0, y0 as f64 + (th as f64 - dv * scale) / 2.0);
        let to_px = |u: f64, vv: f64| (ou + (u - b[0]) * scale, ov + (b[3] - vv) * scale);
        for (name, color) in LAYERS {
            for region in s["layers"][name]["regions"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let loops: Vec<Vec<(f64, f64)>> = region["loops"]
                    .as_array()
                    .map(|ls| {
                        ls.iter()
                            .filter_map(|l| {
                                let f: Vec<f64> = l.as_array()?.iter().filter_map(Value::as_f64).collect();
                                Some(f.chunks_exact(2).map(|p| to_px(p[0], p[1])).collect())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                fill_even_odd(&mut out, &loops, color, (x0, y0, tw, th));
                for l in &loops {
                    stroke(&mut out, l, darker(color), (x0, y0, tw, th));
                }
            }
        }
        frame(&mut out, x0, y0, tw, th);
    }
    Some(out)
}

/// The three cuts of a `diff` reply (through the changed region, one per axis), side by side.
pub fn render_sections(sections: &Value, max_width: usize) -> Option<Picture> {
    let parts: Vec<Picture> =
        ["x", "y", "z"].iter().filter_map(|a| render_slices(&sections[*a], max_width / 3)).collect();
    (!parts.is_empty()).then(|| Picture::row(&parts, 10))
}

fn darker(c: [u8; 3]) -> [u8; 3] {
    c.map(|x| (x as f32 * 0.6) as u8)
}

/// Fill the inside of closed loops (holes are loops too) by scanlines through pixel centres.
fn fill_even_odd(p: &mut Picture, loops: &[Vec<(f64, f64)>], c: [u8; 3], clip: (usize, usize, usize, usize)) {
    let (x0, y0, w, h) = clip;
    let mut xs: Vec<f64> = Vec::new();
    for y in y0..y0 + h {
        let yc = y as f64 + 0.5;
        xs.clear();
        for l in loops {
            for (i, &(ax, ay)) in l.iter().enumerate() {
                let (bx, by) = l[(i + 1) % l.len()];
                if (ay <= yc) != (by <= yc) {
                    xs.push(ax + (yc - ay) / (by - ay) * (bx - ax));
                }
            }
        }
        xs.sort_by(f64::total_cmp);
        for pair in xs.chunks_exact(2) {
            let from = (pair[0] - 0.5).ceil().max(x0 as f64) as usize;
            let to = (pair[1] - 0.5).floor().min((x0 + w) as f64 - 1.0);
            if to < from as f64 {
                continue;
            }
            for x in from..=to as usize {
                p.set(x, y, c);
            }
        }
    }
}

fn stroke(p: &mut Picture, l: &[(f64, f64)], c: [u8; 3], clip: (usize, usize, usize, usize)) {
    let (x0, y0, w, h) = clip;
    for (i, &(ax, ay)) in l.iter().enumerate() {
        let (bx, by) = l[(i + 1) % l.len()];
        let steps = ((bx - ax).abs().max((by - ay).abs()).ceil() as usize).max(1);
        for k in 0..=steps {
            let t = k as f64 / steps as f64;
            let (x, y) = (ax + (bx - ax) * t, ay + (by - ay) * t);
            if x >= x0 as f64 && y >= y0 as f64 && x < (x0 + w) as f64 && y < (y0 + h) as f64 {
                p.set(x as usize, y as usize, c);
            }
        }
    }
}

fn frame(p: &mut Picture, x0: usize, y0: usize, w: usize, h: usize) {
    for x in x0..x0 + w {
        p.set(x, y0, FRAME);
        p.set(x, y0 + h - 1, FRAME);
    }
    for y in y0..y0 + h {
        p.set(x0, y, FRAME);
        p.set(x0 + w - 1, y, FRAME);
    }
}

// ---- sketches as pictures --------------------------------------------------------------

const PROFILE: [u8; 3] = [255, 224, 178];
const ELEMENT: [u8; 3] = [36, 96, 210];
const CONSTRAINED: [u8; 3] = [20, 20, 20];
const CONSTRUCTION: [u8; 3] = [236, 150, 60];

/// The polyline of one element of a `sketch` reply in the sketch's (x, y), or a point.
enum Drawn {
    Curve(Vec<(f64, f64)>),
    Point((f64, f64)),
}

fn xy(v: &Value) -> Option<(f64, f64)> {
    Some((v[0].as_f64()?, v[1].as_f64()?))
}

fn element(g: &Value) -> Option<Drawn> {
    let circle = |c: (f64, f64), r: f64, a0: f64, a1: f64| {
        let n = (((a1 - a0).abs() / std::f64::consts::TAU) * 96.0).ceil().max(4.0) as usize;
        Drawn::Curve((0..=n).map(|k| a0 + (a1 - a0) * k as f64 / n as f64).map(|a| (c.0 + r * a.cos(), c.1 + r * a.sin())).collect())
    };
    match g["type"].as_str()? {
        "LineSegment" => Some(Drawn::Curve(vec![xy(&g["start"])?, xy(&g["end"])?])),
        "Circle" => Some(circle(xy(&g["center"])?, g["radius"].as_f64()?, 0.0, std::f64::consts::TAU)),
        "ArcOfCircle" => {
            let (a0, mut a1) = (g["start_angle"].as_f64()?.to_radians(), g["end_angle"].as_f64()?.to_radians());
            while a1 <= a0 {
                a1 += std::f64::consts::TAU;
            }
            Some(circle(xy(&g["center"])?, g["radius"].as_f64()?, a0, a1))
        }
        "Point" => Some(Drawn::Point(xy(&g["at"])?)),
        "BSplineCurve" => Some(Drawn::Curve(g["poles"].as_array()?.iter().filter_map(xy).collect())),
        _ => match (xy(&g["start"]), xy(&g["end"])) {
            (Some(a), Some(b)) => Some(Drawn::Curve(vec![a, b])),
            _ => None,
        },
    }
}

fn loop_points(v: &Value) -> Vec<(f64, f64)> {
    v.as_array().map(|f| f.chunks_exact(2).filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?))).collect()).unwrap_or_default()
}

/// A picture of a `sketch` reply (asked with outlines): its closed profiles filled, its elements
/// over them (black when the sketch is fully constrained, orange for construction), points as
/// dots; x to the right, y up, at one scale. None for an empty sketch.
pub fn render_sketch(sk: &Value, max_side: usize) -> Option<Picture> {
    let construction = |g: &Value| g["construction"].as_bool() == Some(true);
    let elements: Vec<(Drawn, bool)> =
        sk["geometry"].as_array()?.iter().filter_map(|g| Some((element(g)?, construction(g)))).collect();
    let profiles: Vec<Vec<Vec<(f64, f64)>>> = sk["profiles"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    let mut loops = vec![loop_points(&p["outer"])];
                    loops.extend(p["inner"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().map(loop_points));
                    loops
                })
                .collect()
        })
        .unwrap_or_default();
    let all = elements.iter().flat_map(|(d, _)| match d {
        Drawn::Curve(c) => c.clone(),
        Drawn::Point(p) => vec![*p],
    });
    let (mut lo, mut hi) = ((f64::INFINITY, f64::INFINITY), (f64::NEG_INFINITY, f64::NEG_INFINITY));
    for (x, y) in all {
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    if !lo.0.is_finite() {
        return None;
    }
    let pad = ((hi.0 - lo.0).max(hi.1 - lo.1) * 0.06).max(1.0);
    let (lo, hi) = ((lo.0 - pad, lo.1 - pad), (hi.0 + pad, hi.1 + pad));
    let scale = (max_side as f64 / (hi.0 - lo.0)).min(max_side as f64 / (hi.1 - lo.1));
    let (w, h) = (((hi.0 - lo.0) * scale).ceil().max(32.0) as usize, ((hi.1 - lo.1) * scale).ceil().max(32.0) as usize);
    let mut out = Picture::new(w, h, BACKGROUND);
    let to_px = |p: (f64, f64)| ((p.0 - lo.0) * scale, (hi.1 - p.1) * scale);
    let clip = (0, 0, w, h);
    for loops in &profiles {
        let px: Vec<Vec<(f64, f64)>> = loops.iter().map(|l| l.iter().map(|&p| to_px(p)).collect()).collect();
        fill_even_odd(&mut out, &px, PROFILE, clip);
    }
    let line = if sk["fully_constrained"].as_bool() == Some(true) { CONSTRAINED } else { ELEMENT };
    for (d, constr) in &elements {
        let c = if *constr { CONSTRUCTION } else { line };
        match d {
            Drawn::Curve(pts) => {
                let px: Vec<(f64, f64)> = pts.iter().map(|&p| to_px(p)).collect();
                for seg in px.windows(2) {
                    stroke_segment(&mut out, seg[0], seg[1], c, *constr);
                }
            }
            Drawn::Point(p) => {
                let (x, y) = to_px(*p);
                for dy in -2..=2 {
                    for dx in -2..=2 {
                        out.set((x as i64 + dx).max(0) as usize, (y as i64 + dy).max(0) as usize, c);
                    }
                }
            }
        }
    }
    frame(&mut out, 0, 0, w, h);
    Some(out)
}

/// A line two pixels wide; dashed for construction.
fn stroke_segment(p: &mut Picture, a: (f64, f64), b: (f64, f64), c: [u8; 3], dashed: bool) {
    let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()).ceil() as usize).max(1);
    for k in 0..=steps {
        if dashed && (k / 6) % 2 == 1 {
            continue;
        }
        let t = k as f64 / steps as f64;
        let (x, y) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        if x >= 0.0 && y >= 0.0 {
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                p.set(x as usize + dx, y as usize + dy, c);
            }
        }
    }
}

/// How to read the picture of a sketch.
pub fn sketch_legend(sk: &Value) -> String {
    format!(
        "Picture: sketch {} in its own (x, y), x to the right and y up, all at one scale. Orange fill: the closed \
         profiles an extrude takes; lines: its elements, {}; dashed orange: construction; dots: points.",
        sk["label"].as_str().unwrap_or("?"),
        if sk["fully_constrained"].as_bool() == Some(true) { "black: fully constrained" } else { "blue: not fully constrained yet" }
    )
}

/// A sketch reply without its profiles' outlines, which the picture shows.
pub fn without_profile_outlines(v: &Value) -> Value {
    let mut v = v.clone();
    let sk = if v.get("profiles").is_some() { Some(&mut v) } else { v.get_mut("sketch") };
    if let Some(ps) = sk.and_then(|s| s.get_mut("profiles")).and_then(Value::as_array_mut) {
        for p in ps.iter_mut().filter_map(Value::as_object_mut) {
            p.remove("outer");
            p.remove("inner");
            p.remove("mesh");
        }
    }
    v
}

/// A `slice` or `diff` reply without the outlines, which the picture shows; they stay when
/// the agent asked for them.
pub fn without_outlines(v: &Value) -> Value {
    let mut v = v.clone();
    strip(&mut v);
    v
}

fn strip(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("loops");
            m.values_mut().for_each(strip);
        }
        Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

/// The close-up cuts that come with the first places of a `validate` reply, side by side, and
/// how to read them.
pub fn render_places(v: &Value, tile: usize) -> Option<(Picture, String)> {
    let mut tiles = Vec::new();
    let mut legend = Vec::new();
    for p in v["places"].as_array()? {
        let (Some(pic), Some(b)) = (render_slices(&p["section"], tile), p["view_box"].as_array()) else { continue };
        let n = &p["section"]["normal"];
        let offset = &p["section"]["slices"][0]["offset"];
        let axis = ["x", "y", "z"].iter().zip(0..3).find(|(_, k)| n[*k].as_f64().is_some_and(|c| c > 0.999));
        let plane = axis.map_or_else(|| format!("normal {n} . p = {offset}"), |(a, _)| format!("{a} = {offset}"));
        let wide = b.get(3).and_then(Value::as_f64).unwrap_or(0.0) - b.first().and_then(Value::as_f64).unwrap_or(0.0);
        legend.push(format!("place {} cut by the plane {plane}, a box {wide:.3} mm wide around it", p["id"]));
        tiles.push(pic);
    }
    if tiles.is_empty() {
        return None;
    }
    let text = format!(
        "Picture: a close look at the first places, left to right, each at its own scale: {}. Blue: material. \
         For another look at a place, call slice with its view_box as box.",
        legend.join("; ")
    );
    Some((Picture::row(&tiles, 10), text))
}

/// A `validate` reply without the cuts, which the picture shows.
pub fn without_sections(v: &Value) -> Value {
    let mut v = v.clone();
    for p in v["places"].as_array_mut().into_iter().flatten().filter_map(Value::as_object_mut) {
        p.remove("section");
    }
    v
}

/// How to read the picture of a `slice` reply.
pub fn slice_legend(v: &Value) -> String {
    let n = v["slices"].as_array().map_or(0, Vec::len);
    let cols = n.clamp(1, 4);
    let offsets: Vec<String> = v["slices"]
        .as_array()
        .map(|s| s.iter().map(|x| format!("{}", x["offset"])).collect())
        .unwrap_or_default();
    let layers = if v["compared"].as_bool() == Some(true) {
        "grey: in both states, green: added (only in `state`), red: removed (only in `before`)"
    } else {
        "blue: material"
    };
    format!(
        "Picture: {n} cut(s), {cols} per row, left to right then down, at offsets {} along the normal. \
         Each tile spans the same (u, v) bounds {} with u to the right and v up; {layers}.",
        offsets.join(", "),
        v["bounds"]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_with_a_hole_fills_around_it() {
        let v = json!({
            "bounds": [0.0, 0.0, 10.0, 10.0],
            "compared": false,
            "slices": [{ "offset": 1.0, "layers": { "section": { "regions": [{ "loops": [
                [0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0],
                [4.0, 4.0, 6.0, 4.0, 6.0, 6.0, 4.0, 6.0]
            ] }] } } }]
        });
        let p = render_slices(&v, 200).unwrap();
        let at = |fx: f64, fy: f64| {
            let (x, y) = ((p.width as f64 * fx) as usize, (p.height as f64 * fy) as usize);
            let i = (y * p.width + x) * 3;
            [p.rgb[i], p.rgb[i + 1], p.rgb[i + 2]]
        };
        assert_eq!(at(0.25, 0.25), LAYERS[0].1, "inside the square");
        assert_eq!(at(0.5, 0.5), BACKGROUND, "the hole stays empty");
        let png = p.png();
        assert_eq!(&png[1..4], b"PNG");
        assert!(without_outlines(&v)["slices"][0]["layers"]["section"]["regions"][0].get("loops").is_none());
    }

    #[test]
    fn a_sketch_picture_fills_its_profile_and_draws_its_lines() {
        let sk = json!({
            "label": "Sketch", "fully_constrained": false,
            "geometry": [
                { "type": "LineSegment", "start": [0.0, 0.0], "end": [10.0, 0.0] },
                { "type": "LineSegment", "start": [10.0, 0.0], "end": [10.0, 10.0] },
                { "type": "LineSegment", "start": [10.0, 10.0], "end": [0.0, 10.0] },
                { "type": "LineSegment", "start": [0.0, 10.0], "end": [0.0, 0.0] }
            ],
            "profiles": [{ "area": 100.0, "outer": [0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0], "inner": [] }]
        });
        let p = render_sketch(&sk, 200).unwrap();
        let at = |fx: f64, fy: f64| {
            let (x, y) = ((p.width as f64 * fx) as usize, (p.height as f64 * fy) as usize);
            let i = (y * p.width + x) * 3;
            [p.rgb[i], p.rgb[i + 1], p.rgb[i + 2]]
        };
        assert_eq!(at(0.5, 0.5), PROFILE, "inside the square");
        assert_eq!(at(0.5, 0.97), BACKGROUND, "outside it");
        let bottom = (0..p.height).map(|y| at(0.5, y as f64 / p.height as f64)).filter(|c| *c == ELEMENT).count();
        assert!(bottom >= 2, "the top and bottom lines cross the middle column");
        let stripped = without_profile_outlines(&json!({ "sketch": sk }));
        assert!(stripped["sketch"]["profiles"][0].get("outer").is_none());
        assert!(render_sketch(&json!({ "geometry": [] }), 200).is_none());
    }
}
