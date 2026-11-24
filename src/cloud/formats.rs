//! Reading scans: PLY (text and binary), STL (text and binary), OBJ vertices, text point lists
//! (XYZ, ASC, PTS, TXT, CSV) and E57. Every reader streams its file and hands each point to a
//! `Sink`; none holds the file in memory. Lengths are taken as millimetres, except E57, which
//! is in metres by its standard.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};

use super::{Point, oct_encode, rgba};

/// What the window can read, for the file dialog.
pub const EXTENSIONS: &[&str] = &["ply", "stl", "obj", "xyz", "asc", "pts", "txt", "csv", "e57"];

/// The points of a file, relative to its first point.
pub struct Imported {
    pub base: [f64; 3],
    pub points: Vec<Point>,
    pub has_color: bool,
    pub has_normals: bool,
    pub format: &'static str,
}

/// Where readers put points; counts bytes read and stops when asked.
pub struct Sink<'a> {
    base: Option<[f64; 3]>,
    points: Vec<Point>,
    cancel: &'a AtomicBool,
}

impl Sink<'_> {
    fn push(&mut self, p: [f64; 3], color: [u8; 3], normal: [f32; 3]) -> Result<()> {
        if !(p[0].is_finite() && p[1].is_finite() && p[2].is_finite()) {
            return Ok(());
        }
        let base = *self.base.get_or_insert(p);
        if self.points.len() & 0xF_FFFF == 0 && self.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        if self.points.len() >= u32::MAX as usize {
            bail!("more than {} points: not supported", u32::MAX);
        }
        self.points.push(Point {
            pos: [(p[0] - base[0]) as f32, (p[1] - base[1]) as f32, (p[2] - base[2]) as f32],
            color: rgba(color),
            normal: oct_encode(normal),
        });
        Ok(())
    }
}

/// Counts what is read through it, for progress.
struct Counting<'a, R> {
    inner: R,
    read: &'a AtomicU64,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// `BufRead::read_line` for files that are not all UTF-8 (a header comment in Latin-1, a
/// degree sign): bytes that are not UTF-8 become U+FFFD, and a byte-order mark is dropped.
fn read_line(r: &mut impl BufRead, line: &mut String) -> std::io::Result<usize> {
    thread_local!(static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) });
    BUF.with_borrow_mut(|b| {
        b.clear();
        let n = r.read_until(b'\n', b)?;
        let s = String::from_utf8_lossy(b);
        line.push_str(s.strip_prefix('\u{feff}').unwrap_or(&s));
        Ok(n)
    })
}

const NO_COLOR: [u8; 3] = [0, 0, 0];
const NO_NORMAL: [f32; 3] = [0.0, 0.0, 1.0];

pub fn read(path: &Path, read: &AtomicU64, cancel: &AtomicBool) -> Result<Imported> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mut sink = Sink { base: None, points: Vec::new(), cancel };
    if ext == "e57" {
        let (has_color, _) = e57(path, &mut sink, read)?;
        return finish(sink, has_color, false, "E57");
    }
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::with_capacity(1 << 20, Counting { inner: file, read });
    let (has_color, has_normals, format) = match ext.as_str() {
        "ply" => {
            let (c, n) = ply(&mut r, &mut sink)?;
            (c, n, "PLY")
        }
        "stl" => {
            stl(&mut r, len, &mut sink)?;
            (false, true, "STL")
        }
        "obj" => {
            let c = obj(&mut r, &mut sink)?;
            (c, false, "OBJ")
        }
        "xyz" | "asc" | "pts" | "txt" | "csv" => {
            let (c, n) = text(&mut r, &mut sink)?;
            (c, n, "text")
        }
        _ => bail!("cannot read .{ext} files as scans; VibeCAD reads {}", EXTENSIONS.join(", ")),
    };
    finish(sink, has_color, has_normals, format)
}

