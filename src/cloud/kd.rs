//! A static k-d tree over the points of a scan, for nearest-neighbour queries: normals now, and
//! picking regions of a scan later. Implicit: the points are reordered so that every subtree is
//! a range, split at its middle; the axis of each split is kept at the index of its middle point.

use rayon::prelude::*;

const LEAF: usize = 12;

#[derive(Clone, Copy)]
struct Entry {
    p: [f32; 3],
    id: u32,
}

pub struct KdTree {
    entries: Vec<Entry>,
    /// The split axis of the subtree whose middle point is at this index.
    axes: Vec<u8>,
}

impl KdTree {
    pub fn build(points: impl IndexedParallelIterator<Item = [f32; 3]>) -> KdTree {
        let mut entries: Vec<Entry> = points.enumerate().map(|(i, p)| Entry { p, id: i as u32 }).collect();
        let mut axes = vec![0u8; entries.len()];
        split(&mut entries, &mut axes);
        KdTree { entries, axes }
    }

    /// The `k` points nearest to `q`, nearest first, as (squared distance, point index).
    pub fn nearest(&self, q: [f32; 3], k: usize, out: &mut Vec<(f32, u32)>) {
        out.clear();
        if k == 0 {
            return;
        }
        self.search(0, self.entries.len(), q, k, out);
    }

    fn search(&self, lo: usize, hi: usize, q: [f32; 3], k: usize, out: &mut Vec<(f32, u32)>) {
        if hi - lo <= LEAF {
            for e in &self.entries[lo..hi] {
                offer(out, k, d2(e.p, q), e.id);
            }
            return;
        }
        let mid = (lo + hi) / 2;
        let axis = self.axes[mid] as usize;
        let e = self.entries[mid];
        offer(out, k, d2(e.p, q), e.id);
        let delta = q[axis] - e.p[axis];
        let (near, far) = if delta < 0.0 { ((lo, mid), (mid + 1, hi)) } else { ((mid + 1, hi), (lo, mid)) };
        self.search(near.0, near.1, q, k, out);
        if out.len() < k || delta * delta < out[out.len() - 1].0 {
            self.search(far.0, far.1, q, k, out);
        }
    }
}

/// Keep `out` the `k` smallest so far, sorted.
fn offer(out: &mut Vec<(f32, u32)>, k: usize, d: f32, id: u32) {
    if out.len() == k {
        if d >= out[k - 1].0 {
            return;
        }
        out.pop();
    }
    let at = out.partition_point(|x| x.0 <= d);
    out.insert(at, (d, id));
}

fn d2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    x * x + y * y + z * z
}

fn split(entries: &mut [Entry], axes: &mut [u8]) {
    if entries.len() <= LEAF {
        return;
    }
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for e in entries.iter() {
        for a in 0..3 {
            lo[a] = lo[a].min(e.p[a]);
            hi[a] = hi[a].max(e.p[a]);
        }
    }
    let axis = (0..3).max_by(|&a, &b| (hi[a] - lo[a]).total_cmp(&(hi[b] - lo[b]))).unwrap_or(0);
    let mid = entries.len() / 2;
    entries.select_nth_unstable_by(mid, |a, b| a.p[axis].total_cmp(&b.p[axis]));
    axes[mid] = axis as u8;
    let (left, rest) = entries.split_at_mut(mid);
    let (left_axes, rest_axes) = axes.split_at_mut(mid);
    let (right, right_axes) = (&mut rest[1..], &mut rest_axes[1..]);
    if left.len() > 50_000 {
        rayon::join(|| split(left, left_axes), || split(right, right_axes));
    } else {
        split(left, left_axes);
        split(right, right_axes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_matches_brute_force() {
        let mut s = 12345u32;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s % 10_000) as f32 / 100.0
        };
        let pts: Vec<[f32; 3]> = (0..5000).map(|_| [rnd(), rnd(), rnd()]).collect();
        let tree = KdTree::build(pts.par_iter().copied());
        let mut out = Vec::new();
        for q in [[50.0, 50.0, 50.0], [0.0, 0.0, 0.0], [99.0, 1.0, 42.0]] {
            tree.nearest(q, 8, &mut out);
            let mut brute: Vec<(f32, u32)> = pts.iter().enumerate().map(|(i, p)| (d2(*p, q), i as u32)).collect();
            brute.sort_by(|a, b| a.0.total_cmp(&b.0));
            let want: Vec<f32> = brute[..8].iter().map(|x| x.0).collect();
            let got: Vec<f32> = out.iter().map(|x| x.0).collect();
            assert_eq!(got, want);
        }
    }
}
