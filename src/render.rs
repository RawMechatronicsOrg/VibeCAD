//! The 3D view: draws the scene into offscreen textures with wgpu and hands the colour texture
//! to egui as an image. The view is drawn with 4x MSAA (when the GPU has it) and resolved into
//! that texture. Face ids for picking are drawn in a separate single-sample pass, only on a
//! click, from the same camera and the same culled instances as the frame on screen.
//!
//! Each mesh is uploaded once, in chunks as the worker's batches arrive. Until its mesh is
//! there an instance is drawn as a translucent box. Every frame, the instances whose box is in
//! view are packed into an instance buffer (matrix, ids), grouped by mesh, and each group is one
//! instanced draw. Closed solids are drawn with back faces culled; everything else from both
//! sides.
//!
//! Scans are kept apart from the scene, which is replaced on every edit: each is uploaded once,
//! a buffer group per frame, and drawn chunk by chunk as discs, as many points of a chunk as its
//! size on screen needs (see `cloud::level_of_detail`), within a budget of points per frame.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use glam::{DMat4, Mat3, Mat4, Vec3};
use wgpu::util::DeviceExt as _;

use crate::camera::Frustum;
use crate::cloud::{self, Cloud};
use crate::scene::{DEFAULT_COLOR, InstanceInfo, MeshBatch, SceneData, Vertex};

// egui shows a native texture's bytes as they are, so the view is drawn without sRGB encoding
// and a captured frame matches the screen.
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const MSAA: u32 = 4;
/// Muted pastels for objects that only have FreeCAD's default grey.
const PALETTE: [[u8; 3]; 8] = [
    [158, 188, 222],
    [228, 178, 156],
    [166, 206, 166],
    [230, 210, 156],
    [196, 178, 222],
    [150, 206, 204],
    [226, 172, 188],
    [214, 190, 214],
];

const SHADER: &str = r#"
struct U {
    view_proj: mat4x4<f32>,
    light: vec4<f32>,       // xyz: direction the camera looks in
    sel: vec4<u32>,         // x: selected face id, y: selected instance id
    view: vec4<f32>,        // x, y: viewport in pixels, z: edge width in pixels
    opts: vec4<u32>,        // y: palette colours on
    grid: vec4<f32>,        // ground grid: half size, major step, minor step (mm)
    bg: vec4<f32>,          // background colour at the bottom of the view
};
@group(0) @binding(0) var<uniform> u: U;
// One RGBA8 colour per global face id - 1.
@group(0) @binding(1) var<storage, read> face_colors: array<u32>;
// Edge line list of all meshes, xyz per point, in mesh coordinates.
@group(0) @binding(2) var<storage, read> edge_points: array<f32>;

struct VIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) face: u32,         // 0-based face of the mesh
};
struct Inst {
    @location(3) m0: vec4<f32>,     // model matrix, by columns
    @location(4) m1: vec4<f32>,
    @location(5) m2: vec4<f32>,
    @location(6) m3: vec4<f32>,
    @location(7) n0: vec3<f32>,     // normal matrix, by columns
    @location(8) n1: vec3<f32>,
    @location(9) n2: vec3<f32>,
    // x: 1-based instance id, y: faces before this instance, z: palette colour (RGBA8, 0: none),
    // w: the object's own colour (RGBA8), for its box while the mesh is not here
    @location(10) ids: vec4<u32>,
};
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) face: u32,
    @location(3) @interpolate(flat) inst: u32,
};

fn model(i: Inst) -> mat4x4<f32> {
    return mat4x4<f32>(i.m0, i.m1, i.m2, i.m3);
}

@vertex
fn vs_face(v: VIn, i: Inst) -> VOut {
    var o: VOut;
    let world = model(i) * vec4<f32>(v.pos, 1.0);
    o.clip = u.view_proj * world;
    o.normal = mat3x3<f32>(i.n0, i.n1, i.n2) * v.normal;
    let face = i.ids.y + v.face;
    o.color = unpack4x8unorm(face_colors[face]);
    if (u.opts.y != 0u && i.ids.z != 0u) {
        o.color = unpack4x8unorm(i.ids.z);
    }
    o.face = face + 1u;
    o.inst = i.ids.x;
    return o;
}

@fragment
fn fs_face(v: VOut) -> @location(0) vec4<f32> {
    // A vertex used only by degenerate triangles has no normal; do not let it turn into NaN.
    var n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(v.normal), dot(v.normal, v.normal) > 1e-20);
    let to_eye = -u.light.xyz;
    // Two-sided: open shells and flipped faces still read as surfaces.
    n = select(n, -n, dot(n, to_eye) < 0.0);
    var base = v.color.rgb;
    if (v.inst == u.sel.y && v.inst != 0u) {
        base = mix(base, vec3<f32>(0.35, 0.85, 0.45), 0.35);
    }
    if (v.face == u.sel.x && v.face != 0u) {
        base = mix(base, vec3<f32>(0.20, 0.75, 1.00), 0.65);
    }
    return vec4<f32>(shade(base, n, to_eye), 1.0);
}

// Plastic, lit in linear light and written back as sRGB: a soft sky and ground, a headlight and
// a key light from above-front, and a soft highlight. Lighting the sRGB values directly (as
// before) darkens and greys every face turned away from the light; in linear light the colour
// keeps its strength and the highlight reads as a smooth, solid material.
fn shade(srgb: vec3<f32>, n: vec3<f32>, to_eye: vec3<f32>) -> vec3<f32> {
    let albedo = pow(srgb, vec3<f32>(2.2));
    let key = normalize(vec3<f32>(0.35, -0.45, 0.82));
    let head = max(dot(n, to_eye), 0.0);
    let diffuse = max(dot(n, key), 0.0);
    let ambient = mix(vec3<f32>(0.13, 0.125, 0.12), vec3<f32>(0.30, 0.31, 0.33), n.z * 0.5 + 0.5);
    let light = ambient + vec3<f32>(0.42 * head + 0.55 * diffuse);
    let spec = 0.22 * pow(max(dot(n, normalize(key + to_eye)), 0.0), 60.0) + 0.10 * pow(head, 80.0);
    let fresnel = 0.05 * pow(1.0 - head, 4.0);
    let c = albedo * light + vec3<f32>(spec + fresnel);
    // A soft shoulder instead of a hard clip, so highlights on light parts do not burn out.
    let mapped = c / (1.0 + 0.2 * c) * 1.12;
    return pow(clamp(mapped, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
}

// The face id pass: no lighting, one integer per pixel.
@fragment
fn fs_id(v: VOut) -> @location(0) u32 {
    return v.face;
}

// Instances whose mesh has not arrived yet: their shape's box, translucent, in their colour.
struct BIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
};
struct BOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) inst: u32,
};

@vertex
fn vs_box(v: BIn, i: Inst) -> BOut {
    var o: BOut;
    o.clip = u.view_proj * (model(i) * vec4<f32>(v.pos, 1.0));
    o.normal = mat3x3<f32>(i.n0, i.n1, i.n2) * v.normal;
    o.color = unpack4x8unorm(i.ids.w);
    if (u.opts.y != 0u && i.ids.z != 0u) {
        o.color = unpack4x8unorm(i.ids.z);
    }
    o.inst = i.ids.x;
    return o;
}

@fragment
fn fs_box(v: BOut) -> @location(0) vec4<f32> {
    let n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(v.normal), dot(v.normal, v.normal) > 1e-20);
    var base = v.color.rgb;
    if (v.inst == u.sel.y && v.inst != 0u) {
        base = mix(base, vec3<f32>(0.35, 0.85, 0.45), 0.5);
    }
    return vec4<f32>(base * (0.55 + 0.45 * abs(dot(n, u.light.xyz))), 0.35);
}

// The background: a vertical gradient from near white at the top to `bg` at the bottom.
struct BgOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) t: f32,
};

@vertex
fn vs_bg(@builtin(vertex_index) i: u32) -> BgOut {
    var corners = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: BgOut;
    o.clip = vec4<f32>(corners[i], 1.0, 1.0);
    o.t = corners[i].y * 0.5 + 0.5;
    return o;
}

@fragment
fn fs_bg(v: BgOut) -> @location(0) vec4<f32> {
    let top = mix(u.bg.rgb, vec3<f32>(1.0), 0.85);
    return vec4<f32>(mix(u.bg.rgb, top, clamp(v.t, 0.0, 1.0)), 1.0);
}

// The ground grid: a square on z = 0 around the origin, drawn per pixel. Minor and major lines,
// the outline of the square, and the X (red) and Y (green) axes across it.
struct GOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) p: vec2<f32>,
};

@vertex
fn vs_grid(@builtin(vertex_index) i: u32) -> GOut {
    var corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
                                      vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    // A little past the outline, so the outline's soft edge is whole.
    let p = corners[i] * u.grid.x * 1.01;
    var o: GOut;
    o.clip = u.view_proj * vec4<f32>(p, 0.0, 1.0);
    o.p = p;
    return o;
}