fn finish(sink: Sink, has_color: bool, has_normals: bool, format: &'static str) -> Result<Imported> {
    let Some(base) = sink.base else { bail!("the file has no points") };
    Ok(Imported { base, points: sink.points, has_color, has_normals, format })
}

// ---- PLY -------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scalar {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Scalar {
    fn parse(s: &str) -> Result<Scalar> {
        Ok(match s {
            "char" | "int8" => Scalar::I8,
            "uchar" | "uint8" => Scalar::U8,
            "short" | "int16" => Scalar::I16,
            "ushort" | "uint16" => Scalar::U16,
            "int" | "int32" => Scalar::I32,
            "uint" | "uint32" => Scalar::U32,
            "float" | "float32" => Scalar::F32,
            "double" | "float64" => Scalar::F64,
            _ => bail!("PLY: unknown property type {s}"),
        })
    }

    fn size(self) -> usize {
        match self {
            Scalar::I8 | Scalar::U8 => 1,
            Scalar::I16 | Scalar::U16 => 2,
            Scalar::I32 | Scalar::U32 | Scalar::F32 => 4,
            Scalar::F64 => 8,
        }
    }

    fn decode(self, b: &[u8], big: bool) -> f64 {
        macro_rules! num {
            ($t:ty, $n:expr) => {{
                let a: [u8; $n] = b[..$n].try_into().unwrap();
                (if big { <$t>::from_be_bytes(a) } else { <$t>::from_le_bytes(a) }) as f64
            }};
        }
        match self {
            Scalar::I8 => b[0] as i8 as f64,
            Scalar::U8 => b[0] as f64,
            Scalar::I16 => num!(i16, 2),
            Scalar::U16 => num!(u16, 2),
            Scalar::I32 => num!(i32, 4),
            Scalar::U32 => num!(u32, 4),
            Scalar::F32 => num!(f32, 4),
            Scalar::F64 => num!(f64, 8),
        }
    }

    /// A colour channel of this type as 0..255: integers as they are, floats from 0..1.
    fn channel(self, v: f64) -> u8 {
        match self {
            Scalar::F32 | Scalar::F64 => (v * 255.0).round().clamp(0.0, 255.0) as u8,
            Scalar::U16 => (v / 257.0).round().clamp(0.0, 255.0) as u8,
            _ => v.clamp(0.0, 255.0) as u8,
        }
    }
}

struct Property {
    name: String,
    ty: Scalar,
    /// The count's type, for a list.
    list: Option<Scalar>,
}

struct Element {
    name: String,
    count: u64,
    props: Vec<Property>,
}

#[derive(PartialEq)]
enum PlyFormat {
    Ascii,
    Little,
    Big,
}

