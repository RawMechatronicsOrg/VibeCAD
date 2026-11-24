//! Scans: point clouds and scanned meshes, drawn beside the FreeCAD model but never sent to
//! FreeCAD. A scan in a document is a plain `App::FeaturePython` with the file's path and a
//! Placement (see `cmd_add_scan` in the worker); the window reads the file itself.
//!
//! Reading a file the first time builds a cache next to the mesh cache: the points sorted along
//! a Morton curve and cut into chunks of `CHUNK` neighbouring points, each chunk shuffled. Any
//! first part of a chunk is then an even sample of it, so the view draws as many of a chunk's
//! points as its size on screen needs, and no level-of-detail copies are kept. Points without
//! normals get them from their nearest neighbours, so every scan can be lit as a surface.
//!
//! Positions are `f32` relative to the scan's centre (`origin`, in `f64`), so a part far from
//! the origin of its scanner keeps sub-micrometre steps.

use std::hash::Hasher;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use bytemuck::{Pod, Zeroable};
use glam::{DVec3, Vec3};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub mod formats;
pub mod kd;
pub mod marks;

/// Points per chunk: the unit of culling and of level of detail.
pub const CHUNK: usize = 1 << 15;
/// Neighbours a normal is fitted to.
const NORMAL_NEIGHBOURS: usize = 12;
/// Bumped whenever the cache's layout or the order of its points changes: saved masks of
/// deleted points (see `marks`) name points by their place in that order.
pub const CACHE_VERSION: u32 = 2;
/// Scan caches past this many bytes in all are dropped, least recently used first.
const CACHE_LIMIT: u64 = 16 << 30;
const MAGIC: &[u8; 8] = b"VIBESCAN";

/// One point as stored, in the cache and on the GPU (`CPoint` in the shader).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct Point {
    pub pos: [f32; 3],
    /// RGBA8.
    pub color: u32,
    /// Octahedral unit vector, two snorm16.
    pub normal: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub start: u32,
    pub count: u32,
    pub lo: [f32; 3],
    pub hi: [f32; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Header {
    version: u32,
    /// The file read, canonical: a new reading of it replaces the cache of the old one.
    #[serde(default)]
    source: String,
    source_len: u64,
    source_mtime_ns: u128,
    pub format: String,
    pub origin: [f64; 3],
    pub lo: [f32; 3],
    pub hi: [f32; 3],
    pub has_color: bool,
    pub has_normals: bool,
    /// The normals were fitted here, not read from the file.
    pub normals_estimated: bool,
    pub chunks: Vec<Chunk>,
}

pub struct Cloud {
    pub header: Header,
    /// In chunk order.
    pub points: Vec<Point>,
    pub from_cache: bool,
    pub load_ms: f64,
}

impl Cloud {
    /// The box of the points around the origin, in scan coordinates.
    pub fn local_box(&self) -> (Vec3, Vec3) {
        (Vec3::from(self.header.lo), Vec3::from(self.header.hi))
    }

    pub fn origin(&self) -> DVec3 {
        DVec3::from(self.header.origin)
    }

    /// What tells this reading of a file from another with as many points: its box, its
    /// origin and every 4096th point, in order. A saved mask carries it.
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        h.write(bytemuck::cast_slice(&self.header.origin));
        h.write(bytemuck::cast_slice(&self.header.lo));
        h.write(bytemuck::cast_slice(&self.header.hi));
        for p in self.points.iter().step_by(4096) {
            h.write(bytemuck::bytes_of(p));
        }
        h.finish()
    }
}

/// How far a load is: bytes of the source read, of its length.
#[derive(Default)]
pub struct Progress {
    pub read: AtomicU64,
    pub total: AtomicU64,
    /// 0 reading, 1 sorting, 2 normals, 3 writing the cache.
    pub stage: AtomicU64,
    pub cancel: AtomicBool,
}

