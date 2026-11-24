//! Turns the worker's scene stream into what the renderer uploads. The stream starts with every
//! instance's placement and box ("boxes"), then brings the meshes in batches, biggest first.
//! `SceneData` holds what is known so far; `add_meshes` takes in one batch.
//!
//! The worker sends each shape once, with points shared between its faces. Here every vertex is
//! split per face (so a face has its own id for picking) and edges are taken from the mesh itself,
//! so they lie exactly on the triangles: where two faces meet at an angle, or where a curved face
//! meets another (a cylinder's rim, a fillet's border). Shapes without faces (sketches, wires)
//! come with their B-rep edges instead. Normals are smoothed across faces that meet flatter than
//! `SMOOTH_DEG`, so a solid sewn from a coarse mesh (a cable, a screw) shades round, not faceted. Instances are not copied; the GPU places each mesh once per instance. Faces are
//! numbered in the order their meshes arrive, so ids already given out never change.

use std::collections::HashMap;

use anyhow::{Result, bail};
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use serde_json::Value;

/// Where two flat faces meet, an edge is drawn if they fold by more than `STRONG_EDGE_DEG`, or by
/// more than `WEAK_EDGE_DEG` when it carries on in a straight line from an edge that is drawn
/// (edge detection with hysteresis). A coarse mesh of a round part (a cable) folds by 20-35
/// degrees all over: its seams stay hidden instead of showing as scattered dashes, while a crease
/// that softens along its length stays one line.
const WEAK_EDGE_DEG: f32 = 25.0;
const STRONG_EDGE_DEG: f32 = 40.0;
/// How far a weak edge may turn from the line it continues.
const CONTINUE_DEG: f32 = 30.0;
/// Triangles whose normals differ by less than this are shaded as one smooth surface.
const SMOOTH_DEG: f32 = 40.0;
/// FreeCAD's default shape colour, 0.8 grey: what an object without colours of its own has.
pub const DEFAULT_COLOR: [u8; 4] = [204, 204, 204, 255];

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    /// 0-based face of the mesh; FreeCAD calls it `Face{face + 1}`.
    pub face: u32,
}

/// One shape in its own coordinates, with vertices split per face.
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Line list.
    pub edges: Vec<[f32; 3]>,
    pub faces: u32,
    /// Closed solids with outward, counter-clockwise triangles: faces turned away may be skipped.
    pub closed: bool,
}

impl Mesh {
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Meshes of one batch of the stream, with their index in the scene.
pub struct MeshBatch {
    pub meshes: Vec<(u32, Mesh)>,
    /// Time spent decoding the batch, in milliseconds.
    pub decode_ms: f64,
}

/// What the scene knows of a mesh before and after it arrives.
pub struct MeshInfo {
    pub faces: u32,
    /// Bounds of the shape in its own coordinates, from FreeCAD's box.
    pub local_box: Option<(Vec3, Vec3)>,
    /// Triangles, once the mesh is here.
    pub triangles: Option<usize>,
    pub closed: bool,
}

pub struct FaceRef {
    pub instance: u32,
    /// 0-based; FreeCAD calls it `Face{face + 1}`.
    pub face: u32,
}

pub struct InstanceInfo {
    pub obj: String,
    pub label: String,
    pub mesh: u32,
    /// Mesh coordinates to document coordinates.
    pub matrix: Mat4,
    /// Faces numbered before this instance's: face `f` here has the global id
    /// `face_base + f + 1`. `None` until its mesh arrives.
    pub face_base: Option<u32>,
    /// Bounds in document coordinates: of the shape's box until the mesh arrives, then of the
    /// placed points. `None` for a shape without either.
    pub bbox: Option<(Vec3, Vec3)>,
    /// As the worker sent them: one colour, or one per face.
    pub colors: Vec<[u8; 4]>,
}

impl InstanceInfo {
    /// A mirroring matrix turns the winding of the triangles around.
    pub fn mirrored(&self) -> bool {
        self.matrix.determinant() < 0.0
    }
}

pub struct SceneData {
    pub meshes: Vec<MeshInfo>,
    pub instances: Vec<InstanceInfo>,
    /// One per global face, indexed like `faces`.
    pub face_colors: Vec<[u8; 4]>,
    /// Global face id - 1 -> instance and face; what picking maps an id back with.
    pub faces: Vec<FaceRef>,
    pub bbox: Option<(Vec3, Vec3)>,
    /// Faces of all instances once every mesh is here: what the face numbering will reach.
    pub total_instance_faces: usize,
    /// Per mesh, the instances that place it.
    users: Vec<Vec<u32>>,
}

impl SceneData {
    /// Triangles of the instances whose mesh is here, as drawn when everything is in view.
    pub fn triangles(&self) -> usize {
        self.instances.iter().filter_map(|i| self.meshes[i.mesh as usize].triangles).sum()
    }

