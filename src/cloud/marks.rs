//! What is marked on a scan's points: selected, deleted. Two bits a point, sixteen points a
//! word, in the order of the scan's cache, so the words go to the GPU as they are (see
//! `vs_point`). Selection lives in the window only; the deleted points are saved in the document
//! as a mask file (`write_mask`), which names points by their place in that same order.
//!
//! Also here: selecting by what the points look like on screen, inside a lasso or nearest a
//! click.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Result, bail};
use glam::{Mat4, Vec2, Vec3};
use rayon::prelude::*;

use super::{CHUNK, CACHE_VERSION, Chunk, Point};

pub const SELECTED: u32 = 1;
pub const DELETED: u32 = 2;
/// The selected (low) and deleted (high) bit of every point of a word.
const SEL_BITS: u32 = 0x5555_5555;
const DEL_BITS: u32 = 0xAAAA_AAAA;
const WORDS_PER_CHUNK: usize = CHUNK / 16;
const MASK_MAGIC: &[u8; 8] = b"VIBEMASK";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    /// The points found become the selection.
    Replace,
    Add,
    Remove,
}

#[derive(Clone, Default)]
pub struct Marks {
    words: Vec<u32>,
    n: usize,
    /// Points selected and deleted, counted after every change (each a pass over the words
    /// anyway), so the window can ask every frame.
    selected: usize,
    deleted: usize,
}

impl Marks {
    pub fn new(n: usize) -> Marks {
        Marks { words: vec![0; n.div_ceil(16)], n, selected: 0, deleted: 0 }
    }

    fn recount(&mut self) {
        let (s, d) = self.words.par_iter().map(|w| ((w & SEL_BITS).count_ones() as usize, (w & DEL_BITS).count_ones() as usize)).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        (self.selected, self.deleted) = (s, d);
    }

    pub fn words(&self) -> &[u32] {
        &self.words
    }