// How much of a line of the `step` lattice covers this pixel, faded out where cells get smaller
// than a few pixels so the far grid does not turn into a grey blur.
fn lattice(p: vec2<f32>, step: f32) -> f32 {
    let c = p / step;
    let w = max(fwidth(c), vec2<f32>(1e-6));
    let line = vec2<f32>(1.0) - min(abs(fract(c - 0.5) - 0.5) / w, vec2<f32>(1.0));
    let fade = vec2<f32>(1.0) - smoothstep(vec2<f32>(0.12), vec2<f32>(0.3), w);
    return max(line.x * fade.x, line.y * fade.y);
}

@fragment
fn fs_grid(v: GOut) -> @location(0) vec4<f32> {
    let half = u.grid.x;
    let fw = max(fwidth(v.p), vec2<f32>(1e-6));
    // Pixels from the outline, positive inside.
    let inside = min((half - abs(v.p.x)) / fw.x, (half - abs(v.p.y)) / fw.y);
    if (inside < -1.0) {
        discard;
    }
    let outline = 1.0 - clamp(abs(inside), 0.0, 1.0);
    let minor = lattice(v.p, u.grid.z) * step(0.0, inside);
    let major = lattice(v.p, u.grid.y) * step(0.0, inside);
    let x_axis = (1.0 - clamp(abs(v.p.y) / fw.y - 0.5, 0.0, 1.0)) * step(0.0, inside);
    let y_axis = (1.0 - clamp(abs(v.p.x) / fw.x - 0.5, 0.0, 1.0)) * step(0.0, inside);
    var a = max(max(0.16 * minor, 0.34 * major), 0.62 * outline);
    var c = vec3<f32>(0.32, 0.33, 0.35);
    c = mix(c, vec3<f32>(0.90, 0.25, 0.22), x_axis);
    a = max(a, 0.85 * x_axis);
    c = mix(c, vec3<f32>(0.25, 0.68, 0.30), y_axis * (1.0 - x_axis));
    a = max(a, 0.85 * y_axis);
    return vec4<f32>(c, a);
}

struct EOut {
    @builtin(position) clip: vec4<f32>,
    // Signed distance from the centre line, in pixels.
    @location(0) @interpolate(linear) across: f32,
    @location(1) @interpolate(flat) inst: u32,
};

fn edge_point(i: u32) -> vec3<f32> {
    return vec3<f32>(edge_points[3u * i], edge_points[3u * i + 1u], edge_points[3u * i + 2u]);
}

// A clip-space point moved toward the eye by `px` pixels' worth of world at its depth, in depth
// only: it stays where it is on screen. Row 1 of view_proj is the projection's y scale times the
// view's unit up vector, so its length is that scale, for ortho and perspective alike.
fn toward_eye(c: vec4<f32>, px: f32) -> vec4<f32> {
    let y_scale = length(vec3<f32>(u.view_proj[0][1], u.view_proj[1][1], u.view_proj[2][1]));
    let t = px * c.w / (y_scale * u.view.y * 0.5);
    let d = u.view_proj * vec4<f32>(-u.light.xyz, 0.0);
    let z = (c.z + t * d.z) / (c.w + t * d.w);
    return vec4<f32>(c.xy, max(z, 0.0) * c.w, c.w);
}

// Each segment of the line list becomes a quad facing the screen: six vertices, two triangles.
// The draw's vertex range is the mesh's edge points times three, so vertex_index / 6 is the
// segment and vertex_index % 6 the corner.
@vertex
fn vs_edge(@builtin(vertex_index) vi: u32, i: Inst) -> EOut {
    var o: EOut;
    o.inst = i.ids.x;
    o.across = 0.0;
    let seg = vi / 6u;
    let k = vi % 6u;
    var a = u.view_proj * (model(i) * vec4<f32>(edge_point(2u * seg), 1.0));
    var b = u.view_proj * (model(i) * vec4<f32>(edge_point(2u * seg + 1u), 1.0));
    // Clip against the near plane (z = 0 in wgpu's clip space) before dividing by w.
    if (a.z < 0.0 && b.z < 0.0) {
        o.clip = vec4<f32>(0.0, 0.0, -1.0, 1.0);
        return o;
    }
    if (a.z < 0.0) {
        a = mix(a, b, a.z / (a.z - b.z));
    } else if (b.z < 0.0) {
        b = mix(b, a, b.z / (b.z - a.z));
    }
    let half_view = u.view.xy * 0.5;
    let half_width = u.view.z * 0.5;
    // One pixel of fringe for the falloff; the ends reach half the width past the points so that
    // segments of a polyline overlap at corners instead of leaving notches.
    let reach = half_width + 1.0;
    // The quad lies at the depth of its centre line, so a face turned toward the eye beside the
    // line comes in front of its inner half and eats it, the more the steeper the face: a wide
    // line looked uneven. Pull it toward the eye by twice its reach, enough for faces up to
    // about 60 degrees from facing the eye; faces closer to edge-on hide less of it anyway.
    a = toward_eye(a, 2.0 * reach);
    b = toward_eye(b, 2.0 * reach);
    let d = b.xy / b.w * half_view - a.xy / a.w * half_view;
    let len = length(d);
    let dir = select(vec2<f32>(1.0, 0.0), d / max(len, 1e-6), len > 1e-6);
    let normal = vec2<f32>(-dir.y, dir.x);
    // Corners: (a, -) (b, -) (b, +) and (a, -) (b, +) (a, +).
    let at_b = k == 1u || k == 2u || k == 4u;
    let side = select(-1.0, 1.0, k == 2u || k == 4u || k == 5u);
    var p = select(a, b, at_b);
    let off = normal * side * reach + dir * select(-half_width, half_width, at_b);
    p = vec4<f32>(p.xy + off / half_view * p.w, p.zw);
    o.clip = p;
    o.across = side * reach;
    return o;
}

@fragment
fn fs_edge(v: EOut) -> @location(0) vec4<f32> {
    let alpha = clamp(u.view.z * 0.5 + 0.5 - abs(v.across), 0.0, 1.0);
    if (v.inst == u.sel.y && v.inst != 0u) {
        return vec4<f32>(0.10, 0.55, 0.20, alpha);
    }
    return vec4<f32>(0.06, 0.06, 0.08, alpha);
}

// Scans: every point a disc facing the screen, six vertices per point. A draw is one chunk: its
// vertex range is its points times six within its group's buffer, its instance the chunk's
// index, which gives the width its level of detail chose.
struct CloudU {
    // Scan coordinates to clip space, composed in f64: a scan far from the origin (a site
    // grid, a georeferenced survey) keeps the steps between its points.
    mvp: mat4x4<f32>,
    // The inverse transpose of the model matrix's 3x3, in the first three columns.
    normal: mat4x4<f32>,
    // For points without colours of their own.
    color: vec4<f32>,
    // x: the points have colours, y: the scan is selected, z: how points are coloured (0 their
    // own colours, 1 one colour, 2 by height, 3 by normal), w: lit
    flags: vec4<u32>,
    // x, y: the heights the colour ramp runs between, z: the points' width, times
    ramp: vec4<f32>,
    // Row 2 of the scan's model matrix: a point's height in the document.
    zrow: vec4<f32>,
};
struct CPoint {
    x: f32,
    y: f32,
    z: f32,
    color: u32,
    normal: u32,    // octahedral, two snorm16
};
@group(1) @binding(0) var<storage, read> cloud_points: array<CPoint>;
@group(1) @binding(1) var<uniform> cu: CloudU;
// Width in pixels per chunk, this frame.
@group(1) @binding(2) var<storage, read> chunk_sizes: array<f32>;
// Two bits a point, sixteen a word: 1 selected, 2 deleted (see cloud::marks).
@group(1) @binding(3) var<storage, read> point_marks: array<u32>;

struct POut {
    @builtin(position) clip: vec4<f32>,
    @location(0) corner: vec2<f32>,
    @location(1) color: vec3<f32>,
    @location(2) normal: vec3<f32>,
    @location(3) @interpolate(flat) lit: u32,
};