/// Where scans are cached: next to the worker's mesh cache, a folder per cache version, so
/// builds of other versions side by side keep their own.
pub fn cache_dir() -> Option<PathBuf> {
    let version = format!("v{CACHE_VERSION}");
    if let Some(d) = std::env::var_os("VIBECAD_CACHE_DIR") {
        return Some(PathBuf::from(d).join("scans").join(version));
    }
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("VibeCAD").join("cache")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/VibeCAD")
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache")))?.join("vibecad")
    };
    Some(base.join("scans").join(version))
}

/// The scan in `path`, from the cache when it has this file as it is now, else read and cached.
pub fn load(path: &Path, cache: Option<&Path>, progress: &Progress) -> Result<Cloud> {
    let started = Instant::now();
    let meta = std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    progress.total.store(meta.len(), std::sync::atomic::Ordering::Relaxed);
    let cache_file = cache.map(|d| d.join(format!("{:016x}.scan", cache_key(path, meta.len(), mtime))));
    if let Some(f) = &cache_file
        // Used: the last to go when the cache is trimmed, also while it is read.
        && std::fs::File::options().write(true).open(f).and_then(|f| f.set_modified(std::time::SystemTime::now())).is_ok()
        && let Ok(mut c) = read_cache(f)
        && c.header.source_len == meta.len()
        && c.header.source_mtime_ns == mtime
    {
        prune(f, &c.header.source, CACHE_LIMIT);
        c.load_ms = started.elapsed().as_secs_f64() * 1e3;
        return Ok(c);
    }
    let imported = formats::read(path, &progress.read, &progress.cancel)?;
    let mut cloud = build(imported, progress)?;
    cloud.header.source_len = meta.len();
    cloud.header.source_mtime_ns = mtime;
    cloud.header.source = canonical(path);
    if let Some(f) = &cache_file {
        progress.stage.store(3, std::sync::atomic::Ordering::Relaxed);
        // A cache that cannot be written only costs the next load its time.
        match write_cache(f, &cloud) {
            Ok(()) => prune(f, &cloud.header.source, CACHE_LIMIT),
            Err(e) => eprintln!("scan cache {}: {e:#}", f.display()),
        }
    }
    cloud.load_ms = started.elapsed().as_secs_f64() * 1e3;
    Ok(cloud)
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

fn cache_key(path: &Path, len: u64, mtime: u128) -> u64 {
    // FNV-1a over the path and what identifies its contents.
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.write(canonical(path).as_bytes());
    h.write(&len.to_le_bytes());
    h.write(&mtime.to_le_bytes());
    h.write(&CACHE_VERSION.to_le_bytes());
    h.finish()
}

struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Centre the points, sort them into chunks, shuffle each chunk, fit missing normals.
pub fn build(imported: formats::Imported, progress: &Progress) -> Result<Cloud> {
    use std::sync::atomic::Ordering::Relaxed;
    let formats::Imported { base, mut points, has_color, has_normals, format } = imported;
    progress.stage.store(1, Relaxed);
    let (lo, hi) = bounds(&points);
    let centre = (lo + hi) * 0.5;
    let origin = DVec3::from(base) + centre.as_dvec3();
    points.par_iter_mut().for_each(|p| p.pos = (Vec3::from(p.pos) - centre).into());
    let (lo, hi) = (lo - centre, hi - centre);
    // Morton order over the cube around the points.
    let side = (hi - lo).max_element().max(1e-9);
    let scale = ((1u32 << 21) - 1) as f32 / side;
    // Points of one cell are ordered by all they hold, so the order is the same on every
    // reading: a saved mask names points by it.
    points.par_sort_unstable_by_key(|p| (morton(((Vec3::from(p.pos) - lo) * scale).as_uvec3().to_array()), p.pos.map(f32::to_bits), p.color, p.normal));
    points.par_chunks_mut(CHUNK).enumerate().for_each(|(k, c)| shuffle(c, k as u64));
    if progress.cancel.load(Relaxed) {
        bail!("cancelled");
    }
    if !has_normals {
        progress.stage.store(2, Relaxed);
        estimate_normals(&mut points, &progress.cancel);
        if progress.cancel.load(Relaxed) {
            bail!("cancelled");
        }
    }
    let chunks = points
        .par_chunks(CHUNK)
        .enumerate()
        .map(|(k, c)| {
            let (lo, hi) = bounds(c);
            Chunk { start: (k * CHUNK) as u32, count: c.len() as u32, lo: lo.into(), hi: hi.into() }
        })
        .collect();
    let header = Header {
        version: CACHE_VERSION,
        source: String::new(),
        source_len: 0,
        source_mtime_ns: 0,
        format: format.to_string(),
        origin: origin.into(),
        lo: lo.into(),
        hi: hi.into(),
        has_color,
        has_normals: true,
        normals_estimated: !has_normals,
        chunks,
    };
    Ok(Cloud { header, points, from_cache: false, load_ms: 0.0 })
}

fn bounds(points: &[Point]) -> (Vec3, Vec3) {
    points
        .par_iter()
        .fold(|| (Vec3::MAX, Vec3::MIN), |(lo, hi), p| (lo.min(Vec3::from(p.pos)), hi.max(Vec3::from(p.pos))))
        .reduce(|| (Vec3::MAX, Vec3::MIN), |a, b| (a.0.min(b.0), a.1.max(b.1)))
}

/// The bits of x, y and z interleaved, 21 bits each.
fn morton(c: [u32; 3]) -> u64 {
    fn spread(v: u32) -> u64 {
        let mut x = (v & 0x1f_ffff) as u64;
        x = (x | x << 32) & 0x1f_0000_0000_ffff;
        x = (x | x << 16) & 0x1f_0000_ff00_00ff;
        x = (x | x << 8) & 0x100f_00f0_0f00_f00f;
        x = (x | x << 4) & 0x10c3_0c30_c30c_30c3;
        x = (x | x << 2) & 0x1249_2492_4924_9249;
        x
    }
    spread(c[0]) | spread(c[1]) << 1 | spread(c[2]) << 2
}

/// Fisher-Yates with a fixed seed per chunk, so the same file always gives the same cache.
fn shuffle(c: &mut [Point], seed: u64) {
    let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for i in (1..c.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        c.swap(i, (s % (i as u64 + 1)) as usize);
    }
}

/// The normal of the plane through each point's nearest neighbours. Which way it faces is not
/// known; the view lights both sides alike.
fn estimate_normals(points: &mut [Point], cancel: &AtomicBool) {
    let tree = kd::KdTree::build(points.par_iter().map(|p| p.pos));
    let normals: Vec<u32> = points
        .par_iter()
        .map_init(Vec::new, |near, p| {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return 0; // dropped with the scan
            }
            tree.nearest(p.pos, NORMAL_NEIGHBOURS, near);
            let n = plane_normal(near.iter().map(|&(_, i)| Vec3::from(points[i as usize].pos)));
            oct_encode(n.unwrap_or(Vec3::Z).to_array())
        })
        .collect();
    points.par_iter_mut().zip(normals).for_each(|(p, n)| p.normal = n);
}