    pub fn get(&self, i: usize) -> u32 {
        (self.words[i / 16] >> ((i % 16) * 2)) & 3
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn deleted(&self) -> usize {
        self.deleted
    }

    /// Select or unselect one point; a deleted one stays as it is.
    pub fn set_selected(&mut self, i: usize, on: bool) {
        let (w, shift) = (&mut self.words[i / 16], (i % 16) * 2);
        if *w >> shift & DELETED == 0 {
            let was = *w >> shift & SELECTED != 0;
            *w = if on { *w | SELECTED << shift } else { *w & !(SELECTED << shift) };
            self.selected = self.selected + on as usize - was as usize;
        }
    }

    pub fn clear_selection(&mut self) {
        self.words.par_iter_mut().for_each(|w| *w &= !SEL_BITS);
        self.selected = 0;
    }

    /// The points not deleted and not selected become the selection.
    pub fn invert_selection(&mut self) {
        let n = self.n;
        self.words.par_iter_mut().enumerate().for_each(|(k, w)| {
            let deleted = (*w & DEL_BITS) >> 1;
            let sel = !*w & SEL_BITS & !deleted & live(k, n);
            *w = (*w & DEL_BITS) | sel;
        });
        self.recount();
    }

    /// The selection, deleted.
    pub fn delete_selected(&mut self) {
        self.words.par_iter_mut().for_each(|w| *w = (*w & DEL_BITS) | ((*w & SEL_BITS) << 1));
        self.recount();
    }

    /// The deleted points as one bit each (bit i % 32 of word i / 32), for the mask file.
    pub fn deleted_bits(&self) -> Vec<u32> {
        let mut out = vec![0u32; self.n.div_ceil(32)];
        out.par_iter_mut().enumerate().for_each(|(k, o)| {
            for half in 0..2 {
                let w = self.words.get(2 * k + half).copied().unwrap_or(0);
                for i in 0..16 {
                    if w >> (2 * i + 1) & 1 != 0 {
                        *o |= 1 << (half * 16 + i);
                    }
                }
            }
        });
        out
    }

    /// Deleted are these points (one bit each) and no others; a deleted point is not selected.
    pub fn set_deleted(&mut self, bits: &[u32]) {
        let n = self.n;
        self.words.par_iter_mut().enumerate().for_each(|(k, w)| {
            let b = bits.get(k / 2).copied().unwrap_or(0) >> ((k % 2) * 16) & 0xffff;
            let mut del = 0u32;
            for i in 0..16 {
                if b >> i & 1 != 0 {
                    del |= 2 << (2 * i);
                }
            }
            // Bits past the last point (a mask from elsewhere) mean nothing.
            let del = del & (live(k, n) << 1);
            *w = (*w & SEL_BITS & !(del >> 1)) | del;
        });
        self.recount();
    }

    /// Select (or unselect) the points not deleted for which `hit` holds, chunk by chunk in
    /// parallel; `look` says whether a chunk may hold any (so most are passed over whole).
    pub fn select(&mut self, points: &[Point], chunks: &[Chunk], op: Op, look: impl Fn(&Chunk) -> bool + Sync, hit: impl Fn(&Point) -> bool + Sync) {
        if op == Op::Replace {
            self.clear_selection();
        }
        self.words.par_chunks_mut(WORDS_PER_CHUNK).zip(points.par_chunks(CHUNK)).zip(chunks.par_iter()).for_each(|((words, pts), chunk)| {
            if !look(chunk) {
                return;
            }
            for (i, p) in pts.iter().enumerate() {
                let (w, shift) = (&mut words[i / 16], (i % 16) * 2);
                if *w >> shift & DELETED != 0 || !hit(p) {
                    continue;
                }
                match op {
                    Op::Remove => *w &= !(SELECTED << shift),
                    _ => *w |= SELECTED << shift,
                }
            }
        });
        self.recount();
    }
}

/// The selected bits of word `k` that stand for points (the last word may run past `n`).
fn live(k: usize, n: usize) -> u32 {
    let first = k * 16;
    if first + 16 <= n {
        SEL_BITS
    } else {
        let left = n.saturating_sub(first);
        SEL_BITS & ((1u64 << (2 * left)) - 1) as u32
    }
}

/// Write the deleted points of a scan; `fingerprint` is its `Cloud::fingerprint`.
pub fn write_mask(path: &Path, marks: &Marks, fingerprint: u64) -> Result<()> {
    let mut w = BufWriter::new(std::fs::File::create(path)?);
    w.write_all(MASK_MAGIC)?;
    w.write_all(&CACHE_VERSION.to_le_bytes())?;
    w.write_all(&(marks.n as u64).to_le_bytes())?;
    w.write_all(&fingerprint.to_le_bytes())?;
    w.write_all(bytemuck::cast_slice(&marks.deleted_bits()))?;
    w.flush()?;
    Ok(())
}

/// The deleted points of a mask written for this scan (`n` points, `fingerprint`), one bit each.
pub fn read_mask(path: &Path, n: usize, fingerprint: u64) -> Result<Vec<u32>> {
    let mut r = BufReader::new(std::fs::File::open(path)?);
    let mut head = [0u8; 28];
    r.read_exact(&mut head)?;
    if &head[..8] != MASK_MAGIC {
        bail!("not a mask of deleted points");
    }
    let version = u32::from_le_bytes(head[8..12].try_into().unwrap());
    let count = u64::from_le_bytes(head[12..20].try_into().unwrap());
    let print = u64::from_le_bytes(head[20..28].try_into().unwrap());
    if version != CACHE_VERSION || count != n as u64 || print != fingerprint {
        bail!("its deleted points were saved for another version of its file: all are shown");
    }
    let mut bits = vec![0u32; n.div_ceil(32)];
    r.read_exact(bytemuck::cast_slice_mut(&mut bits))?;
    Ok(bits)
}

/// Where points fall on screen, for one scan: `mvp` takes its coordinates to clip space, the
/// view is `size` pixels with y down.
#[derive(Clone, Copy)]
pub struct Screen {
    pub mvp: Mat4,
    pub size: Vec2,
}

impl Screen {
    /// Pixel and depth (0 near, 1 far) of a point, if it is in front of the eye and within the
    /// depth of the view.
    pub fn project(&self, p: Vec3) -> Option<(Vec2, f32)> {
        let c = self.mvp * p.extend(1.0);
        if c.w <= 0.0 || c.z < 0.0 || c.z > c.w {
            return None;
        }
        let ndc = c.truncate() / c.w;
        Some((Vec2::new((ndc.x * 0.5 + 0.5) * self.size.x, (0.5 - ndc.y * 0.5) * self.size.y), ndc.z))
    }