fn oct_decode(e: u32) -> vec3<f32> {
    let f = unpack2x16snorm(e);
    var n = vec3<f32>(f, 1.0 - abs(f.x) - abs(f.y));
    let t = max(-n.z, 0.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return normalize(n);
}

// Blue, cyan, green, yellow, red from 0 to 1.
fn height_ramp(t: f32) -> vec3<f32> {
    let stops = array<vec3<f32>, 5>(vec3<f32>(0.16, 0.32, 0.86), vec3<f32>(0.12, 0.74, 0.84), vec3<f32>(0.32, 0.78, 0.32),
                                    vec3<f32>(0.95, 0.84, 0.22), vec3<f32>(0.90, 0.26, 0.16));
    let x = clamp(t, 0.0, 1.0) * 4.0;
    let i = min(u32(x), 3u);
    return mix(stops[i], stops[i + 1u], x - f32(i));
}

@vertex
fn vs_point(@builtin(vertex_index) vi: u32, @builtin(instance_index) chunk: u32) -> POut {
    var corners = array<vec2<f32>, 6>(vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
                                      vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    let i = vi / 6u;
    let p = cloud_points[i];
    let c = corners[vi % 6u];
    var o: POut;
    let mark = (point_marks[i / 16u] >> ((i % 16u) * 2u)) & 3u;
    if ((mark & 2u) != 0u) {
        o.clip = vec4<f32>(0.0, 0.0, -1.0, 1.0); // deleted: outside the view
        return o;
    }
    let pos = vec4<f32>(p.x, p.y, p.z, 1.0);
    let clip = cu.mvp * pos;
    // `size` pixels across: half of it in NDC units is size / viewport.
    o.clip = vec4<f32>(clip.xy + c * chunk_sizes[chunk] * cu.ramp.z / u.view.xy * clip.w, clip.zw);
    o.corner = c;
    let n = mat3x3<f32>(cu.normal[0].xyz, cu.normal[1].xyz, cu.normal[2].xyz) * oct_decode(p.normal);
    switch cu.flags.z {
        case 0u: { o.color = select(cu.color.rgb, unpack4x8unorm(p.color).rgb, cu.flags.x != 0u); }
        case 2u: { o.color = height_ramp((dot(cu.zrow, pos) - cu.ramp.x) / max(cu.ramp.y - cu.ramp.x, 1e-6)); }
        // Fitted normals face either way: their direction, not their sense.
        case 3u: { o.color = abs(normalize(n)) * 0.8 + 0.15; }
        default: { o.color = cu.color.rgb; }
    }
    if (cu.flags.y != 0u) {
        o.color = mix(o.color, vec3<f32>(0.35, 0.85, 0.45), 0.35);
    }
    if ((mark & 1u) != 0u) {
        o.color = vec3<f32>(1.0, 0.55, 0.08);
    }
    o.normal = n;
    o.lit = cu.flags.w;
    return o;
}

@fragment
fn fs_point(v: POut) -> @location(0) vec4<f32> {
    if (dot(v.corner, v.corner) > 1.0) {
        discard;
    }
    if (v.lit == 0u) {
        return vec4<f32>(v.color, 1.0);
    }
    var n = select(vec3<f32>(0.0, 0.0, 1.0), normalize(v.normal), dot(v.normal, v.normal) > 1e-20);
    let to_eye = -u.light.xyz;
    // A fitted normal may face either way.
    n = select(n, -n, dot(n, to_eye) < 0.0);
    return vec4<f32>(shade(v.color, n, to_eye), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    light: [f32; 4],
    sel: [u32; 4],
    view: [f32; 4],
    opts: [u32; 4],
    grid: [f32; 4],
    bg: [f32; 4],
}

/// The square ground grid on z = 0 around the origin, sized to the scene: steps of 1, 2, 2.5 or
/// 5 times a power of ten (25, 50, 75 … mm for a part a few hundred millimetres across), ten
/// to twenty major cells, five minor ones in each.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundGrid {
    /// Half the side of the square, a whole number of major steps.
    pub half: f32,
    pub major: f32,
    pub minor: f32,
}

impl GroundGrid {
    /// For a scene with these bounds (document coordinates).
    pub fn around(lo: Vec3, hi: Vec3) -> GroundGrid {
        let reach = [lo.x, hi.x, lo.y, hi.y].iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1.0) * 1.15;
        let major = nice_step(2.0 * reach / 14.0);
        GroundGrid { half: (reach / major).ceil().max(2.0) * major, major, minor: major / 5.0 }
    }

    /// The same square with finer lines as the view zooms in: a major cell of about 90 pixels,
    /// never coarser than the scene's own step. `px_per_mm` is the view's scale at its centre.
    pub fn for_scale(self, px_per_mm: f32) -> GroundGrid {
        if !(px_per_mm > 0.0) {
            return self;
        }
        let major = nice_step(90.0 / px_per_mm).min(self.major).max(self.major * 1e-4);
        GroundGrid { major, minor: major / 5.0, ..self }
    }
}

/// The step of 1, 2, 2.5 or 5 times a power of ten nearest to `raw`.
fn nice_step(raw: f32) -> f32 {
    let raw = raw.max(1e-6);
    let base = 10f32.powf(raw.log10().floor());
    [1.0f32, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * base)
        .min_by(|a, b| (a.ln() - raw.ln()).abs().total_cmp(&(b.ln() - raw.ln()).abs()))
        .unwrap_or(base)
}

/// Per-instance vertex data, `Inst` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuInstance {
    model: [[f32; 4]; 4],
    normal: [[f32; 3]; 3],
    /// 1-based instance id, the faces of the instances before it, the palette colour (RGBA8,
    /// 0 when the object has colours of its own), padding.
    ids: [u32; 4],
}

/// How an instance's triangles are drawn; also the order of the draws.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Sides {
    /// Open shells, faces that may face either way: no culling.
    Both,
    /// Closed solids, outward and counter-clockwise: back faces culled.
    Front,
    /// The same, placed by a mirroring matrix, which makes them clockwise.
    FrontMirrored,
}

struct Targets {
    size: (u32, u32),
    /// Single-sample: what egui shows and `read_color` copies.
    color_tex: wgpu::Texture,
    color_view: wgpu::TextureView,
    /// Multisampled colour, resolved into `color_view`; `None` without MSAA.
    msaa_view: Option<wgpu::TextureView>,
    depth_view: wgpu::TextureView,
    id_tex: wgpu::Texture,
    id_view: wgpu::TextureView,
    id_depth_view: wgpu::TextureView,
}

/// One batch of meshes on the GPU: their vertices, indices and edge points, and the bindings
/// that point the shader at those edge points.
struct Chunk {
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    ebuf: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

/// Where a mesh sits in its chunk.
struct GpuMesh {
    base_vertex: i32,
    indices: Range<u32>,
    edges: Range<u32>,
}

/// An instance whose mesh is here, as culling sees it.
struct Item {
    lo: Vec3,
    hi: Vec3,
    mesh: u32,
    chunk: u32,
    sides: Sides,
    gpu: GpuInstance,
}

/// An instance still waiting for its mesh: its box.
struct BoxItem {
    lo: Vec3,
    hi: Vec3,
    gpu: GpuInstance,
}

/// Visible instances of one mesh, next to each other in the instance buffer.
struct Run {
    chunk: u32,
    mesh: u32,
    sides: Sides,
    instances: Range<u32>,
}

struct GpuScene {
    chunks: Vec<Chunk>,
    /// By the worker's mesh index; `None` until the mesh arrives.
    meshes: Vec<Option<GpuMesh>>,
    /// One RGBA8 per global face, written as faces get numbered.
    colors: wgpu::Buffer,
    color_capacity: usize,
    colors_written: usize,
    /// For the boxes, which read no edges.
    no_edges: wgpu::Buffer,
    box_bind: wgpu::BindGroup,
    inst_buf: wgpu::Buffer,
    /// Sorted by how they are drawn, then by chunk and mesh, so runs are long.
    items: Vec<Item>,
    /// By instance; `None` once its mesh is here (or will never come).
    boxes: Vec<Option<BoxItem>>,
    visible: Vec<GpuInstance>,
    runs: Vec<Run>,
    /// The visible boxes, in the instance buffer after the runs.
    box_range: Range<u32>,
}

/// What the last `set_scene` and `render` cost.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub gpu_bytes: u64,
    pub upload_ms: f64,
    pub drawn_instances: u32,
    pub drawn_triangles: u64,
    pub drawn_points: u64,
}

/// Points drawn per frame at most, over all scans; `VIBECAD_POINT_BUDGET` sets another.
const POINT_BUDGET: u64 = 12_000_000;
/// About one point per this many pixels across, where a chunk has enough points.
const POINT_SPACING: f32 = 1.2;
/// The smallest disc, in pixels.
const POINT_MIN_SIZE: f32 = 2.0;
/// Bytes of one buffer of scan points at most: one is uploaded per frame.
const POINT_GROUP_BYTES: u64 = 128 << 20;
/// Points without colours of their own: a light grey with a little blue, as scans are shown.
const SCAN_COLOR: [f32; 4] = [0.74, 0.76, 0.80, 1.0];

/// `CloudU` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CloudUniforms {
    mvp: [[f32; 4]; 4],
    normal: [[f32; 4]; 4],
    color: [f32; 4],
    flags: [u32; 4],
    ramp: [f32; 4],
    zrow: [f32; 4],
}

/// How points take their colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum ScanColor {
    /// The file's colours, else one colour.
    #[default]
    File,
    Single,
    /// A ramp from the lowest point of the scan to its highest.
    Height,
    /// The normal's direction: faces turned alike look alike.
    Normal,
}

impl ScanColor {
    pub const ALL: [ScanColor; 4] = [ScanColor::File, ScanColor::Single, ScanColor::Height, ScanColor::Normal];

    pub fn name(self) -> &'static str {
        match self {
            ScanColor::File => "File colours",
            ScanColor::Single => "One colour",
            ScanColor::Height => "Height",
            ScanColor::Normal => "Normal",
        }
    }
}