    /// Meshes that arrived, of all.
    pub fn loaded(&self) -> (usize, usize) {
        (self.meshes.iter().filter(|m| m.triangles.is_some()).count(), self.meshes.len())
    }

    /// Number the faces of the instances of these meshes after those already numbered, take
    /// their colours and tighten their boxes to the placed points. Returns those instances.
    pub fn add_meshes(&mut self, batch: &MeshBatch) -> Result<Vec<u32>> {
        let mut placed = Vec::new();
        for (k, mesh) in &batch.meshes {
            let k = *k as usize;
            let Some(info) = self.meshes.get_mut(k) else { bail!("the worker sent mesh {k}, which the scene does not have") };
            if info.triangles.is_some() {
                bail!("the worker sent mesh {k} twice");
            }
            info.triangles = Some(mesh.triangles());
            info.closed = mesh.closed;
            info.faces = mesh.faces;
            for &i in &self.users[k] {
                let inst = &mut self.instances[i as usize];
                inst.face_base = Some(self.faces.len() as u32);
                for f in 0..mesh.faces {
                    self.faces.push(FaceRef { instance: i, face: f });
                    self.face_colors.push(inst.colors.get(f as usize).or(inst.colors.first()).copied().unwrap_or(DEFAULT_COLOR));
                }
                // The placed points themselves, so a turned part gets a tight box.
                let points = mesh.vertices.iter().map(|v| v.pos).chain(mesh.edges.iter().copied());
                let m = inst.matrix;
                inst.bbox = bounds(points.map(|p| m.transform_point3(Vec3::from(p))));
                placed.push(i);
            }
        }
        self.bbox = self.instances.iter().filter_map(|i| i.bbox).reduce(|(a, b), (lo, hi)| (a.min(lo), b.max(hi)));
        Ok(placed)
    }