fn ply(r: &mut impl BufRead, sink: &mut Sink) -> Result<(bool, bool)> {
    let mut line = String::new();
    read_line(r, &mut line)?;
    if line.trim() != "ply" {
        bail!("not a PLY file");
    }
    let mut format = None;
    let mut elements: Vec<Element> = Vec::new();
    loop {
        line.clear();
        if read_line(r, &mut line)? == 0 {
            bail!("PLY: the header has no end_header");
        }
        let w: Vec<&str> = line.split_ascii_whitespace().collect();
        match w.as_slice() {
            ["format", f, ..] => {
                format = Some(match *f {
                    "ascii" => PlyFormat::Ascii,
                    "binary_little_endian" => PlyFormat::Little,
                    "binary_big_endian" => PlyFormat::Big,
                    _ => bail!("PLY: unknown format {f}"),
                })
            }
            ["element", name, count] => elements.push(Element { name: name.to_string(), count: count.parse().context("PLY: element count")?, props: Vec::new() }),
            ["property", "list", count, ty, name] => {
                let e = elements.last_mut().ok_or_else(|| anyhow!("PLY: a property before any element"))?;
                e.props.push(Property { name: name.to_string(), ty: Scalar::parse(ty)?, list: Some(Scalar::parse(count)?) });
            }
            ["property", ty, name] => {
                let e = elements.last_mut().ok_or_else(|| anyhow!("PLY: a property before any element"))?;
                e.props.push(Property { name: name.to_string(), ty: Scalar::parse(ty)?, list: None });
            }
            ["end_header"] => break,
            _ => {} // comment, obj_info
        }
    }
    let format = format.ok_or_else(|| anyhow!("PLY: no format line"))?;
    let Some(vi) = elements.iter().position(|e| e.name == "vertex") else { bail!("PLY: no vertex element") };
    // Elements written before the vertices are read past.
    for e in &elements[..vi] {
        skip_element(r, e, &format)?;
    }
    let v = &elements[vi];
    let find = |names: &[&str]| v.props.iter().position(|p| names.contains(&p.name.as_str()));
    let (Some(x), Some(y), Some(z)) = (find(&["x"]), find(&["y"]), find(&["z"])) else { bail!("PLY: the vertices have no x, y, z") };
    let normal = match (find(&["nx", "normal_x"]), find(&["ny", "normal_y"]), find(&["nz", "normal_z"])) {
        (Some(a), Some(b), Some(c)) => Some([a, b, c]),
        _ => None,
    };
    let color = match (find(&["red", "r", "diffuse_red"]), find(&["green", "g", "diffuse_green"]), find(&["blue", "b", "diffuse_blue"])) {
        (Some(a), Some(b), Some(c)) => Some([a, b, c]),
        _ => None,
    };
    let mut vals = vec![0.0f64; v.props.len()];
    // Some exporters write normals of zero: a file of those has none.
    let zero_normals = std::cell::Cell::new(0u64);
    let emit = |vals: &[f64], sink: &mut Sink| {
        let c = color.map_or(NO_COLOR, |c| [0, 1, 2].map(|k| v.props[c[k]].ty.channel(vals[c[k]])));
        let n = normal.map_or(NO_NORMAL, |n| [0, 1, 2].map(|k| vals[n[k]] as f32));
        if n == [0.0; 3] {
            zero_normals.set(zero_normals.get() + 1);
        }
        sink.push([vals[x], vals[y], vals[z]], c, n)
    };
    if format == PlyFormat::Ascii {
        for _ in 0..v.count {
            line.clear();
            if read_line(r, &mut line)? == 0 {
                bail!("PLY: the file ends before its {} vertices", v.count);
            }
            let mut it = line.split_ascii_whitespace();
            for (k, p) in v.props.iter().enumerate() {
                if p.list.is_some() {
                    bail!("PLY: list properties on vertices are not supported");
                }
                vals[k] = it.next().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("PLY: a vertex line has too few numbers"))?;
            }
            emit(&vals, sink)?;
        }
    } else {
        if v.props.iter().any(|p| p.list.is_some()) {
            bail!("PLY: list properties on vertices are not supported");
        }
        let big = format == PlyFormat::Big;
        let offsets: Vec<usize> = v.props.iter().scan(0, |o, p| {
            let at = *o;
            *o += p.ty.size();
            Some(at)
        }).collect();
        let size: usize = v.props.iter().map(|p| p.ty.size()).sum();
        let per_batch = (1 << 20) / size.max(1);
        let mut buf = vec![0u8; per_batch * size];
        let mut left = v.count;
        while left > 0 {
            let n = left.min(per_batch as u64) as usize;
            r.read_exact(&mut buf[..n * size]).context("PLY: the file ends before its vertices")?;
            for rec in buf[..n * size].chunks_exact(size) {
                for (k, p) in v.props.iter().enumerate() {
                    vals[k] = p.ty.decode(&rec[offsets[k]..], big);
                }
                emit(&vals, sink)?;
            }
            left -= n as u64;
        }
    }
    Ok((color.is_some(), normal.is_some() && zero_normals.get() * 2 < v.count))
}