    /// The pixels a box covers, or None when it is wholly behind the eye or past the view's
    /// depth; everything when it reaches behind the eye (it cannot be bounded then).
    pub fn box_rect(&self, lo: Vec3, hi: Vec3) -> Option<(Vec2, Vec2)> {
        let (mut a, mut b) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
        let mut any = false;
        for i in 0..8 {
            let p = Vec3::new(if i & 1 == 0 { lo.x } else { hi.x }, if i & 2 == 0 { lo.y } else { hi.y }, if i & 4 == 0 { lo.z } else { hi.z });
            let c = self.mvp * p.extend(1.0);
            if c.w <= 1e-9 {
                return Some((Vec2::splat(f32::MIN), Vec2::splat(f32::MAX)));
            }
            let ndc = c.truncate() / c.w;
            any |= ndc.z <= 1.0;
            let px = Vec2::new((ndc.x * 0.5 + 0.5) * self.size.x, (0.5 - ndc.y * 0.5) * self.size.y);
            a = a.min(px);
            b = b.max(px);
        }
        any.then_some((a, b))
    }
}

/// A lasso filled in on the pixels it covers (even-odd), over its own bounding rectangle.
pub struct Lasso {
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
    inside: Vec<bool>,
}

impl Lasso {
    /// From the path the pointer drew, in pixels; `None` for a path too small to hold anything.
    pub fn new(path: &[Vec2]) -> Option<Lasso> {
        if path.len() < 3 {
            return None;
        }
        let (lo, hi) = path.iter().fold((Vec2::MAX, Vec2::MIN), |(a, b), p| (a.min(*p), b.max(*p)));
        let (x0, y0) = (lo.x.floor() as i32, lo.y.floor() as i32);
        let (w, h) = ((hi.x.ceil() as i32 - x0 + 1).min(16384), (hi.y.ceil() as i32 - y0 + 1).min(16384));
        if w < 2 || h < 2 {
            return None;
        }
        let mut inside = vec![false; (w * h) as usize];
        let mut xs: Vec<f32> = Vec::new();
        for row in 0..h {
            // Scanline through pixel centres: where it crosses the closed path.
            let y = (y0 + row) as f32 + 0.5;
            xs.clear();
            for k in 0..path.len() {
                let (a, b) = (path[k], path[(k + 1) % path.len()]);
                if (a.y <= y) != (b.y <= y) {
                    xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
                }
            }
            xs.sort_by(f32::total_cmp);
            for span in xs.chunks_exact(2) {
                let from = ((span[0] - 0.5).ceil() as i32 - x0).clamp(0, w);
                let to = ((span[1] - 0.5).floor() as i32 - x0 + 1).clamp(0, w);
                for x in from..to {
                    inside[(row * w + x) as usize] = true;
                }
            }
        }
        Some(Lasso { x0, y0, w, h, inside })
    }

    pub fn contains(&self, p: Vec2) -> bool {
        let (x, y) = (p.x.floor() as i32 - self.x0, p.y.floor() as i32 - self.y0);
        x >= 0 && y >= 0 && x < self.w && y < self.h && self.inside[(y * self.w + x) as usize]
    }