    /// Instances whose mesh will not come (the worker could not tessellate it).
    pub fn drop_meshes(&mut self, failed: &[u32]) -> Vec<u32> {
        let mut dropped = Vec::new();
        for &k in failed {
            if let Some(users) = self.users.get(k as usize) {
                for &i in users {
                    self.instances[i as usize].bbox = None;
                    dropped.push(i);
                }
            }
        }
        self.bbox = self.instances.iter().filter_map(|i| i.bbox).reduce(|(a, b), (lo, hi)| (a.min(lo), b.max(hi)));
        dropped
    }
}

/// The stream's first frame: every instance and the box of every shape, no meshes yet.
pub fn parse_boxes(header: &Value) -> Result<SceneData> {
    let list = |k: &str| header[k].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let meshes: Vec<MeshInfo> = list("meshes")
        .iter()
        .map(|m| MeshInfo {
            faces: m["faces"].as_u64().unwrap_or(0) as u32,
            local_box: box6(&m["bbox"]),
            triangles: None,
            closed: false,
        })
        .collect();
    let mut users = vec![Vec::new(); meshes.len()];
    let mut instances = Vec::new();
    let mut total_instance_faces = 0;
    for inst in list("instances") {
        let mesh_no = inst["mesh"].as_u64().unwrap_or(u64::MAX) as usize;
        let Some(info) = meshes.get(mesh_no) else { bail!("instance refers to missing mesh {mesh_no}") };
        let m = matrix(&inst["matrix"])?;
        let colors: Vec<[u8; 4]> = match inst["face_colors"].as_array() {
            Some(list) => list.iter().map(rgba).collect(),
            None => vec![rgba(&inst["color"])],
        };
        // The corners of the shape's box, placed, bound the placed shape.
        let bbox = info.local_box.and_then(|(a, b)| {
            bounds((0..8).map(|c| {
                let pick = |bit: u32, lo: f32, hi: f32| if c & bit == 0 { lo } else { hi };
                m.transform_point3(Vec3::new(pick(1, a.x, b.x), pick(2, a.y, b.y), pick(4, a.z, b.z)))
            }))
        });
        users[mesh_no].push(instances.len() as u32);
        total_instance_faces += info.faces as usize;
        instances.push(InstanceInfo {
            obj: inst["obj"].as_str().unwrap_or("").to_string(),
            label: inst["label"].as_str().unwrap_or("").to_string(),
            mesh: mesh_no as u32,
            matrix: m,
            face_base: None,
            bbox,
            colors,
        });
    }
    let bbox = instances.iter().filter_map(|i| i.bbox).reduce(|(a, b), (lo, hi)| (a.min(lo), b.max(hi)));
    Ok(SceneData {
        meshes,
        instances,
        face_colors: Vec::with_capacity(total_instance_faces),
        faces: Vec::with_capacity(total_instance_faces),
        bbox,
        total_instance_faces,
        users,
    })
}

/// A "meshes" frame: its meshes, decoded and split per face.
pub fn parse_meshes(header: &Value, payload: &[u8]) -> Result<MeshBatch> {
    let started = std::time::Instant::now();
    let mut meshes = Vec::new();
    for m in header["meshes"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let Some(index) = m["index"].as_u64().and_then(|i| u32::try_from(i).ok()) else { bail!("a mesh without an index") };
        meshes.push((index, prepare_mesh(m, payload)?));
    }
    Ok(MeshBatch { meshes, decode_ms: started.elapsed().as_secs_f64() * 1e3 })
}

/// [x0, y0, z0, x1, y1, z1] of finite numbers, or None.
fn box6(v: &Value) -> Option<(Vec3, Vec3)> {
    let a: Vec<f32> = v.as_array()?.iter().filter_map(Value::as_f64).map(|x| x as f32).collect();
    (a.len() == 6 && a.iter().all(|x| x.is_finite()) && a[0] <= a[3] && a[1] <= a[4] && a[2] <= a[5])
        .then(|| (Vec3::new(a[0], a[1], a[2]), Vec3::new(a[3], a[4], a[5])))
}

fn bounds(points: impl Iterator<Item = Vec3>) -> Option<(Vec3, Vec3)> {
    points.fold(None, |b, p| Some(b.map_or((p, p), |(lo, hi): (Vec3, Vec3)| (lo.min(p), hi.max(p)))))
}

fn prepare_mesh(m: &Value, payload: &[u8]) -> Result<Mesh> {
    let get = |k: &str| m[k].as_u64().unwrap_or(0) as usize;
    let (points, triangles, edge_points, edge_segments) =
        (get("points"), get("triangles"), get("edge_points"), get("edge_segments"));
    let (offset, size) = (get("offset"), get("size"));
    let expected = points
        .checked_mul(12)
        .and_then(|a| a.checked_add(triangles.checked_mul(16)?))
        .and_then(|a| a.checked_add(edge_points.checked_mul(12)?))
        .and_then(|a| a.checked_add(edge_segments.checked_mul(8)?));
    let end = offset.checked_add(size);
    if expected != Some(size) || end.is_none_or(|e| e > payload.len()) {
        bail!("mesh data does not match its header ({size} bytes, expected {expected:?})");
    }
    let mut cur = &payload[offset..offset + size];
    let pts: Vec<Vec3> = take_f32(&mut cur, points * 3).chunks_exact(3).map(Vec3::from_slice).collect();
    let corners = take_u32(&mut cur, triangles * 3);
    let tri_face = take_u32(&mut cur, triangles);
    let epts: Vec<Vec3> = take_f32(&mut cur, edge_points * 3).chunks_exact(3).map(Vec3::from_slice).collect();
    let eidx = take_u32(&mut cur, edge_segments * 2);
    if corners.iter().any(|&c| c as usize >= points) || eidx.iter().any(|&c| c as usize >= edge_points) {
        bail!("mesh index out of range");
    }

    // Split shared points per face, so that every face has its own vertices and id.
    let canon = weld(&pts);
    let mut split: HashMap<u64, u32> = HashMap::with_capacity(points * 2);
    let mut faces = m["faces"].as_u64().unwrap_or(0) as u32;
    let mut vertices: Vec<Vertex> = Vec::with_capacity(points * 2);
    let mut face_normal: Vec<Vec3> = Vec::with_capacity(points * 2);
    let mut vertex_point: Vec<u32> = Vec::with_capacity(points * 2);
    let mut indices = Vec::with_capacity(triangles * 3);
    // Area-weighted normals; `unit` for the angle tests.
    let tri_normal: Vec<Vec3> = corners
        .chunks_exact(3)
        .map(|t| (pts[t[1] as usize] - pts[t[0] as usize]).cross(pts[t[2] as usize] - pts[t[0] as usize]))
        .collect();
    let unit: Vec<Vec3> = tri_normal.iter().map(|n| n.normalize_or_zero()).collect();
    let mut around: Vec<Vec<u32>> = vec![Vec::new(); points];
    for (t, tri) in corners.chunks_exact(3).enumerate() {
        let f = tri_face[t];
        faces = faces.max(f + 1);
        for &c in tri {
            around[canon[c as usize] as usize].push(t as u32);
            let key = (c as u64) << 32 | f as u64;
            let idx = *split.entry(key).or_insert_with(|| {
                vertices.push(Vertex { pos: pts[c as usize].to_array(), normal: [0.0; 3], face: f });
                face_normal.push(Vec3::ZERO);
                vertex_point.push(canon[c as usize]);
                vertices.len() as u32 - 1
            });
            face_normal[idx as usize] += tri_normal[t];
            indices.push(idx);
        }
    }
    // Smooth across faces: every triangle at the point that turns less than SMOOTH_DEG from the
    // vertex's own face counts, so sharp folds stay sharp and facets of a curve blend.
    let cos_smooth = SMOOTH_DEG.to_radians().cos();
    for (i, v) in vertices.iter_mut().enumerate() {
        let own = face_normal[i].normalize_or_zero();
        let smooth: Vec3 = around[vertex_point[i] as usize]
            .iter()
            .filter(|&&t| unit[t as usize].dot(own) > cos_smooth)
            .map(|&t| tri_normal[t as usize])
            .sum();
        v.normal = smooth.try_normalize().unwrap_or(own).to_array();
    }

    let edges = if m["brep_edges"].as_bool() == Some(true) {
        eidx.iter().map(|&i| epts[i as usize].to_array()).collect()
    } else {
        let mut curved = vec![false; faces as usize];
        for f in m["curved_faces"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().filter_map(Value::as_u64) {
            if let Some(c) = curved.get_mut(f as usize) {
                *c = true;
            }
        }
        mesh_edges(&pts, &canon, &corners, &tri_face, &unit, &curved).iter().map(Vec3::to_array).collect()
    };
    Ok(Mesh { vertices, indices, edges, faces, closed: m["closed"].as_bool() == Some(true) })
}

/// For every point, the index of the first point at the same position: a face meshed on its own
/// repeats the points it shares with its neighbours.
fn weld(pts: &[Vec3]) -> Vec<u32> {
    let (lo, hi) = bounds(pts.iter().copied()).unwrap_or((Vec3::ZERO, Vec3::ONE));
    let quantum = ((hi - lo).length() * 1e-6).max(1e-9);
    let mut first: HashMap<[i64; 3], u32> = HashMap::with_capacity(pts.len());
    pts.iter()
        .enumerate()
        .map(|(i, p)| *first.entry((*p / quantum).round().to_array().map(|x| x as i64)).or_insert(i as u32))
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Strength {
    Strong,
    Weak,
}

/// The edges to draw, as a line list of mesh points (see `WEAK_EDGE_DEG`): the outline of open
/// shells, borders where a curved face meets another, sharp folds between flat faces, and gentler
/// folds that continue a drawn line. Never lines inside one face, so a cylinder shows its rims
/// and not its facets.
fn mesh_edges(pts: &[Vec3], canon: &[u32], corners: &[u32], tri_face: &[u32], unit: &[Vec3], curved: &[bool]) -> Vec<Vec3> {
    let (cos_weak, cos_strong) = (WEAK_EDGE_DEG.to_radians().cos(), STRONG_EDGE_DEG.to_radians().cos());
    let is_curved = |f: u32| curved.get(f as usize).copied().unwrap_or(false);
    // edge (by welded points) -> (first triangle, second triangle, how many triangles share it)
    let mut shared: HashMap<(u32, u32), (u32, u32, u32)> = HashMap::with_capacity(corners.len());
    for (t, tri) in corners.chunks_exact(3).enumerate() {
        for k in 0..3 {
            let (a, b) = (canon[tri[k] as usize], canon[tri[(k + 1) % 3] as usize]);
            if a == b {
                continue;
            }
            let e = shared.entry(if a < b { (a, b) } else { (b, a) }).or_insert((t as u32, u32::MAX, 0));
            if e.2 == 1 {
                e.1 = t as u32;
            }
            e.2 += 1;
        }
    }
    let mut candidates: Vec<((u32, u32), Strength)> = Vec::new();
    for (&key, &(t1, t2, count)) in &shared {
        let strength = if count != 2 {
            Some(Strength::Strong) // an outline, or a seam of more than two faces
        } else {
            let (fa, fb) = (tri_face[t1 as usize], tri_face[t2 as usize]);
            let fold = unit[t1 as usize].dot(unit[t2 as usize]);
            if fa == fb {
                None
            } else if is_curved(fa) || is_curved(fb) || fold < cos_strong {
                Some(Strength::Strong)
            } else if fold < cos_weak {
                Some(Strength::Weak)
            } else {
                None
            }
        };
        if let Some(s) = strength {
            candidates.push((key, s));
        }
    }
    // Hysteresis: from every strong edge, follow weak edges that carry on in a straight line.
    let mut at_point: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, &((a, b), _)) in candidates.iter().enumerate() {
        at_point.entry(a).or_default().push(i);
        at_point.entry(b).or_default().push(i);
    }
    let cos_continue = CONTINUE_DEG.to_radians().cos();
    let mut keep: Vec<bool> = candidates.iter().map(|(_, s)| *s == Strength::Strong).collect();
    let mut stack: Vec<usize> = (0..candidates.len()).filter(|&i| keep[i]).collect();
    while let Some(i) = stack.pop() {
        let (a, b) = candidates[i].0;
        for (at, from) in [(a, b), (b, a)] {
            let back = (pts[from as usize] - pts[at as usize]).normalize_or_zero();
            for &j in at_point.get(&at).map(Vec::as_slice).unwrap_or(&[]) {
                if keep[j] {
                    continue;
                }
                let (c, d) = candidates[j].0;
                let to = if c == at { d } else { c };
                let ahead = (pts[to as usize] - pts[at as usize]).normalize_or_zero();
                if back.dot(ahead) < -cos_continue {
                    keep[j] = true;
                    stack.push(j);
                }
            }
        }
    }
    candidates
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .flat_map(|(((a, b), _), _)| [pts[*a as usize], pts[*b as usize]])
        .collect()
}

fn matrix(v: &Value) -> Result<Mat4> {
    let a: Vec<f32> = v.as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect()).unwrap_or_default();
    if a.len() != 16 || a.iter().any(|x| !x.is_finite()) {
        bail!("instance matrix must have 16 finite numbers");
    }
    // FreeCAD's Matrix.A is row-major; glam is column-major.
    Ok(Mat4::from_cols_slice(&a).transpose())
}

fn rgba(v: &Value) -> [u8; 4] {
    let c: Vec<f64> = v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
    if c.len() < 3 {
        return DEFAULT_COLOR;
    }
    let to = |x: f64| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    [to(c[0]), to(c[1]), to(c[2]), to(*c.get(3).unwrap_or(&1.0))]
}

fn take_f32(cur: &mut &[u8], n: usize) -> Vec<f32> {
    let (head, rest) = cur.split_at(n * 4);
    *cur = rest;
    head.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

fn take_u32(cur: &mut &[u8], n: usize) -> Vec<u32> {
    let (head, rest) = cur.split_at(n * 4);
    *cur = rest;
    head.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(points: &[f32], tris: &[u32], faces: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        points.iter().for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        tris.iter().chain(faces).for_each(|x| b.extend_from_slice(&x.to_le_bytes()));
        b
    }

    /// The boxes frame: one folded square (two faces) and a unit cube, the square placed three
    /// times, the cube once.
    fn boxes() -> Value {
        json!({
            "meshes": [{"faces": 2, "bbox": [0, 0, 0, 2, 2, 2]}, {"faces": 6, "bbox": [0, 0, 0, 2, 2, 2]}],
            "total_faces": 8,
            "instances": [
                {"obj": "A", "label": "A", "mesh": 0, "matrix": [1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1], "color": [1,0,0,1]},
                {"obj": "B", "label": "B", "mesh": 0, "matrix": [1,0,0,10, 0,1,0,0, 0,0,1,0, 0,0,0,1],
                 "face_colors": [[0,1,0,1], [0,0,1,1]]},
                // mirrored in x and lifted 5 along z
                {"obj": "C", "label": "C", "mesh": 0, "matrix": [-1,0,0,0, 0,1,0,0, 0,0,1,5, 0,0,0,1], "color": [1,1,1,1]},
                {"obj": "D", "label": "D", "mesh": 1, "matrix": [1,0,0,0, 0,1,0,-20, 0,0,1,0, 0,0,0,1], "color": [0,0,0,1]}
            ]
        })
    }

    /// A meshes frame with the folded square: two triangles, folded 90 degrees, as two faces.
    fn folded() -> (Value, Vec<u8>) {
        let pts = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0];
        let p = payload(&pts, &[0, 1, 2, 0, 3, 1], &[0, 1]);
        let header = json!({"meshes": [{"index": 0, "points": 4, "triangles": 2, "faces": 2, "brep_edges": false,
                                         "closed": true, "edge_points": 0, "edge_segments": 0, "offset": 0, "size": p.len()}]});
        (header, p)
    }

    fn with_square() -> SceneData {
        let mut s = parse_boxes(&boxes()).unwrap();
        let (h, p) = folded();
        s.add_meshes(&parse_meshes(&h, &p).unwrap()).unwrap();
        s
    }

    #[test]
    fn boxes_place_every_instance_before_any_mesh() {
        let s = parse_boxes(&boxes()).unwrap();
        assert_eq!((s.loaded(), s.triangles(), s.faces.len()), ((0, 2), 0, 0));
        assert!(s.instances.iter().all(|i| i.face_base.is_none()));
        // the box of the cube, moved -20 along y; the mirrored square's box lies at negative x
        assert_eq!(s.instances[3].bbox, Some((Vec3::new(0.0, -20.0, 0.0), Vec3::new(2.0, -18.0, 2.0))));
        assert_eq!(s.instances[2].bbox, Some((Vec3::new(-2.0, 0.0, 5.0), Vec3::new(0.0, 2.0, 7.0))));
        assert_eq!(s.bbox, Some((Vec3::new(-2.0, -20.0, 0.0), Vec3::new(12.0, 2.0, 7.0))));
        assert_eq!(s.total_instance_faces, 3 * 2 + 6);
        assert_eq!(s.instances.iter().map(InstanceInfo::mirrored).collect::<Vec<_>>(), [false, false, true, false]);
    }

    #[test]
    fn a_batch_splits_vertices_per_face_and_numbers_faces_in_arrival_order() {
        let (h, p) = folded();
        let batch = parse_meshes(&h, &p).unwrap();
        // 4 points, 2 shared by both faces -> 6 vertices, stored once for all three instances
        let m = &batch.meshes[0].1;
        assert_eq!((batch.meshes[0].0, m.vertices.len(), m.closed), (0, 6, true));
        // every vertex keeps its mesh-local face
        assert!(m.indices[..3].iter().all(|&i| m.vertices[i as usize].face == 0));
        assert!(m.indices[3..].iter().all(|&i| m.vertices[i as usize].face == 1));

        let mut s = parse_boxes(&boxes()).unwrap();
        assert_eq!(s.add_meshes(&batch).unwrap(), [0, 1, 2]);
        assert_eq!((s.loaded(), s.triangles()), ((1, 2), 6));
        // global ids: instance i owns ids face_base + 1 ..= face_base + 2; the cube has none yet
        assert_eq!(s.instances.iter().map(|i| i.face_base).collect::<Vec<_>>(), [Some(0), Some(2), Some(4), None]);
        assert_eq!((s.faces[3].instance, s.faces[3].face), (1, 1));
        // colours follow the global face
        let (red, green, blue, white) = ([255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255; 4]);
        assert_eq!(s.face_colors, [red, red, green, blue, white, white]);
        // the same mesh again is refused
        assert!(s.add_meshes(&batch).is_err());
    }

    #[test]
    fn arrived_meshes_tighten_their_boxes() {
        let s = with_square();
        // FreeCAD's box of the square was 2 wide; the placed points span 1
        assert_eq!(s.instances[1].bbox, Some((Vec3::new(10.0, 0.0, 0.0), Vec3::new(11.0, 1.0, 1.0))));
        assert_eq!(s.instances[2].bbox, Some((Vec3::new(-1.0, 0.0, 5.0), Vec3::new(0.0, 1.0, 6.0))));
        // the cube still counts with its box
        assert_eq!(s.bbox, Some((Vec3::new(-1.0, -20.0, 0.0), Vec3::new(11.0, 1.0, 6.0))));
        let mut s = s;
        assert_eq!(s.drop_meshes(&[1]), [3]);
        assert_eq!(s.bbox, Some((Vec3::new(-1.0, 0.0, 0.0), Vec3::new(11.0, 1.0, 6.0))));
    }

    #[test]
    fn draws_the_fold_and_the_outline_only() {
        let (h, p) = folded();
        let batch = parse_meshes(&h, &p).unwrap();
        // once per mesh: the shared edge (90 degrees) plus 4 outline edges
        assert_eq!(batch.meshes[0].1.edges.len(), 5 * 2);
    }

    /// Two triangles of a square meeting 10 degrees out of plane, as two faces; with `shared`
    /// false each face repeats the two points on the fold, as a face meshed on its own does.
    fn shallow(shared: bool, curved: &[u32]) -> Mesh {
        let z = 10f32.to_radians().tan();
        let (pts, tris): (Vec<f32>, Vec<u32>) = if shared {
            (vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, -1.0, z], vec![0, 1, 2, 0, 3, 1])
        } else {
            (vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, -1.0, z, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0], vec![0, 1, 2, 4, 3, 5])
        };
        let p = payload(&pts, &tris, &[0, 1]);
        let h = json!({"meshes": [{"index": 0, "points": pts.len() / 3, "triangles": 2, "faces": 2, "brep_edges": false,
                                   "curved_faces": curved, "closed": false, "edge_points": 0, "edge_segments": 0,
                                   "offset": 0, "size": p.len()}]});
        parse_meshes(&h, &p).unwrap().meshes.remove(0).1
    }

    #[test]
    fn a_shallow_seam_between_flat_faces_is_not_drawn() {
        // only the 4 outline edges: the facets of a sewn mesh read as one surface
        assert_eq!(shallow(true, &[]).edges.len(), 4 * 2);
    }

    #[test]
    fn a_curved_face_keeps_its_border_even_where_it_meets_smoothly() {
        // a fillet or a cylinder's rim: the shallow seam is drawn because face 1 is curved
        assert_eq!(shallow(true, &[1]).edges.len(), 5 * 2);
    }

    #[test]
    fn points_repeated_per_face_are_matched_by_position() {
        // without matching, the fold would count as two outline edges and be drawn twice
        assert_eq!(shallow(false, &[]).edges.len(), 4 * 2);
        assert_eq!(shallow(false, &[1]).edges.len(), 5 * 2);
    }

    /// A crease along the x axis in two halves, four faces: on the left the faces fold by
    /// `left` degrees, on the right by `right`.
    fn two_part_crease(left: f32, right: f32) -> Mesh {
        let (zl, zr) = (left.to_radians().tan(), right.to_radians().tan());
        #[rustfmt::skip]
        let pts = [
            0.0, 0.0, 0.0,  1.0, 0.0, 0.0,  2.0, 0.0, 0.0,   // the crease: 0 1 2
            0.5, 1.0, 0.0,  1.5, 1.0, 0.0,                   // flat side: 3 4
            0.5, -1.0, zl,  1.5, -1.0, zr,                   // folded side: 5 6
        ];
        let tris = [0, 1, 3, 0, 5, 1, 1, 2, 4, 1, 6, 2];
        let p = payload(&pts, &tris, &[0, 1, 2, 3]);
        let h = json!({"meshes": [{"index": 0, "points": 7, "triangles": 4, "faces": 4, "brep_edges": false,
                                   "closed": false, "edge_points": 0, "edge_segments": 0, "offset": 0, "size": p.len()}]});
        parse_meshes(&h, &p).unwrap().meshes.remove(0).1
    }

    #[test]
    fn a_gentle_fold_is_drawn_only_where_it_continues_a_sharp_one() {
        // 8 outline edges, plus the sharp left half and the 30-degree right half that carries it on
        assert_eq!(two_part_crease(80.0, 30.0).edges.len(), 10 * 2);
        // a 30-degree fold on its own, as on a coarse cable, is not drawn
        assert_eq!(two_part_crease(10.0, 30.0).edges.len(), 8 * 2);
    }

    #[test]
    fn shading_is_smooth_across_a_gentle_fold_and_sharp_across_a_steep_one() {
        let normals_at_origin = |m: &Mesh| -> Vec<Vec3> {
            m.vertices.iter().filter(|v| v.pos == [0.0, 0.0, 0.0]).map(|v| Vec3::from(v.normal)).collect()
        };
        let gentle = normals_at_origin(&shallow(true, &[]));
        assert_eq!(gentle.len(), 2);
        assert!(gentle[0].dot(gentle[1]) > 0.9999, "{gentle:?}");
        let steep = normals_at_origin(&two_part_crease(80.0, 30.0));
        assert!(steep[0].dot(steep[1]) < 0.5, "{steep:?}");
    }

    #[test]
    fn rejects_data_that_does_not_match() {
        let (mut h, p) = folded();
        h["meshes"][0]["points"] = json!(5);
        assert!(parse_meshes(&h, &p).is_err());
        let mut b = boxes();
        b["instances"][0]["mesh"] = json!(2);
        assert!(parse_boxes(&b).is_err());
        let mut s = parse_boxes(&boxes()).unwrap();
        let (mut h, p) = folded();
        h["meshes"][0]["index"] = json!(7);
        assert!(s.add_meshes(&parse_meshes(&h, &p).unwrap()).is_err());
    }
}