fn skip_element(r: &mut impl BufRead, e: &Element, format: &PlyFormat) -> Result<()> {
    if *format == PlyFormat::Ascii {
        let mut line = String::new();
        for _ in 0..e.count {
            line.clear();
            read_line(r, &mut line)?;
        }
        return Ok(());
    }
    let big = *format == PlyFormat::Big;
    let mut b = [0u8; 8];
    for _ in 0..e.count {
        for p in &e.props {
            let n = match p.list {
                Some(c) => {
                    r.read_exact(&mut b[..c.size()])?;
                    c.decode(&b, big) as u64
                }
                None => 1,
            };
            std::io::copy(&mut r.by_ref().take(n * p.ty.size() as u64), &mut std::io::sink())?;
        }
    }
    Ok(())
}

// ---- STL -------------------------------------------------------------------------------

/// A mesh's corners, each once (STL repeats them per triangle), with area-weighted normals.
fn stl(r: &mut impl BufRead, len: u64, sink: &mut Sink) -> Result<()> {
    let mut header = [0u8; 84];
    let head = r.fill_buf()?;
    let n = head.len().min(84);
    header[..n].copy_from_slice(&head[..n]);
    // Text only when it reads as text: a binary header may start with "solid" too, and binary
    // files may be padded or carry a count of zero (written as they stream).
    let probe = &head[..head.len().min(1024)];
    let text = probe.starts_with(b"solid") && probe.is_ascii() && probe.windows(5).any(|w| w == b"facet");
    let binary = !text;
    if binary && n < 84 {
        bail!("STL: too short for a binary STL and not a text one");
    }
    let declared = u32::from_le_bytes(header[80..84].try_into().unwrap()) as u64;
    let fits = len.saturating_sub(84) / 50;
    let count = if declared == 0 || declared > fits { fits } else { declared };
    let mut corners: HashMap<[u32; 3], u32> = HashMap::new();
    let mut pos: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut add = |tri: [[f32; 3]; 3]| {
        let [a, b, c] = tri.map(glam::Vec3::from);
        let n = (b - a).cross(c - a); // twice the area, along the normal
        for p in tri {
            let k = corners.entry(p.map(f32::to_bits)).or_insert_with(|| {
                pos.push(p);
                normals.push([0.0; 3]);
                pos.len() as u32 - 1
            });
            let acc = &mut normals[*k as usize];
            acc[0] += n.x;
            acc[1] += n.y;
            acc[2] += n.z;
        }
    };
    if binary {
        r.read_exact(&mut header)?;
        let mut rec = [0u8; 50];
        for _ in 0..count {
            r.read_exact(&mut rec)?;
            let f = |i: usize| f32::from_le_bytes(rec[i..i + 4].try_into().unwrap());
            add([[f(12), f(16), f(20)], [f(24), f(28), f(32)], [f(36), f(40), f(44)]]);
        }
    } else {
        let mut line = String::new();
        let mut tri = Vec::with_capacity(3);
        loop {
            line.clear();
            if read_line(r, &mut line)? == 0 {
                break;
            }
            let mut w = line.split_ascii_whitespace();
            if w.next() != Some("vertex") {
                continue;
            }
            let v: Vec<f32> = w.take(3).filter_map(|s| s.parse().ok()).collect();
            if v.len() != 3 {
                bail!("STL: a vertex line without three numbers");
            }
            tri.push([v[0], v[1], v[2]]);
            if tri.len() == 3 {
                add([tri[0], tri[1], tri[2]]);
                tri.clear();
            }
        }
    }
    if pos.is_empty() {
        bail!("STL: no triangles");
    }
    for (p, n) in pos.iter().zip(&normals) {
        sink.push(p.map(f64::from), NO_COLOR, *n)?;
    }
    Ok(())
}

// ---- OBJ and text point lists ----------------------------------------------------------