/// The direction of least spread of these points: the smallest eigenvector of their covariance.
pub fn plane_normal(pts: impl Iterator<Item = Vec3> + Clone) -> Option<Vec3> {
    let n = pts.clone().count();
    if n < 3 {
        return None;
    }
    let mean = pts.clone().map(Vec3::as_dvec3).sum::<DVec3>() / n as f64;
    let mut c = [[0.0f64; 3]; 3];
    for p in pts {
        let d = p.as_dvec3() - mean;
        let d = d.to_array();
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += d[i] * d[j];
            }
        }
    }
    smallest_eigenvector(c).map(|v| v.as_vec3())
}

/// For a symmetric 3x3 matrix, by the closed form for its eigenvalues.
fn smallest_eigenvector(a: [[f64; 3]; 3]) -> Option<DVec3> {
    let p1 = a[0][1].powi(2) + a[0][2].powi(2) + a[1][2].powi(2);
    let q = (a[0][0] + a[1][1] + a[2][2]) / 3.0;
    let p2 = (a[0][0] - q).powi(2) + (a[1][1] - q).powi(2) + (a[2][2] - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();
    if !(p > 1e-30) {
        return None; // all directions alike
    }
    let b = |i: usize, j: usize| (a[i][j] - if i == j { q } else { 0.0 }) / p;
    let det = b(0, 0) * (b(1, 1) * b(2, 2) - b(1, 2) * b(2, 1)) - b(0, 1) * (b(1, 0) * b(2, 2) - b(1, 2) * b(2, 0))
        + b(0, 2) * (b(1, 0) * b(2, 1) - b(1, 1) * b(2, 0));
    let phi = (det / 2.0).clamp(-1.0, 1.0).acos() / 3.0;
    let smallest = q + 2.0 * p * (phi + 2.0 * std::f64::consts::PI / 3.0).cos();
    // The rows of A - λI span the plane at right angles to the eigenvector of λ.
    let row = |i: usize| {
        let mut r = DVec3::from(a[i]);
        r[i] -= smallest;
        r
    };
    let (r0, r1, r2) = (row(0), row(1), row(2));
    let best = [r0.cross(r1), r0.cross(r2), r1.cross(r2)].into_iter().max_by(|x, y| x.length_squared().total_cmp(&y.length_squared()))?;
    (best.length_squared() > 1e-60).then(|| best.normalize())
}

pub fn rgba(c: [u8; 3]) -> u32 {
    u32::from_le_bytes([c[0], c[1], c[2], 255])
}

/// A unit vector as two snorm16 on the octahedron; decoded in the shader.
pub fn oct_encode(n: [f32; 3]) -> u32 {
    let v = Vec3::from(n);
    let l1 = v.x.abs() + v.y.abs() + v.z.abs();
    let v = if l1 > 1e-20 { v / l1 } else { Vec3::Z };
    let sign = |x: f32| if x >= 0.0 { 1.0 } else { -1.0 };
    let (x, y) = if v.z >= 0.0 { (v.x, v.y) } else { ((1.0 - v.y.abs()) * sign(v.x), (1.0 - v.x.abs()) * sign(v.y)) };
    let q = |x: f32| ((x.clamp(-1.0, 1.0) * 32767.0).round() as i16) as u16 as u32;
    q(x) | q(y) << 16
}

pub fn oct_decode(e: u32) -> Vec3 {
    let f = |b: u32| ((b & 0xffff) as u16 as i16) as f32 / 32767.0;
    let (x, y) = (f(e), f(e >> 16));
    let mut n = Vec3::new(x, y, 1.0 - x.abs() - y.abs());
    let t = (-n.z).max(0.0);
    n.x += if n.x >= 0.0 { -t } else { t };
    n.y += if n.y >= 0.0 { -t } else { t };
    n.normalize()
}

fn write_cache(path: &Path, c: &Cloud) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let header = serde_json::to_vec(&c.header)?;
    // Two readers of one file (two windows, one file in two scans) each write their own.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("{}-{}.part", std::process::id(), WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&(header.len() as u64).to_le_bytes())?;
        w.write_all(&header)?;
        w.write_all(bytemuck::cast_slice(&c.points))?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// After a cache is written or read: drop the older readings of the same file, broken caches,
/// files left half written for a day, and then the least recently used caches until all fit
/// in `limit`. A cache that cannot be opened now (another program holds it) is left alone.
fn prune(written: &Path, source: &str, limit: u64) {
    let Some(dir) = written.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let day = std::time::Duration::from_secs(24 * 3600);
    let mut kept: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = std::fs::metadata(written).map_or(0, |m| m.len());
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        match p.extension().and_then(|x| x.to_str()) {
            Some("part") if modified.elapsed().is_ok_and(|age| age > day) => {
                let _ = std::fs::remove_file(&p);
            }
            Some("scan") if p != written => match std::fs::File::open(&p).map(|f| header_of(&mut BufReader::new(f))) {
                Err(_) => {} // held by another program: left alone
                Ok(Ok(h)) if h.source != source => {
                    total += meta.len();
                    kept.push((modified, meta.len(), p));
                }
                // Another reading of the same file, or a broken cache.
                Ok(_) => {
                    let _ = std::fs::remove_file(&p);
                }
            },
            _ => {}
        }
    }
    kept.sort_by_key(|k| k.0);
    for (_, len, p) in kept {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}

fn header_of(r: &mut impl Read) -> Result<Header> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a scan cache");
    }
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > 1 << 30 {
        bail!("scan cache header too large");
    }
    let mut header = vec![0u8; len as usize];
    r.read_exact(&mut header)?;
    let header: Header = serde_json::from_slice(&header)?;
    if header.version != CACHE_VERSION {
        bail!("scan cache of another version");
    }
    Ok(header)
}