/// How scans are drawn: for all of them, kept with the settings.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanDisplay {
    pub color: ScanColor,
    /// Lit as a surface by their normals, or flat.
    pub lit: bool,
    /// The points' width, times.
    pub size: f32,
}

impl Default for ScanDisplay {
    fn default() -> Self {
        ScanDisplay { color: ScanColor::File, lit: true, size: 1.0 }
    }
}

/// A scan to draw this frame.
pub struct CloudDraw {
    /// As given to `add_cloud`.
    pub key: u64,
    /// Scan coordinates (around the scan's origin) to document coordinates.
    pub model: DMat4,
    pub selected: bool,
    /// The scan's lowest and highest point, in document z: the height ramp's ends.
    pub heights: (f32, f32),
}

/// Points of consecutive chunks in one storage buffer.
struct CloudGroup {
    bind: wgpu::BindGroup,
    _points: wgpu::Buffer,
    /// Its points' marks, `marks[first..]` of the scan's.
    marks: wgpu::Buffer,
    first: usize,
}

struct GpuChunk {
    group: u32,
    /// The chunk's first point in its group's buffer.
    start: u32,
    count: u32,
    lo: Vec3,
    hi: Vec3,
}

struct GpuCloud {
    /// Until every group is uploaded.
    source: Option<Arc<Cloud>>,
    has_color: bool,
    groups: Vec<CloudGroup>,
    chunks: Vec<GpuChunk>,
    /// Points per group (a whole number of chunks).
    group_points: usize,
    uniform: wgpu::Buffer,
    sizes: wgpu::Buffer,
    sizes_cpu: Vec<f32>,
    /// This frame: (group, chunk, points to draw).
    draws: Vec<(u32, u32, u32)>,
    /// The marks of all its points, two bits each (see cloud::marks).
    marks: Vec<u32>,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    egui_renderer: Arc<egui::epaint::mutex::RwLock<egui_wgpu::Renderer>>,
    /// Indexed by `Sides`.
    face_pipelines: [wgpu::RenderPipeline; 3],
    /// The same culling, writing face ids.
    id_pipelines: [wgpu::RenderPipeline; 3],
    edge_pipeline: wgpu::RenderPipeline,
    box_pipeline: wgpu::RenderPipeline,
    point_pipeline: wgpu::RenderPipeline,
    cloud_layout: wgpu::BindGroupLayout,
    /// Group 0 for draws that use the uniforms only (background, grid, scans).
    base_bind: wgpu::BindGroup,
    clouds: HashMap<u64, GpuCloud>,
    background_pipeline: wgpu::RenderPipeline,
    grid_pipeline: wgpu::RenderPipeline,
    /// A unit cube, `[0, 1]³`, for the boxes.
    cube: wgpu::Buffer,
    /// Samples per pixel of the colour pass: `MSAA`, or 1 where the GPU lacks it.
    pub samples: u32,
    bind_layout: wgpu::BindGroupLayout,
    ubuf: wgpu::Buffer,
    targets: Option<Targets>,
    pub texture_id: Option<egui::TextureId>,
    scene: Option<GpuScene>,
    pick_buf: wgpu::Buffer,
    /// Bumped by `set_scene`; picking only trusts a frame drawn from the current scene.
    scene_gen: u64,
    drawn_gen: Option<u64>,
    pub stats: Stats,
    /// Why this GPU cannot draw the view (OpenGL ES-level backends lack what instancing needs).
    unsupported: Option<String>,
}

pub struct Frame {
    pub view_proj: Mat4,
    pub forward: glam::Vec3,
    pub selected_face: u32,
    pub selected_instance: u32,
    pub show_edges: bool,
    /// In pixels.
    pub edge_width: f32,
    /// Colour objects that have only FreeCAD's default grey from `PALETTE`.
    pub palette: bool,
    /// The colour at the bottom of the view; the top is near white.
    pub background: [f64; 3],
    pub grid: Option<GroundGrid>,
    pub clouds: Vec<CloudDraw>,
    pub scan_display: ScanDisplay,
}