/// The `v` lines of an OBJ, with the colour some scanners add after the position (0..1).
fn obj(r: &mut impl BufRead, sink: &mut Sink) -> Result<bool> {
    let mut line = String::new();
    let mut has_color = None;
    loop {
        line.clear();
        if read_line(r, &mut line)? == 0 {
            break;
        }
        let mut words = line.split_ascii_whitespace();
        if words.next() != Some("v") {
            continue;
        }
        let v: Vec<f64> = words.filter_map(|s| s.parse().ok()).collect();
        if v.len() < 3 {
            continue;
        }
        let colored = *has_color.get_or_insert(v.len() >= 6);
        let c = if colored && v.len() >= 6 { [v[3], v[4], v[5]].map(|x| (x * 255.0).round().clamp(0.0, 255.0) as u8) } else { NO_COLOR };
        sink.push([v[0], v[1], v[2]], c, NO_NORMAL)?;
    }
    Ok(has_color.unwrap_or(false))
}

/// What the columns after x, y, z hold, guessed from the first line of numbers.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Columns {
    color: Option<usize>,
    normal: Option<usize>,
}

fn guess_columns(v: &[f64]) -> Columns {
    let is_normal = |at: usize| v.len() >= at + 3 && v[at..at + 3].iter().all(|x| x.abs() <= 1.0) && (v[at..at + 3].iter().map(|x| x * x).sum::<f64>() - 1.0).abs() < 0.05;
    let is_color = |at: usize| v.len() >= at + 3 && v[at..at + 3].iter().all(|x| x.fract() == 0.0 && (0.0..=255.0).contains(x));
    let mut c = Columns { color: None, normal: None };
    match v.len() {
        // A dark first point (0 0 1) reads as a normal too: past 1 it can only be a colour.
        6 if is_color(3) && v[3..6].iter().any(|x| *x > 1.0) => c.color = Some(3),
        6 if is_normal(3) => c.normal = Some(3),
        6 if is_color(3) => c.color = Some(3),
        // PTS: x y z intensity r g b
        7 if is_color(4) => c.color = Some(4),
        n if n >= 9 => {
            if is_color(3) && is_normal(6) {
                c = Columns { color: Some(3), normal: Some(6) };
            } else if is_normal(3) && is_color(6) {
                c = Columns { color: Some(6), normal: Some(3) };
            } else if is_normal(3) {
                c.normal = Some(3);
            }
        }
        _ => {}
    }
    c
}

/// One point per line, numbers apart by spaces, tabs, commas or semicolons. Lines with fewer
/// than three numbers (a header, a count) are passed over.
fn text(r: &mut impl BufRead, sink: &mut Sink) -> Result<(bool, bool)> {
    let mut line = String::new();
    let mut cols = None;
    let mut v: Vec<f64> = Vec::with_capacity(12);
    loop {
        line.clear();
        if read_line(r, &mut line)? == 0 {
            break;
        }
        v.clear();
        for s in line.split(|c: char| c.is_ascii_whitespace() || c == ',' || c == ';').filter(|s| !s.is_empty()) {
            match s.parse::<f64>() {
                Ok(x) => v.push(x),
                Err(_) => break,
            }
        }
        if v.len() < 3 {
            continue;
        }
        let c: Columns = *cols.get_or_insert_with(|| guess_columns(&v));
        let color = c.color.filter(|&at| v.len() >= at + 3).map_or(NO_COLOR, |at| [v[at], v[at + 1], v[at + 2]].map(|x| x.clamp(0.0, 255.0) as u8));
        let normal = c.normal.filter(|&at| v.len() >= at + 3).map_or(NO_NORMAL, |at| [v[at] as f32, v[at + 1] as f32, v[at + 2] as f32]);
        sink.push([v[0], v[1], v[2]], color, normal)?;
    }
    let c = cols.unwrap_or(Columns { color: None, normal: None });
    Ok((c.color.is_some(), c.normal.is_some()))
}

// ---- E57 -------------------------------------------------------------------------------