fn read_cache(path: &Path) -> Result<Cloud> {
    let mut r = BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
    let header = header_of(&mut r)?;
    let n = header.chunks.last().map_or(0, |c| c.start as usize + c.count as usize);
    let mut points = vec![Point::zeroed(); n];
    r.read_exact(bytemuck::cast_slice_mut(&mut points))?;
    Ok(Cloud { header, points, from_cache: true, load_ms: 0.0 })
}

/// How many of a chunk's `n` points to draw when its box spans `px` pixels on screen, and how
/// wide each point is then, in pixels: about one point per `spacing` pixels across, and points
/// grown so that a sample of the chunk still covers it.
pub fn level_of_detail(n: u32, px: f32, spacing: f32, min_size: f32) -> (u32, f32) {
    if n == 0 {
        return (0, min_size);
    }
    // A chunk of a surface covers about px² / 2 pixels.
    let want = (px * px * 0.5 / (spacing * spacing)).ceil();
    let count = if want.is_finite() { (want as u64).clamp(64.min(n as u64), n as u64) as u32 } else { n };
    (count, point_size(count, px, min_size))
}

/// The width that lets `count` points cover a chunk `px` pixels across.
pub fn point_size(count: u32, px: f32, min_size: f32) -> f32 {
    let spacing = px * 0.7 / (count.max(1) as f32).sqrt();
    (spacing * 1.3).clamp(min_size, 24.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported(pts: Vec<[f64; 3]>, normals: bool) -> formats::Imported {
        let base = pts[0];
        let points = pts
            .iter()
            .map(|p| Point { pos: [(p[0] - base[0]) as f32, (p[1] - base[1]) as f32, (p[2] - base[2]) as f32], color: 0, normal: oct_encode([0.0, 0.0, 1.0]) })
            .collect();
        formats::Imported { base, points, has_color: false, has_normals: normals, format: "test" }
    }

    #[test]
    fn octahedral_normals_round_trip() {
        for n in [[0.0, 0.0, 1.0], [0.0, 0.0, -1.0], [1.0, 0.0, 0.0], [0.3, -0.5, -0.81], [-0.6, 0.64, 0.48]] {
            let v = Vec3::from(n).normalize();
            let back = oct_decode(oct_encode(v.to_array()));
            assert!(back.dot(v) > 0.99999, "{v} -> {back}");
        }
    }

    #[test]
    fn eigenvector_of_a_plane() {
        // points on a tilted plane: its normal comes out, either way round
        let n = Vec3::new(1.0, 2.0, 2.0).normalize();
        let u = n.any_orthonormal_vector();
        let v = n.cross(u);
        let pts: Vec<Vec3> = (0..30).map(|i| u * (i % 6) as f32 + v * (i / 6) as f32 * 1.3).collect();
        let got = plane_normal(pts.iter().copied()).unwrap();
        assert!(got.dot(n).abs() > 0.9999, "{got}");
        assert!(plane_normal([Vec3::ZERO; 2].into_iter()).is_none());
    }

    #[test]
    fn build_centres_chunks_and_fits_normals() {
        // a 300 x 300 grid on z = 5, far from the origin
        let pts: Vec<[f64; 3]> = (0..90_000).map(|i| [1e5 + (i % 300) as f64 * 0.1, -2e4 + (i / 300) as f64 * 0.1, 5.0]).collect();
        let c = build(imported(pts, false), &Progress::default()).unwrap();
        let h = &c.header;
        assert!((h.origin[0] - (1e5 + 14.95)).abs() < 1e-6 && (h.origin[2] - 5.0).abs() < 1e-9, "{:?}", h.origin);
        assert_eq!(h.chunks.len(), 90_000usize.div_ceil(CHUNK));
        assert_eq!(h.chunks.iter().map(|c| c.count).sum::<u32>(), 90_000);
        assert!(h.normals_estimated);
        // a plane: every fitted normal is along z
        assert!(c.points.iter().all(|p| oct_decode(p.normal).z.abs() > 0.999));
        // chunks are neighbourhoods: together they are not much bigger than the grid
        let area: f32 = h.chunks.iter().map(|c| (c.hi[0] - c.lo[0]) * (c.hi[1] - c.lo[1])).sum();
        assert!(area < 30.0 * 30.0 * 2.0, "{area}");
    }

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("vibecad-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("grid.xyz");
        let mut text = String::new();
        for i in 0..2000 {
            text += &format!("{} {} {}\n", i % 50, i / 50, (i % 7) as f32 * 0.01);
        }
        std::fs::write(&src, text).unwrap();
        let cache = dir.join("cache");
        let first = load(&src, Some(&cache), &Progress::default()).unwrap();
        assert!(!first.from_cache);
        let second = load(&src, Some(&cache), &Progress::default()).unwrap();
        assert!(second.from_cache);
        assert_eq!(first.header, second.header);
        assert_eq!(first.points, second.points);
        // the file changes: read again
        std::fs::write(&src, "0 0 0\n1 0 0\n0 1 0\n0 0 1\n").unwrap();
        let third = load(&src, Some(&cache), &Progress::default()).unwrap();
        assert!(!third.from_cache);
        assert_eq!(third.points.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_keeps_one_reading_per_file_within_its_limit() {
        let dir = std::env::temp_dir().join(format!("vibecad-scan-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("cache");
        let file = |name: &str, n: usize| {
            let f = dir.join(name);
            std::fs::write(&f, (0..n).map(|i| format!("{i} {} 0
", i % 7)).collect::<String>()).unwrap();
            f
        };
        let caches = || std::fs::read_dir(&cache).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "scan")).count();
        let a = file("a.xyz", 100);
        load(&a, Some(&cache), &Progress::default()).unwrap();
        // a new reading of the same file replaces the old one
        file("a.xyz", 120);
        load(&a, Some(&cache), &Progress::default()).unwrap();
        assert_eq!(caches(), 1);
        load(&file("b.xyz", 100), Some(&cache), &Progress::default()).unwrap();
        assert_eq!(caches(), 2);
        // over the limit: the least recently used goes, never the one just written
        let newest = cache.join(format!("{:016x}.scan", {
            let c = file("c.xyz", 100);
            load(&c, Some(&cache), &Progress::default()).unwrap();
            let m = std::fs::metadata(&c).unwrap();
            cache_key(&c, m.len(), m.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_nanos())
        }));
        prune(&newest, "nothing else", 1);
        assert_eq!(caches(), 1);
        assert!(newest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detail_follows_size_on_screen() {
        // far away: a few points, a little bigger
        let (n, s) = level_of_detail(CHUNK as u32, 20.0, 1.5, 1.5);
        assert!(n < 200 && s >= 1.5, "{n} {s}");
        // close: all of them, at the smallest size
        let (n, s) = level_of_detail(CHUNK as u32, 2000.0, 1.5, 1.5);
        assert_eq!(n, CHUNK as u32);
        assert!(s > 1.5);
        assert_eq!(level_of_detail(10, 1.0, 1.5, 2.0).0, 10);
    }
}