impl Renderer {
    pub fn new(rs: &egui_wgpu::RenderState) -> Self {
        let device = rs.device.clone();
        let need = wgpu::DownlevelFlags::VERTEX_STORAGE | wgpu::DownlevelFlags::BASE_VERTEX;
        let have = rs.adapter.get_downlevel_capabilities().flags;
        let unsupported = (!have.contains(need) || device.limits().max_storage_buffers_per_shader_stage < 2).then(|| {
            format!("this GPU backend ({:?}) cannot read storage buffers in vertex shaders; the 3D view needs Vulkan, DX12 or Metal", rs.adapter.get_info().backend)
        });
        // wgpu's default handler panics; a failed draw should not take the window down.
        device.on_uncaptured_error(Arc::new(|e| eprintln!("wgpu error: {e}")));
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vibecad shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let storage = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vibecad bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1),
                storage(2),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vibecad layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let samples = msaa_samples(rs);
        let vertex_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Uint32];
        let inst_attrs = wgpu::vertex_attr_array![
            3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4,
            7 => Float32x3, 8 => Float32x3, 9 => Float32x3, 10 => Uint32x4
        ];
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &inst_attrs,
        };
        let face_buffers = [
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &vertex_attrs,
            }),
            Some(instance_layout.clone()),
        ];
        // Edge points come from a storage buffer; slot 0 stays empty so instances keep slot 1.
        let edge_buffers = [None, Some(instance_layout.clone())];
        let box_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
        let box_buffers = [
            Some(wgpu::VertexBufferLayout { array_stride: 24, step_mode: wgpu::VertexStepMode::Vertex, attributes: &box_attrs }),
            Some(instance_layout),
        ];
        let color_target = [Some(wgpu::ColorTargetState { format: COLOR_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })];
        let edge_target = [Some(wgpu::ColorTargetState {
            format: COLOR_FORMAT,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let id_target = [Some(wgpu::ColorTargetState { format: ID_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })];
        let pipeline = |d: PipelineDesc| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(d.label),
                layout: Some(&layout),
                vertex: wgpu::VertexState { module: &module, entry_point: Some(d.vs), compilation_options: Default::default(), buffers: d.buffers },
                primitive: d.primitive,
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(d.depth_write),
                    depth_compare: Some(d.compare),
                    stencil: Default::default(),
                    bias: d.bias,
                }),
                multisample: wgpu::MultisampleState { count: d.samples, ..Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(d.fs),
                    compilation_options: Default::default(),
                    targets: d.targets,
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let sides = [(None, wgpu::FrontFace::Ccw), (Some(wgpu::Face::Back), wgpu::FrontFace::Ccw), (Some(wgpu::Face::Back), wgpu::FrontFace::Cw)];
        let triangles = |(cull_mode, front_face)| wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode,
            front_face,
            ..Default::default()
        };
        // Faces are pushed back a little so the edges on them win the depth test. Edges come from
        // the triangles themselves, so a small push does: a bigger slope term grows with the size
        // of a pixel and, zoomed out, lets edges behind thin walls show through. The id pass uses
        // the same bias, so nearly coincident faces sort there as they do on screen.
        let face_bias = wgpu::DepthBiasState { constant: 1, slope_scale: 1.0, clamp: 0.0 };
        let face_pipelines = std::array::from_fn(|k| {
            let (label, s) = (["faces, both sides", "faces, front", "faces, front, mirrored"][k], sides[k]);
            pipeline(PipelineDesc {
                label,
                vs: "vs_face",
                fs: "fs_face",
                buffers: &face_buffers,
                targets: &color_target,
                primitive: triangles(s),
                bias: face_bias,
                compare: wgpu::CompareFunction::Less,
                depth_write: true,
                samples,
            })
        });
        let id_pipelines = std::array::from_fn(|k| {
            let (label, s) = (["ids, both sides", "ids, front", "ids, front, mirrored"][k], sides[k]);
            pipeline(PipelineDesc {
                label,
                vs: "vs_face",
                fs: "fs_id",
                buffers: &face_buffers,
                targets: &id_target,
                primitive: triangles(s),
                bias: face_bias,
                compare: wgpu::CompareFunction::Less,
                depth_write: true,
                samples: 1,
            })
        });
        // Edges blend and do not write depth, so the soft rim of one never cuts another.
        let edge_pipeline = pipeline(PipelineDesc {
            label: "edges",
            vs: "vs_edge",
            fs: "fs_edge",
            buffers: &edge_buffers,
            targets: &edge_target,
            primitive: triangles((None, wgpu::FrontFace::Ccw)),
            bias: Default::default(),
            compare: wgpu::CompareFunction::LessEqual,
            depth_write: false,
            samples,
        });
        // Boxes blend over what is already drawn and write no depth, so the meshes that arrived
        // stay solid and a box never hides another.
        let box_pipeline = pipeline(PipelineDesc {
            label: "boxes",
            vs: "vs_box",
            fs: "fs_box",
            buffers: &box_buffers,
            targets: &edge_target,
            // One layer per box: the far sides would double the tint and blur the shapes.
            primitive: triangles((Some(wgpu::Face::Back), wgpu::FrontFace::Ccw)),
            bias: Default::default(),
            compare: wgpu::CompareFunction::Less,
            depth_write: false,
            samples,
        });
        // Background and grid read no vertex buffers: their corners come from the vertex index.
        let background_pipeline = pipeline(PipelineDesc {
            label: "background",
            vs: "vs_bg",
            fs: "fs_bg",
            buffers: &[],
            targets: &color_target,
            primitive: triangles((None, wgpu::FrontFace::Ccw)),
            bias: Default::default(),
            compare: wgpu::CompareFunction::Always,
            depth_write: false,
            samples,
        });
        // The grid blends over the faces and writes no depth; pushed back further than the faces,
        // a face lying on z = 0 (the bottom of a part) covers it instead of flickering with it.
        let grid_pipeline = pipeline(PipelineDesc {
            label: "ground grid",
            vs: "vs_grid",
            fs: "fs_grid",
            buffers: &[],
            targets: &edge_target,
            primitive: triangles((None, wgpu::FrontFace::Ccw)),
            bias: wgpu::DepthBiasState { constant: 8, slope_scale: 3.0, clamp: 0.0 },
            compare: wgpu::CompareFunction::Less,
            depth_write: false,
            samples,
        });
        // Scans: group 0 as for everything else, group 1 the points of one buffer group, their
        // scan's placement and this frame's widths.
        let read_only = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let cloud_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vibecad scan bindings"),
            entries: &[
                read_only(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                read_only(2),
                read_only(3),
            ],
        });
        let point_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vibecad scan layout"),
            bind_group_layouts: &[Some(&bind_layout), Some(&cloud_layout)],
            immediate_size: 0,
        });
        let point_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scan points"),
            layout: Some(&point_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_point"), compilation_options: Default::default(), buffers: &[] },
            primitive: triangles((None, wgpu::FrontFace::Ccw)),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: samples, ..Default::default() },
            fragment: Some(wgpu::FragmentState { module: &module, entry_point: Some("fs_point"), compilation_options: Default::default(), targets: &color_target }),
            multiview_mask: None,
            cache: None,
        });
        let dummy = |label, size| device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage: wgpu::BufferUsages::STORAGE, mapped_at_creation: false });
        let cube = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vibecad box"),
            contents: bytemuck::cast_slice(&unit_cube()),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let base_bind = bind_group(&device, &bind_layout, &ubuf, &dummy("vibecad no colours", 4), &dummy("vibecad no edges", 12));
        let pick_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad pick"),
            size: 256,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Renderer {
            device,
            queue: rs.queue.clone(),
            egui_renderer: rs.renderer.clone(),
            face_pipelines,
            id_pipelines,
            edge_pipeline,
            box_pipeline,
            point_pipeline,
            cloud_layout,
            base_bind,
            clouds: HashMap::new(),
            background_pipeline,
            grid_pipeline,
            cube,
            samples,
            bind_layout,
            ubuf,
            targets: None,
            texture_id: None,
            scene: None,
            pick_buf,
            scene_gen: 0,
            drawn_gen: None,
            stats: Stats::default(),
            unsupported,
        }
    }

    /// A new scene, from its first frame: every instance as a box, no meshes yet.
    pub fn set_layout(&mut self, scene: &SceneData) -> Result<(), String> {
        let started = std::time::Instant::now();
        self.stats = Stats::default();
        self.scene_gen += 1;
        self.scene = None;
        if let Some(why) = &self.unsupported {
            return Err(why.clone());
        }
        let limits = self.device.limits();
        let storage_limit = limits.max_buffer_size.min(limits.max_storage_buffer_binding_size as u64);
        let color_capacity = scene.total_instance_faces.max(1);
        check_size("face colour", (color_capacity * 4) as u64, storage_limit)?;
        let inst_bytes = (scene.instances.len().max(1) * size_of::<GpuInstance>()) as u64;
        check_size("instance", inst_bytes, limits.max_buffer_size)?;

        let d = &self.device;
        let colors = color_buffer(d, color_capacity);
        let no_edges = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad no edges"),
            size: 12,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let box_bind = self.bind(&colors, &no_edges);
        let inst_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad instances"),
            size: inst_bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let boxes = scene
            .instances
            .iter()
            .enumerate()
            .map(|(i, inst)| {
                let (lo, hi) = inst.bbox?;
                let (a, b) = scene.meshes[inst.mesh as usize].local_box?;
                let m = inst.matrix * Mat4::from_translation(a) * Mat4::from_scale(b - a);
                let own = inst.colors.first().copied().unwrap_or(DEFAULT_COLOR);
                let gpu = gpu_instance(m, [i as u32 + 1, 0, palette_color(inst), u32::from_le_bytes(own)]);
                Some(BoxItem { lo, hi, gpu })
            })
            .collect();
        self.stats.gpu_bytes = (color_capacity * 4) as u64 + inst_bytes + 12;
        self.scene = Some(GpuScene {
            chunks: Vec::new(),
            meshes: (0..scene.meshes.len()).map(|_| None).collect(),
            colors,
            color_capacity,
            colors_written: 0,
            no_edges,
            box_bind,
            inst_buf,
            items: Vec::new(),
            boxes,
            visible: Vec::with_capacity(scene.instances.len()),
            runs: Vec::new(),
            box_range: 0..0,
        });
        self.stats.upload_ms = started.elapsed().as_secs_f64() * 1e3;
        Ok(())
    }

    /// Upload a batch of meshes as one chunk; `placed` are the instances that now have their
    /// mesh (`SceneData::add_meshes` numbered their faces), whose boxes give way to them.
    pub fn add_meshes(&mut self, scene: &SceneData, batch: &MeshBatch, placed: &[u32]) -> Result<(), String> {
        let started = std::time::Instant::now();
        let limits = self.device.limits();
        let storage_limit = limits.max_buffer_size.min(limits.max_storage_buffer_binding_size as u64);
        let Some(s) = self.scene.as_mut() else { return Ok(()) };

        let mut vertices: Vec<Vertex> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut edges: Vec<[f32; 3]> = Vec::new();
        let chunk = s.chunks.len() as u32;
        let mut placed_meshes = Vec::with_capacity(batch.meshes.len());
        for (k, m) in &batch.meshes {
            let (i0, e0) = (indices.len() as u32, edges.len() as u32);
            placed_meshes.push((
                *k,
                GpuMesh { base_vertex: vertices.len() as i32, indices: i0..i0 + m.indices.len() as u32, edges: e0..e0 + m.edges.len() as u32 },
            ));
            vertices.extend_from_slice(&m.vertices);
            indices.extend_from_slice(&m.indices);
            edges.extend_from_slice(&m.edges);
        }
        // Every buffer must fit the GPU on its own.
        check_size("vertex", (vertices.len() * size_of::<Vertex>()) as u64, limits.max_buffer_size)?;
        check_size("index", (indices.len() * 4) as u64, limits.max_buffer_size)?;
        check_size("edge", (edges.len() * 12) as u64, storage_limit)?;
        check_size("face colour", (scene.face_colors.len() * 4) as u64, storage_limit)?;

        let d = &self.device;
        // Empty buffers are not allowed; keep one dummy element.
        let init = |label, contents: &[u8], usage| {
            let dummy = [0u8; 12];
            let contents = if contents.is_empty() { &dummy[..] } else { contents };
            let buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage });
            (buf, contents.len() as u64)
        };
        let (vbuf, vb) = init("vibecad vertices", bytemuck::cast_slice(&vertices), wgpu::BufferUsages::VERTEX);
        let (ibuf, ib) = init("vibecad indices", bytemuck::cast_slice(&indices), wgpu::BufferUsages::INDEX);
        let (ebuf, eb) = init("vibecad edges", bytemuck::cast_slice(&edges), wgpu::BufferUsages::STORAGE);
        self.stats.gpu_bytes += vb + ib + eb;

        // The colours of the faces numbered since the last batch; a scene that outgrows its
        // estimate gets a bigger buffer, and every chunk its bindings anew.
        if scene.face_colors.len() > s.color_capacity {
            s.color_capacity = scene.face_colors.len().max(s.color_capacity * 2);
            s.colors = color_buffer(d, s.color_capacity);
            s.colors_written = 0;
            self.stats.gpu_bytes += (s.color_capacity * 4) as u64;
            s.box_bind = bind_group(d, &self.bind_layout, &self.ubuf, &s.colors, &s.no_edges);
            for c in &mut s.chunks {
                c.bind = bind_group(d, &self.bind_layout, &self.ubuf, &s.colors, &c.ebuf);
            }
        }
        if s.colors_written < scene.face_colors.len() {
            let new = &scene.face_colors[s.colors_written..];
            self.queue.write_buffer(&s.colors, (s.colors_written * 4) as u64, bytemuck::cast_slice(new));
            s.colors_written = scene.face_colors.len();
        }
        let bind = bind_group(d, &self.bind_layout, &self.ubuf, &s.colors, &ebuf);
        s.chunks.push(Chunk { vbuf, ibuf, ebuf, bind });
        for (k, gm) in placed_meshes {
            if let Some(slot) = s.meshes.get_mut(k as usize) {
                *slot = Some(gm);
            }
        }

        for &i in placed {
            let inst = &scene.instances[i as usize];
            if let Some(b) = s.boxes.get_mut(i as usize) {
                *b = None;
            }
            let (Some((lo, hi)), Some(face_base)) = (inst.bbox, inst.face_base) else { continue };
            let sides = match (scene.meshes[inst.mesh as usize].closed, inst.mirrored()) {
                (false, _) => Sides::Both,
                (true, false) => Sides::Front,
                (true, true) => Sides::FrontMirrored,
            };
            let gpu = gpu_instance(inst.matrix, [i + 1, face_base, palette_color(inst), 0]);
            s.items.push(Item { lo, hi, mesh: inst.mesh, chunk, sides, gpu });
        }
        s.items.sort_by_key(|it| (it.sides, it.chunk, it.mesh));
        // Wait for the copies, so the time below is the whole upload.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        self.stats.upload_ms += started.elapsed().as_secs_f64() * 1e3;
        Ok(())
    }

    /// Stop drawing the boxes of these instances (their mesh will not come).
    pub fn drop_boxes(&mut self, instances: &[u32]) {
        if let Some(s) = self.scene.as_mut() {
            for &i in instances {
                if let Some(b) = s.boxes.get_mut(i as usize) {
                    *b = None;
                }
            }
        }
    }

    /// Keep a scan on the GPU under `key`; its points go up a buffer group per frame.
    pub fn add_cloud(&mut self, key: u64, cloud: Arc<Cloud>) -> Result<(), String> {
        if let Some(why) = &self.unsupported {
            return Err(why.clone());
        }
        let limits = self.device.limits();
        let point = size_of::<cloud::Point>() as u64;
        let max = POINT_GROUP_BYTES.min(limits.max_buffer_size).min(limits.max_storage_buffer_binding_size as u64);
        let group_points = ((max / point) as usize / cloud::CHUNK * cloud::CHUNK).max(cloud::CHUNK);
        let chunks: Vec<GpuChunk> = cloud
            .header
            .chunks
            .iter()
            .map(|c| GpuChunk {
                group: (c.start as usize / group_points) as u32,
                start: (c.start as usize % group_points) as u32,
                count: c.count,
                lo: Vec3::from(c.lo),
                hi: Vec3::from(c.hi),
            })
            .collect();
        let d = &self.device;
        let uniform = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad scan placement"),
            size: size_of::<CloudUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sizes = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad scan point sizes"),
            size: (chunks.len().max(1) * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let gpu = GpuCloud {
            marks: vec![0; cloud.points.len().div_ceil(16).max(1)],
            has_color: cloud.header.has_color,
            source: Some(cloud),
            groups: Vec::new(),
            sizes_cpu: vec![POINT_MIN_SIZE; chunks.len()],
            chunks,
            group_points,
            uniform,
            sizes,
            draws: Vec::new(),
        };
        self.clouds.insert(key, gpu);
        Ok(())
    }

    pub fn remove_cloud(&mut self, key: u64) {
        self.clouds.remove(&key);
    }

    /// New marks (selected, deleted) for a scan's points, as `cloud::marks::Marks::words`.
    pub fn set_cloud_marks(&mut self, key: u64, words: &[u32]) {
        let Some(c) = self.clouds.get_mut(&key) else { return };
        let n = words.len().min(c.marks.len());
        c.marks[..n].copy_from_slice(&words[..n]);
        for g in &c.groups {
            let end = (g.first + c.group_points / 16).min(c.marks.len());
            if g.first < end {
                self.queue.write_buffer(&g.marks, 0, bytemuck::cast_slice(&c.marks[g.first..end]));
            }
        }
    }

    /// Whether every point of this scan is on the GPU.
    pub fn cloud_uploaded(&self, key: u64) -> bool {
        self.clouds.get(&key).is_some_and(|c| c.source.is_none())
    }

    /// Bytes of scan points on the GPU.
    pub fn cloud_bytes(&self) -> u64 {
        let point = size_of::<cloud::Point>() as u64;
        self.clouds.values().map(|c| c.chunks.iter().filter(|ch| (ch.group as usize) < c.groups.len()).map(|ch| ch.count as u64 * point).sum::<u64>()).sum()
    }

    /// One buffer group of one scan, so a big scan comes in over frames instead of freezing one.
    fn upload_clouds(&mut self) {
        let Some(c) = self.clouds.values_mut().find(|c| c.source.is_some()) else { return };
        let Some(src) = c.source.as_ref() else { return };
        let g = c.groups.len();
        let range = g * c.group_points..((g + 1) * c.group_points).min(src.points.len());
        // A scan without points still needs a buffer to bind.
        let zero = [cloud::Point::zeroed()];
        let contents = if range.is_empty() { &zero[..] } else { &src.points[range.clone()] };
        let points = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vibecad scan points"),
            contents: bytemuck::cast_slice(contents),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let first = range.start / 16;
        let words = &c.marks[first.min(c.marks.len())..range.end.div_ceil(16).clamp(first + 1, c.marks.len())];
        let marks = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vibecad scan marks"),
            contents: bytemuck::cast_slice(words),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vibecad scan bindings"),
            layout: &self.cloud_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: points.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: c.uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: c.sizes.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: marks.as_entire_binding() },
            ],
        });
        c.groups.push(CloudGroup { bind, _points: points, marks, first });
        if range.end >= src.points.len() {
            c.source = None;
        }
    }

    /// Which chunks of each scan show, how many of their points and how wide; their placement.
    fn prepare_clouds(&mut self, f: &Frame, size: (u32, u32)) {
        let vp = f.view_proj;
        // Pixels per unit of length at clip w = 1: row 1 of view_proj holds the projection's y
        // scale. An orthographic view has w = 1 everywhere.
        let y_scale = Vec3::new(vp.x_axis.y, vp.y_axis.y, vp.z_axis.y).length() * size.1 as f32 * 0.5;
        let perspective = vp.row(3).truncate().length_squared() > 1e-18;
        let budget = std::env::var("VIBECAD_POINT_BUDGET").ok().and_then(|v| v.parse().ok()).unwrap_or(POINT_BUDGET);
        let mut wanted = 0u64;
        // (scan, its draw, pixels across) for the budget pass.
        let mut spans: Vec<(u64, usize, f32)> = Vec::new();
        for c in self.clouds.values_mut() {
            c.draws.clear();
        }
        for d in &f.clouds {
            let Some(c) = self.clouds.get_mut(&d.key) else { continue };
            let mvp = (vp.as_dmat4() * d.model).as_mat4();
            let frustum = Frustum::new(mvp);
            for (k, ch) in c.chunks.iter().enumerate() {
                if ch.group as usize >= c.groups.len() || ch.count == 0 || !frustum.intersects(ch.lo, ch.hi) {
                    continue;
                }
                let w = (mvp * ((ch.lo + ch.hi) * 0.5).extend(1.0)).w;
                let diag = (ch.hi - ch.lo).length().max(1e-6);
                // The eye inside or next to the chunk: taken as half its size away, so the
                // chunk is drawn whole but its points do not swell to cover the screen.
                let px = diag * y_scale / if perspective { w.max(diag * 0.5) } else { w };
                let (count, width) = cloud::level_of_detail(ch.count, px, POINT_SPACING, POINT_MIN_SIZE);
                c.sizes_cpu[k] = width;
                spans.push((d.key, c.draws.len(), px));
                c.draws.push((ch.group, k as u32, count));
                wanted += count as u64;
            }
        }
        if wanted > budget {
            // Over budget: fewer points of every chunk, each grown to cover the same.
            let scale = budget as f32 / wanted as f32;
            for (key, i, px) in spans {
                let Some(c) = self.clouds.get_mut(&key) else { continue };
                let (_, k, count) = &mut c.draws[i];
                let k = *k as usize;
                *count = ((*count as f32 * scale) as u32).clamp(1, c.chunks[k].count);
                c.sizes_cpu[k] = cloud::point_size(*count, px, POINT_MIN_SIZE);
            }
        }
        self.stats.drawn_points = 0;
        for d in &f.clouds {
            let Some(c) = self.clouds.get(&d.key) else { continue };
            let n = Mat3::from_mat4(d.model.as_mat4());
            let n = if n.determinant().abs() > 1e-20 { n.inverse().transpose() } else { n };
            let show = f.scan_display;
            let color = match show.color {
                ScanColor::File => 0,
                ScanColor::Single => 1,
                ScanColor::Height => 2,
                ScanColor::Normal => 3,
            };
            let u = CloudUniforms {
                mvp: (vp.as_dmat4() * d.model).as_mat4().to_cols_array_2d(),
                normal: Mat4::from_mat3(n).to_cols_array_2d(),
                color: SCAN_COLOR,
                flags: [c.has_color as u32, d.selected as u32, color, show.lit as u32],
                ramp: [d.heights.0, d.heights.1, show.size.clamp(0.25, 8.0), 0.0],
                zrow: d.model.row(2).as_vec4().to_array(),
            };
            self.queue.write_buffer(&c.uniform, 0, bytemuck::bytes_of(&u));
            if !c.sizes_cpu.is_empty() {
                self.queue.write_buffer(&c.sizes, 0, bytemuck::cast_slice(&c.sizes_cpu));
            }
            self.stats.drawn_points += c.draws.iter().map(|d| d.2 as u64).sum::<u64>();
        }
    }

    fn draw_clouds(&self, pass: &mut wgpu::RenderPass<'_>, f: &Frame) {
        let mut set = false;
        for d in &f.clouds {
            let Some(c) = self.clouds.get(&d.key) else { continue };
            let mut group = None;
            for &(g, k, count) in &c.draws {
                if !set {
                    pass.set_pipeline(&self.point_pipeline);
                    pass.set_bind_group(0, &self.base_bind, &[]);
                    set = true;
                }
                if group != Some(g) {
                    pass.set_bind_group(1, &c.groups[g as usize].bind, &[]);
                    group = Some(g);
                }
                let start = c.chunks[k as usize].start;
                pass.draw(start * 6..(start + count) * 6, k..k + 1);
            }
        }
    }

    fn bind(&self, colors: &wgpu::Buffer, edges: &wgpu::Buffer) -> wgpu::BindGroup {
        bind_group(&self.device, &self.bind_layout, &self.ubuf, colors, edges)
    }

    fn ensure_targets(&mut self, size: (u32, u32)) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let tex = |label, format, usage, sample_count| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        use wgpu::TextureUsages as TU;
        let view = |t: wgpu::Texture| t.create_view(&Default::default());
        let n = self.samples;
        let color = tex("vibecad color", COLOR_FORMAT, TU::RENDER_ATTACHMENT | TU::TEXTURE_BINDING | TU::COPY_SRC, 1);
        let msaa_view = (n > 1).then(|| view(tex("vibecad color msaa", COLOR_FORMAT, TU::RENDER_ATTACHMENT, n)));
        let depth_view = view(tex("vibecad depth", DEPTH_FORMAT, TU::RENDER_ATTACHMENT, n));
        let id_tex = tex("vibecad ids", ID_FORMAT, TU::RENDER_ATTACHMENT | TU::COPY_SRC, 1);
        let id_depth_view = view(tex("vibecad id depth", DEPTH_FORMAT, TU::RENDER_ATTACHMENT, 1));
        let color_view = color.create_view(&Default::default());
        let mut r = self.egui_renderer.write();
        match self.texture_id {
            Some(id) => r.update_egui_texture_from_wgpu_texture(&self.device, &color_view, wgpu::FilterMode::Linear, id),
            None => self.texture_id = Some(r.register_native_texture(&self.device, &color_view, wgpu::FilterMode::Linear)),
        }
        drop(r);
        self.targets = Some(Targets {
            size,
            color_tex: color,
            color_view,
            msaa_view,
            depth_view,
            id_view: id_tex.create_view(&Default::default()),
            id_tex,
            id_depth_view,
        });
    }

    /// Keep the instances whose box is in view, packed into the instance buffer in runs.
    fn cull(&mut self, view_proj: Mat4) {
        let Some(s) = self.scene.as_mut() else { return };
        let frustum = Frustum::new(view_proj);
        s.visible.clear();
        s.runs.clear();
        for it in &s.items {
            if !frustum.intersects(it.lo, it.hi) {
                continue;
            }
            let n = s.visible.len() as u32;
            match s.runs.last_mut() {
                Some(r) if r.mesh == it.mesh && r.sides == it.sides => r.instances.end = n + 1,
                _ => s.runs.push(Run { chunk: it.chunk, mesh: it.mesh, sides: it.sides, instances: n..n + 1 }),
            }
            s.visible.push(it.gpu);
        }
        let first_box = s.visible.len() as u32;
        for b in s.boxes.iter().flatten() {
            if frustum.intersects(b.lo, b.hi) {
                s.visible.push(b.gpu);
            }
        }
        s.box_range = first_box..s.visible.len() as u32;
        if !s.visible.is_empty() {
            self.queue.write_buffer(&s.inst_buf, 0, bytemuck::cast_slice(&s.visible));
        }
        self.stats.drawn_instances = s.visible.len() as u32;
        self.stats.drawn_triangles = s
            .runs
            .iter()
            .filter_map(|r| Some((r.instances.len() * s.meshes[r.mesh as usize].as_ref()?.indices.len() / 3) as u64))
            .sum();
    }

    /// Draw into the offscreen texture; `texture_id` then shows it.
    pub fn render(&mut self, size: (u32, u32), f: &Frame) {
        let max = self.device.limits().max_texture_dimension_2d;
        let size = (size.0.clamp(1, max), size.1.clamp(1, max));
        self.ensure_targets(size);
        self.stats.drawn_instances = 0;
        self.stats.drawn_triangles = 0;
        self.cull(f.view_proj);
        self.upload_clouds();
        self.prepare_clouds(f, size);
        let t = self.targets.as_ref().unwrap();
        let u = Uniforms {
            view_proj: f.view_proj.to_cols_array_2d(),
            light: [f.forward.x, f.forward.y, f.forward.z, 0.0],
            sel: [f.selected_face, f.selected_instance, 0, 0],
            view: [size.0 as f32, size.1 as f32, f.edge_width.clamp(0.5, 8.0), 0.0],
            opts: [0, f.palette as u32, 0, 0],
            grid: f.grid.map_or([0.0; 4], |g| [g.half, g.major, g.minor, 1.0]),
            bg: [f.background[0] as f32, f.background[1] as f32, f.background[2] as f32, 1.0],
        };
        self.queue.write_buffer(&self.ubuf, 0, bytemuck::bytes_of(&u));
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let [r, g, b] = f.background;
            let clear = wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 });
            // With MSAA the samples are resolved into the texture egui shows and then dropped.
            let color = match &t.msaa_view {
                Some(msaa) => wgpu::RenderPassColorAttachment {
                    view: msaa,
                    depth_slice: None,
                    resolve_target: Some(&t.color_view),
                    ops: wgpu::Operations { load: clear, store: wgpu::StoreOp::Discard },
                },
                None => wgpu::RenderPassColorAttachment {
                    view: &t.color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: clear, store: wgpu::StoreOp::Store },
                },
            };
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vibecad view"),
                color_attachments: &[Some(color)],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // The background and the grid use the uniforms only.
            pass.set_pipeline(&self.background_pipeline);
            pass.set_bind_group(0, &self.base_bind, &[]);
            pass.draw(0..3, 0..1);
            if let Some(s) = self.scene.as_ref() {
                draw_faces(&mut pass, s, &self.face_pipelines);
            }
            self.draw_clouds(&mut pass, f);
            if f.grid.is_some() && (self.scene.is_some() || !f.clouds.is_empty()) {
                pass.set_pipeline(&self.grid_pipeline);
                pass.set_bind_group(0, &self.base_bind, &[]);
                pass.draw(0..6, 0..1);
            }
            if let Some(s) = self.scene.as_ref() {
                if f.show_edges && !s.runs.is_empty() {
                    // Six vertices per segment, two points per segment: three per point.
                    pass.set_pipeline(&self.edge_pipeline);
                    pass.set_vertex_buffer(1, s.inst_buf.slice(..));
                    let mut chunk = None;
                    for r in &s.runs {
                        let Some(m) = &s.meshes[r.mesh as usize] else { continue };
                        if m.edges.is_empty() {
                            continue;
                        }
                        if chunk != Some(r.chunk) {
                            pass.set_bind_group(0, &s.chunks[r.chunk as usize].bind, &[]);
                            chunk = Some(r.chunk);
                        }
                        pass.draw(m.edges.start * 3..m.edges.end * 3, r.instances.clone());
                    }
                }
                if !s.box_range.is_empty() {
                    pass.set_pipeline(&self.box_pipeline);
                    pass.set_bind_group(0, &s.box_bind, &[]);
                    pass.set_vertex_buffer(0, self.cube.slice(..));
                    pass.set_vertex_buffer(1, s.inst_buf.slice(..));
                    pass.draw(0..36, s.box_range.clone());
                }
            }
        }
        self.queue.submit([enc.finish()]);
        self.drawn_gen = Some(self.scene_gen);
    }

    /// Milliseconds per frame, CPU and GPU, over `n` frames drawn back to back and waited for.
    pub fn bench(&mut self, size: (u32, u32), f: &Frame, n: u32) -> f64 {
        self.render(size, f);
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let started = std::time::Instant::now();
        for _ in 0..n {
            self.render(size, f);
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        }
        started.elapsed().as_secs_f64() * 1e3 / n.max(1) as f64
    }

    /// Size in pixels of the last frame.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.targets.as_ref().map(|t| t.size)
    }

    /// The last frame as RGBA rows, top row first.
    pub fn read_color(&self) -> Option<(u32, u32, Vec<u8>)> {
        let t = self.targets.as_ref()?;
        let (w, h) = t.size;
        let row = (w * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vibecad capture"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.color_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().ok()?.ok()?;
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        if let Ok(data) = slice.get_mapped_range() {
            for y in 0..h {
                let start = (y * row) as usize;
                out.extend_from_slice(&data[start..start + (w * 4) as usize]);
            }
        }
        buf.unmap();
        (!out.is_empty()).then_some((w, h, out))
    }

    /// Face id under a pixel of the last frame (0 = background). Draws the id pass for that one
    /// pixel, with the camera, the uniforms and the culled instances of the last frame (still in
    /// their buffers), then reads it back. Edges are not drawn there: a click on an edge picks
    /// the face under it.
    pub fn pick(&self, x: u32, y: u32) -> u32 {
        let Some(t) = &self.targets else { return 0 };
        if x >= t.size.0 || y >= t.size.1 || self.drawn_gen != Some(self.scene_gen) {
            return 0;
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vibecad ids"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &t.id_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.id_depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(s) = self.scene.as_ref() {
                pass.set_scissor_rect(x, y, 1, 1);
                draw_faces(&mut pass, s, &self.id_pipelines);
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &t.id_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.pick_buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = self.pick_buf.slice(0..4);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        if !matches!(rx.recv(), Ok(Ok(()))) {
            return 0; // not mapped: nothing to unmap
        }
        let id = match slice.get_mapped_range() {
            Ok(data) => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            Err(_) => 0,
        };
        self.pick_buf.unmap();
        id
    }
}

/// Everything the colour and id passes share and that wgpu wants in one struct.
struct PipelineDesc<'a> {
    label: &'a str,
    vs: &'a str,
    fs: &'a str,
    buffers: &'a [Option<wgpu::VertexBufferLayout<'a>>],
    targets: &'a [Option<wgpu::ColorTargetState>],
    primitive: wgpu::PrimitiveState,
    bias: wgpu::DepthBiasState,
    compare: wgpu::CompareFunction,
    depth_write: bool,
    samples: u32,
}