/// Every scan of the file, placed by its pose, metres to millimetres. Without colours, the
/// intensity shows as grey.
fn e57(path: &Path, sink: &mut Sink, read: &AtomicU64) -> Result<(bool, bool)> {
    let mut reader = e57::E57Reader::from_file(path).map_err(|e| anyhow!("E57: {e}"))?;
    let clouds = reader.pointclouds();
    let total: u64 = clouds.iter().map(|c| c.records).sum();
    let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let has_color = clouds.iter().any(|c| c.has_color() || c.has_intensity());
    let mut done = 0u64;
    for pc in &clouds {
        let mut it = reader.pointcloud_simple(pc).map_err(|e| anyhow!("E57: {e}"))?;
        it.apply_pose(true);
        it.spherical_to_cartesian(true);
        for p in it {
            let p = p.map_err(|e| anyhow!("E57: {e}"))?;
            done += 1;
            if done.is_multiple_of(65536) && total > 0 {
                read.store(file_len * done / total, Ordering::Relaxed);
            }
            let e57::CartesianCoordinate::Valid { x, y, z } = p.cartesian else { continue };
            let c = match (&p.color, p.intensity) {
                (Some(c), _) => [c.red, c.green, c.blue].map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8),
                (None, Some(i)) => [(i * 255.0).round().clamp(0.0, 255.0) as u8; 3],
                (None, None) => NO_COLOR,
            };
            sink.push([x * 1000.0, y * 1000.0, z * 1000.0], c, NO_NORMAL)?;
        }
    }
    read.store(file_len, Ordering::Relaxed);
    Ok((has_color, false))
}

// ---- writing ---------------------------------------------------------------------------