    /// Whether a rectangle of pixels meets the lasso's.
    pub fn meets(&self, (a, b): (Vec2, Vec2)) -> bool {
        b.x >= self.x0 as f32 && b.y >= self.y0 as f32 && a.x <= (self.x0 + self.w) as f32 && a.y <= (self.y0 + self.h) as f32
    }
}

/// The point nearest the eye among those within `reach` pixels of `at`, not deleted: its index
/// and depth.
pub fn nearest(points: &[Point], chunks: &[Chunk], marks: &Marks, screen: &Screen, at: Vec2, reach: f32) -> Option<(usize, f32)> {
    points
        .par_chunks(CHUNK)
        .zip(chunks.par_iter())
        .enumerate()
        .filter_map(|(k, (pts, c))| {
            let (a, b) = screen.box_rect(Vec3::from(c.lo), Vec3::from(c.hi))?;
            if at.x < a.x - reach || at.y < a.y - reach || at.x > b.x + reach || at.y > b.y + reach {
                return None;
            }
            pts.iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let index = k * CHUNK + i;
                    if marks.get(index) & DELETED != 0 {
                        return None;
                    }
                    let (px, depth) = screen.project(Vec3::from(p.pos))?;
                    (px.distance_squared(at) <= reach * reach).then_some((index, depth))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_select_delete_invert_and_mask() {
        let n = 40; // past two whole words and into a third
        let mut m = Marks::new(n);
        let pts: Vec<Point> = (0..n).map(|i| Point { pos: [i as f32, 0.0, 0.0], color: 0, normal: 0 }).collect();
        let chunks = [Chunk { start: 0, count: n as u32, lo: [0.0; 3], hi: [n as f32, 0.0, 0.0] }];
        m.select(&pts, &chunks, Op::Replace, |_| true, |p| p.pos[0] < 10.0);
        assert_eq!(m.selected(), 10);
        m.select(&pts, &chunks, Op::Remove, |_| true, |p| p.pos[0] < 5.0);
        assert_eq!(m.selected(), 5);
        m.delete_selected();
        assert_eq!((m.selected(), m.deleted()), (0, 5));
        assert_eq!(m.get(7), DELETED);
        // deleted points are never selected again
        m.select(&pts, &chunks, Op::Add, |_| true, |_| true);
        assert_eq!(m.selected(), 35);
        m.clear_selection();
        m.invert_selection();
        assert_eq!(m.selected(), 35, "the padding past n stays clear");

        let dir = std::env::temp_dir().join(format!("vibecad-mask-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("m.mask");
        write_mask(&f, &m, 77).unwrap();
        let bits = read_mask(&f, n, 77).unwrap();
        let mut back = Marks::new(n);
        back.set_deleted(&bits);
        assert_eq!(back.deleted(), 5);
        assert!((5..10).all(|i| back.get(i) == DELETED) && back.get(4) == 0);
        assert!(read_mask(&f, n + 1, 77).is_err(), "a mask of another reading is refused");
        assert!(read_mask(&f, n, 78).is_err(), "a mask of another file of as many points is refused");
        // bits past the last point are dropped
        let mut odd = Marks::new(n);
        odd.set_deleted(&[0, 0xffff_ffff]);
        assert_eq!(odd.deleted(), n - 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lasso_fills_its_inside() {
        // a triangle (0,0) (100,0) (0,100)
        let l = Lasso::new(&[Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), Vec2::new(0.0, 100.0)]).unwrap();
        assert!(l.contains(Vec2::new(10.0, 10.0)));
        assert!(l.contains(Vec2::new(45.0, 45.0)));
        assert!(!l.contains(Vec2::new(60.0, 60.0)));
        assert!(!l.contains(Vec2::new(-5.0, 5.0)));
        assert!(Lasso::new(&[Vec2::ZERO, Vec2::ONE]).is_none());
    }

    #[test]
    fn nearest_point_to_a_click() {
        // an orthographic view of x, y in [-10, 10] onto 200 x 200 pixels, z toward the eye
        let mvp = Mat4::from_cols(glam::Vec4::new(0.1, 0.0, 0.0, 0.0), glam::Vec4::new(0.0, 0.1, 0.0, 0.0), glam::Vec4::new(0.0, 0.0, -0.05, 0.0), glam::Vec4::new(0.0, 0.0, 0.5, 1.0));
        let s = Screen { mvp, size: Vec2::new(200.0, 200.0) };
        let pts = vec![
            Point { pos: [0.0, 0.0, -1.0], color: 0, normal: 0 },
            Point { pos: [0.0, 0.0, 2.0], color: 0, normal: 0 }, // nearer the eye
            Point { pos: [5.0, 5.0, 0.0], color: 0, normal: 0 },
        ];
        let chunks = [Chunk { start: 0, count: 3, lo: [0.0, 0.0, -1.0], hi: [5.0, 5.0, 2.0] }];
        let mut m = Marks::new(3);
        assert_eq!(nearest(&pts, &chunks, &m, &s, Vec2::new(100.0, 100.0), 4.0).map(|h| h.0), Some(1));
        m.select(&pts, &chunks, Op::Replace, |_| true, |p| p.pos[2] > 1.0);
        m.delete_selected();
        assert_eq!(nearest(&pts, &chunks, &m, &s, Vec2::new(100.0, 100.0), 4.0).map(|h| h.0), Some(0));
        // (5, 5) is at pixel (150, 50): y down
        assert_eq!(nearest(&pts, &chunks, &m, &s, Vec2::new(151.0, 49.0), 4.0).map(|h| h.0), Some(2));
        assert!(nearest(&pts, &chunks, &m, &s, Vec2::new(10.0, 190.0), 4.0).is_none());
    }
}