/// The visible instances' triangles, one instanced draw per run, with `pipelines[sides]`.
fn draw_faces(pass: &mut wgpu::RenderPass<'_>, s: &GpuScene, pipelines: &[wgpu::RenderPipeline; 3]) {
    if s.runs.is_empty() {
        return;
    }
    pass.set_vertex_buffer(1, s.inst_buf.slice(..));
    let (mut sides, mut chunk) = (None, None);
    for r in &s.runs {
        let Some(m) = &s.meshes[r.mesh as usize] else { continue };
        if m.indices.is_empty() {
            continue;
        }
        if sides != Some(r.sides) {
            pass.set_pipeline(&pipelines[r.sides as usize]);
            sides = Some(r.sides);
        }
        if chunk != Some(r.chunk) {
            let c = &s.chunks[r.chunk as usize];
            pass.set_bind_group(0, &c.bind, &[]);
            pass.set_vertex_buffer(0, c.vbuf.slice(..));
            pass.set_index_buffer(c.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            chunk = Some(r.chunk);
        }
        pass.draw_indexed(m.indices.clone(), m.base_vertex, r.instances.clone());
    }
}

fn check_size(what: &str, bytes: u64, limit: u64) -> Result<(), String> {
    if bytes > limit {
        return Err(format!("the scene needs a {} MB {what} buffer and this GPU allows {} MB; hide some objects", bytes >> 20, limit >> 20));
    }
    Ok(())
}

fn color_buffer(d: &wgpu::Device, faces: usize) -> wgpu::Buffer {
    d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vibecad face colours"),
        size: (faces.max(1) * 4) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn bind_group(d: &wgpu::Device, layout: &wgpu::BindGroupLayout, ubuf: &wgpu::Buffer, colors: &wgpu::Buffer, edges: &wgpu::Buffer) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vibecad bindings"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: colors.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: edges.as_entire_binding() },
        ],
    })
}