/// Write the points of a scan not deleted as a binary PLY, in document coordinates (`model`
/// takes scan coordinates there): x, y, z as doubles, the normals, the colours when it has them.
/// Returns how many points were written.
pub fn write_ply(path: &Path, cloud: &super::Cloud, marks: &super::marks::Marks, model: glam::DMat4) -> Result<usize> {
    use std::io::Write as _;
    let keep = cloud.points.len().saturating_sub(marks.deleted());
    let color = cloud.header.has_color;
    let mut head = format!(
        "ply\nformat binary_little_endian 1.0\ncomment VibeCAD scan, document coordinates in mm\nelement vertex {keep}\n\
         property double x\nproperty double y\nproperty double z\nproperty float nx\nproperty float ny\nproperty float nz\n"
    );
    if color {
        head += "property uchar red\nproperty uchar green\nproperty uchar blue\n";
    }
    head += "end_header\n";
    let tmp = path.with_extension("ply.part");
    let rotate = glam::DMat3::from_mat4(model);
    let rotate = if rotate.determinant().abs() > 1e-20 { rotate.inverse().transpose() } else { rotate };
    let mut written = 0;
    let result = (|| -> Result<()> {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?);
        w.write_all(head.as_bytes())?;
        let mut rec = Vec::with_capacity(39);
        for (i, p) in cloud.points.iter().enumerate() {
            if marks.get(i) & super::marks::DELETED != 0 {
                continue;
            }
            rec.clear();
            let q = model.transform_point3(glam::DVec3::from(p.pos.map(f64::from)));
            q.to_array().iter().for_each(|v| rec.extend(v.to_le_bytes()));
            let n = (rotate * super::oct_decode(p.normal).as_dvec3()).normalize_or_zero().as_vec3();
            n.to_array().iter().for_each(|v| rec.extend(v.to_le_bytes()));
            if color {
                rec.extend(&p.color.to_le_bytes()[..3]);
            }
            w.write_all(&rec)?;
            written += 1;
        }
        w.flush()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn run(f: impl FnOnce(&mut Sink) -> Result<()>) -> Vec<Point> {
        let cancel = AtomicBool::new(false);
        let mut s = Sink { base: None, points: Vec::new(), cancel: &cancel };
        f(&mut s).unwrap();
        s.points
    }

    #[test]
    fn ply_ascii_with_faces_and_colours() {
        let src = "ply\nformat ascii 1.0\ncomment x\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\n\
                   property uchar red\nproperty uchar green\nproperty uchar blue\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n\
                   1 2 3 255 0 0\n4 5 6 0 255 0\n7 8 9 0 0 255\n3 0 1 2\n";
        let mut flags = (false, false);
        let pts = run(|s| {
            flags = ply(&mut Cursor::new(src), s)?;
            Ok(())
        });
        assert_eq!(flags, (true, false));
        assert_eq!(pts.len(), 3);
        // relative to the first point
        assert_eq!(pts[2].pos, [6.0, 6.0, 6.0]);
        assert_eq!(pts[1].color, rgba([0, 255, 0]));
    }

    #[test]
    fn ply_binary_skips_elements_before_vertices() {
        let mut b = b"ply\nformat binary_little_endian 1.0\nelement camera 1\nproperty list uchar float k\nelement vertex 2\n\
                      property double x\nproperty double y\nproperty double z\nproperty float nx\nproperty float ny\nproperty float nz\nend_header\n"
            .to_vec();
        b.push(2);
        b.extend(1.0f32.to_le_bytes());
        b.extend(2.0f32.to_le_bytes());
        for (p, n) in [([10.0f64, 0.0, 0.0], [1.0f32, 0.0, 0.0]), ([10.0, 0.0, 2.5], [0.0, 0.0, -1.0])] {
            p.iter().for_each(|v| b.extend(v.to_le_bytes()));
            n.iter().for_each(|v| b.extend(v.to_le_bytes()));
        }
        let mut flags = (false, false);
        let pts = run(|s| {
            flags = ply(&mut Cursor::new(b), s)?;
            Ok(())
        });
        assert_eq!(flags, (false, true));
        assert_eq!(pts[1].pos, [0.0, 0.0, 2.5]);
        assert_eq!(pts[1].normal, oct_encode([0.0, 0.0, -1.0]));
    }

    #[test]
    fn stl_corners_are_shared() {
        // two triangles of a square in z = 0: four corners
        let mut b = vec![0u8; 80];
        b.extend(2u32.to_le_bytes());
        for tri in [[[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]], [[0.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]]] {
            b.extend([0u8; 12]);
            tri.iter().flatten().for_each(|v| b.extend(v.to_le_bytes()));
            b.extend([0u8; 2]);
        }
        let len = b.len() as u64;
        let pts = run(|s| stl(&mut Cursor::new(b), len, s));
        assert_eq!(pts.len(), 4);
        assert!(pts.iter().all(|p| p.normal == oct_encode([0.0, 0.0, 1.0])));
        let ascii = "solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 1 1 0\nendloop\nendfacet\nendsolid t\n";
        let pts = run(|s| stl(&mut Cursor::new(ascii), ascii.len() as u64, s));
        assert_eq!(pts.len(), 3);
    }

    #[test]
    fn text_columns_are_guessed() {
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0]), Columns { color: None, normal: None });
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0, 0.0, 0.6, 0.8]), Columns { color: None, normal: Some(3) });
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0, 200.0, 10.0, 0.0]), Columns { color: Some(3), normal: None });
        // a dark point first: 0 0 1 could be a normal, but colour wins only past 1
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0, 0.0, 0.0, 1.0]), Columns { color: None, normal: Some(3) });
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0, -1200.0, 200.0, 10.0, 0.0]), Columns { color: Some(4), normal: None });
        assert_eq!(guess_columns(&[1.0, 2.0, 3.0, 0.0, 0.0, 1.0, 9.0, 9.0, 9.0]), Columns { color: Some(6), normal: Some(3) });
        let src = "3\n1.5,2,3,10,20,30\n4;5;6;40;50;60\n";
        let pts = run(|s| text(&mut Cursor::new(src), s).map(|_| ()));
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[1].pos, [2.5, 3.0, 3.0]);
        assert_eq!(pts[1].color, rgba([40, 50, 60]));
    }

    #[test]
    fn text_that_is_not_utf8() {
        // a byte-order mark, then a Latin-1 degree sign in a header line
        let mut src = vec![0xef, 0xbb, 0xbf];
        src.extend(b"1 2 3\n# 20\xb0C\n4 5 6\n");
        let pts = run(|s| text(&mut Cursor::new(src), s).map(|_| ()));
        assert_eq!(pts.len(), 2, "the first point is kept");
        let ply_src = b"ply\nformat ascii 1.0\ncomment scanned at 20\xb0C\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nend_header\n1 2 3\n".to_vec();
        let pts = run(|s| ply(&mut Cursor::new(ply_src), s).map(|_| ()));
        assert_eq!(pts.len(), 1);
    }

    #[test]
    fn ply_normals_of_zero_are_none() {
        let src = "ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\n\
                   property float nx\nproperty float ny\nproperty float nz\nend_header\n0 0 0 0 0 0\n1 0 0 0 0 0\n";
        let mut flags = (true, true);
        run(|s| {
            flags = ply(&mut Cursor::new(src), s)?;
            Ok(())
        });
        assert_eq!(flags, (false, false));
    }

    #[test]
    fn stl_binary_with_a_count_of_zero_and_a_solid_header() {
        let mut b = b"solid but binary".to_vec();
        b.resize(80, 0);
        b.extend(0u32.to_le_bytes());
        b.extend([0u8; 12]);
        for v in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            b.extend(v.to_le_bytes());
        }
        b.extend([0u8; 2]);
        let len = b.len() as u64;
        let pts = run(|s| stl(&mut Cursor::new(b), len, s));
        assert_eq!(pts.len(), 3);
    }

    #[test]
    fn written_ply_reads_back_without_the_deleted_points() {
        let pts: Vec<[f64; 3]> = (0..100).map(|i| [i as f64, 2.0 * i as f64, 1.0]).collect();
        let imported = Imported {
            base: pts[0],
            points: pts.iter().map(|p| Point { pos: [p[0] as f32, p[1] as f32, 0.0], color: rgba([1, 2, 3]), normal: oct_encode([0.0, 0.0, 1.0]) }).collect(),
            has_color: true,
            has_normals: true,
            format: "test",
        };
        let cloud = super::super::build(imported, &super::super::Progress::default()).unwrap();
        let mut marks = super::super::marks::Marks::new(cloud.points.len());
        let chunks = cloud.header.chunks.clone();
        marks.select(&cloud.points, &chunks, super::super::marks::Op::Replace, |_| true, |p| p.pos[0] < 0.0);
        let selected = marks.selected();
        marks.delete_selected();
        let dir = std::env::temp_dir().join(format!("vibecad-ply-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("out.ply");
        // moved 1000 along x in the document
        let model = glam::DMat4::from_translation(glam::DVec3::new(1000.0, 0.0, 0.0)) * glam::DMat4::from_translation(cloud.origin());
        assert_eq!(write_ply(&f, &cloud, &marks, model).unwrap(), 100 - selected);
        let cancel = AtomicBool::new(false);
        let back = read(&f, &AtomicU64::new(0), &cancel).unwrap();
        assert_eq!((back.points.len(), back.has_color, back.has_normals), (100 - selected, true, true));
        let xs: Vec<f64> = back.points.iter().map(|p| back.base[0] + p.pos[0] as f64).collect();
        assert!(xs.iter().all(|x| (1049.0..=1099.0).contains(x)), "only the kept half, moved: {xs:?}");
        assert_eq!(back.points[0].color, rgba([1, 2, 3]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn obj_vertices_with_colour() {
        let src = "# scan\nv 0 0 0 1 0 0\nvn 0 0 1\n  v\t1 1 1 0 1 0\nf 1 2 1\n";
        let mut colored = false;
        let pts = run(|s| {
            colored = obj(&mut Cursor::new(src), s)?;
            Ok(())
        });
        assert!(colored);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].color, rgba([255, 0, 0]));
    }
}