/// The instance record for a placement: model matrix, normal matrix, ids.
fn gpu_instance(model: Mat4, ids: [u32; 4]) -> GpuInstance {
    let n = Mat3::from_mat4(model);
    let normal = if n.determinant().abs() > 1e-20 { n.inverse().transpose() } else { n };
    GpuInstance { model: model.to_cols_array_2d(), normal: normal.to_cols_array_2d(), ids }
}

/// `[0, 1]³` as 12 triangles, counter-clockwise from outside: position, normal.
fn unit_cube() -> Vec<[f32; 6]> {
    let mut out = Vec::with_capacity(36);
    for axis in 0..3 {
        for side in [0.0f32, 1.0] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let corner = |a: f32, b: f32| {
                let mut p = [0.0f32; 3];
                p[axis] = side;
                p[u] = a;
                p[v] = b;
                p
            };
            let mut n = [0.0f32; 3];
            n[axis] = if side > 0.5 { 1.0 } else { -1.0 };
            // (u, v, axis) is right-handed: going round u then v faces +axis.
            let mut quad = [corner(0.0, 0.0), corner(1.0, 0.0), corner(1.0, 1.0), corner(0.0, 1.0)];
            if side < 0.5 {
                quad.reverse();
            }
            for k in [0, 1, 2, 0, 2, 3] {
                let p = quad[k];
                out.push([p[0], p[1], p[2], n[0], n[1], n[2]]);
            }
        }
    }
    out
}

/// `MSAA` samples if this GPU can draw and resolve them in the view's formats, else 1.
/// `VIBECAD_NO_MSAA` forces 1, to see the fallback.
fn msaa_samples(rs: &egui_wgpu::RenderState) -> u32 {
    if std::env::var_os("VIBECAD_NO_MSAA").is_some() {
        return 1;
    }
    // Without this feature the device allows only the guaranteed capabilities, whatever the
    // adapter reports.
    let features = rs.device.features();
    let flags = |f: wgpu::TextureFormat| {
        if features.contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES) {
            rs.adapter.get_texture_format_features(f).flags
        } else {
            f.guaranteed_format_features(features).flags
        }
    };
    let (color, depth) = (flags(COLOR_FORMAT), flags(DEPTH_FORMAT));
    let ok = color.sample_count_supported(MSAA)
        && color.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE)
        && depth.sample_count_supported(MSAA);
    if ok { MSAA } else { 1 }
}

/// A palette colour (RGBA8) for an instance whose faces all have FreeCAD's default grey, chosen
/// by a stable hash of its object name; 0, keep the document's colours, otherwise.
fn palette_color(inst: &InstanceInfo) -> u32 {
    if inst.colors.iter().any(|c| *c != DEFAULT_COLOR) {
        return 0;
    }
    // FNV-1a: the same name gets the same colour in every session.
    let h = inst.obj.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    let [r, g, b] = PALETTE[h as usize % PALETTE.len()];
    u32::from_le_bytes([r, g, b, 255])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_cube_faces_outward_counter_clockwise() {
        let cube = unit_cube();
        assert_eq!(cube.len(), 36);
        for t in cube.chunks_exact(3) {
            let p = |k: usize| Vec3::new(t[k][0], t[k][1], t[k][2]);
            let n = Vec3::new(t[0][3], t[0][4], t[0][5]);
            assert!((p(1) - p(0)).cross(p(2) - p(0)).dot(n) > 0.0, "{t:?}");
            // the normal points away from the centre
            assert!((p(0) - Vec3::splat(0.5)).dot(n) > 0.0);
        }
    }

    #[test]
    fn ground_grid_steps_like_fusion() {
        // the sample plate, 80 x 50 mm from the origin
        let g = GroundGrid::around(Vec3::ZERO, Vec3::new(80.0, 50.0, 6.0));
        assert_eq!((g.major, g.minor), (10.0, 2.0));
        assert!(g.half >= 92.0 && (g.half / g.major).fract() == 0.0, "{g:?}");
        // a few hundred millimetres across: 25 mm steps, as Fusion shows
        let g = GroundGrid::around(Vec3::new(-180.0, -170.0, 0.0), Vec3::new(160.0, 175.0, 300.0));
        assert_eq!(g.major, 25.0);
        assert!(g.half >= 207.0);
        // zoomed in, the lines get finer in the same steps, never coarser than the scene's
        let near = g.for_scale(40.0);
        assert_eq!((near.major, near.minor, near.half), (2.5, 0.5, g.half));
        assert_eq!(g.for_scale(0.01).major, 25.0);
        // a tiny part far from nothing still gets a grid
        let g = GroundGrid::around(Vec3::splat(0.1), Vec3::splat(0.2));
        assert!(g.half > 0.0 && g.minor > 0.0);
    }

    #[test]
    fn shader_compiles() {
        let module = naga::front::wgsl::parse_str(SHADER).expect("WGSL parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .expect("WGSL validates");
    }
}
