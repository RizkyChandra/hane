//! The WebGPU submitter: the second backend, running the same `hane_gpu`
//! draw data through a compute shader (#24, D-003, D-010).
//!
//! `hane-gpu` is untouched by this file. It flattens, bins, narrows to `f32`
//! and orders the commands exactly as it does for WebGL2; both backends consume
//! the same [`DrawData`], which is what keeps them diffable against each other
//! and against the P1 oracle.
//!
//! # What compute shaders changed, and what they did not
//!
//! Not the rasterization. D-011 chose the oracle's own coverage loop -- sixteen
//! sample lines per pixel, the crossings inside that pixel sorted, analytic
//! horizontal spans -- because it is the only one of the three candidates that
//! can *be right* on a corpus where `pentagram` puts a winding-2 sector and a
//! winding-0 one inside one pixel. That argument does not depend on the shader
//! stage, so the loop below is a line-for-line port of `glrender.rs`'s. A
//! backend that missed the oracle would not be a faster backend, it would be a
//! different picture.
//!
//! What compute changed is everything *around* the loop:
//!
//! - **The framebuffer is a storage buffer of packed RGBA8, not a texture.** An
//!   invocation reads and writes its own pixel, so the ping-pong that WebGL2
//!   forced -- a full-target blit before every single draw, because a fragment
//!   shader cannot read the pixel it is about to write -- is gone. That was the
//!   standing `ponytail:` note at the top of `glrender.rs`.
//! - **The 8-bit round trip is gone with it.** The GL path computed
//!   `floor(v + 0.5)`, divided by 255 and trusted the driver to store the byte
//!   back unchanged; here the byte *is* the value, packed by hand.
//! - **One workgroup is one tile.** `TILE_SIZE` is 16, a 16x16 workgroup is 256
//!   invocations, and the tile instances `hane-gpu` already emits become the
//!   dispatch grid unchanged. No vertex stage, no quad, no rasterizer.
//! - **One pipeline for the whole renderer.** Clip reset, clip path, fill, group
//!   push and group pop are five modes of one entry point rather than three
//!   programs, two vertex shaders and a fixed-function blend state.
//!
//! So the same picture comes out with no scratch targets, no blit, no blend
//! state and no vertex stage. Whether it is *faster* is a separate question and
//! is measured in `BENCHMARKS.md`; correctness came first.
//!
//! # Ordering
//!
//! Every op is one dispatch into one compute pass. WebGPU orders dispatches
//! within a pass and inserts the memory barriers between them, so op *n + 1*
//! sees op *n*'s writes -- which is what lets a draw read the backdrop it is
//! compositing onto with no copy at all.

use crate::glrender::{MAX_CROSSINGS, blend_code, group_depth, rule_code};
use hane_gpu::{DrawData, MAX_STOPS, Op, PaintData, TILE_SIZE};
use hane_raster::{Scene, corpus};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    GpuAutoLayoutMode, GpuBindGroup, GpuBindGroupDescriptor, GpuBindGroupEntry, GpuBuffer,
    GpuBufferBinding, GpuBufferDescriptor, GpuComputePipeline, GpuComputePipelineDescriptor,
    GpuDevice, GpuErrorFilter, GpuProgrammableStage, GpuShaderModuleDescriptor, HtmlCanvasElement,
};

/// The workgroup is one tile, so the shader's hard-coded 16 and the binner's
/// tile size are the same number. A `TILE_SIZE` of anything else would put the
/// wrong pixels under the wrong edge list, silently.
const _: () = assert!(TILE_SIZE == 16);

// `GPUBufferUsage` and `GPUMapMode` are JS namespace objects with no web-sys
// binding, so the bits are spelled out. They are spec constants and cannot
// change without a new WebGPU version.
const USAGE_MAP_READ: u32 = 0x0001;
const USAGE_COPY_SRC: u32 = 0x0004;
const USAGE_COPY_DST: u32 = 0x0008;
const USAGE_UNIFORM: u32 = 0x0040;
const USAGE_STORAGE: u32 = 0x0080;
const MAP_READ: u32 = 0x0001;

/// Bytes per per-op uniform slot.
///
/// The block itself is 400 bytes; the stride is the 256-byte
/// `minUniformBufferOffsetAlignment` floor rounded up, so every op's block
/// starts at a legal offset in one buffer rather than needing a buffer each.
const PARAM_STRIDE: usize = 512;

/// `PARAM_STRIDE` as `u32` words, which is how the block is built.
const PARAM_WORDS: usize = PARAM_STRIDE / 4;

// The five modes of the one entry point. Kept in step with the `MODE_` block of
// `MAIN_WGSL` by `modes_agree_with_the_shader`.
const MODE_CLEAR: u32 = 0;
const MODE_CLIP_RESET: u32 = 1;
const MODE_CLIP_PATH: u32 = 2;
const MODE_FILL: u32 = 3;
const MODE_GROUP: u32 = 4;

// ---------------------------------------------------------------------------
// the shader
// ---------------------------------------------------------------------------

/// Bindings and the uniform block every mode reads.
///
/// The block is laid out so every field lands where [`Job::params`] puts it:
/// scalars pack at four bytes each from offset 0, the two arrays start at the
/// next 16-byte boundary, and the whole thing is 400 bytes inside a
/// [`PARAM_STRIDE`] slot.
const HEAD_WGSL: &str = r#"
struct Params {
    g0: vec2<f32>,
    g1: vec2<f32>,
    radius: f32,
    alpha: f32,
    kind: u32,
    spread: u32,
    rule: u32,
    blend: u32,
    use_clip: u32,
    stop_count: u32,
    first_inst: u32,
    mode: u32,
    dst_base: u32,
    src_base: u32,
    width: u32,
    height: u32,
    pad: vec2<u32>,
    offsets: array<vec4<f32>, 4>,
    colors: array<vec4<f32>, 16>,
};

// Four floats per edge, `ax, ay, bx, by`, in the direction the path runs.
@group(0) @binding(0) var<storage, read> edges: array<vec4<f32>>;
// Four per tile instance: origin x, origin y, first edge, edge count.
@group(0) @binding(1) var<storage, read> insts: array<vec4<f32>>;
// The canvas and, above it, one plane per group nesting level. Premultiplied
// RGBA8 packed little-endian, which is the `Pixmap` byte order.
@group(0) @binding(2) var<storage, read_write> layers: array<u32>;
@group(0) @binding(3) var<storage, read_write> clipmask: array<f32>;
@group(0) @binding(4) var<uniform> P: Params;

fn unpack_px(v: u32) -> vec4<f32> {
    return vec4<f32>(f32(v & 255u), f32((v >> 8u) & 255u),
                     f32((v >> 16u) & 255u), f32((v >> 24u) & 255u));
}

fn pack_px(c: vec4<f32>) -> u32 {
    let q = vec4<u32>(clamp(c, vec4<f32>(0.0), vec4<f32>(255.0)));
    return q.x | (q.y << 8u) | (q.z << 16u) | (q.w << 24u);
}

// WGSL has no `isnan`/`isinf`: the shading language is allowed to assume they
// never happen, so a `x != x` test is not reliable. The exponent bits are.
fn is_nan(x: f32) -> bool {
    return (bitcast<u32>(x) & 0x7fffffffu) > 0x7f800000u;
}

fn non_finite(x: f32) -> bool {
    return (bitcast<u32>(x) & 0x7f800000u) == 0x7f800000u;
}
"#;

/// The coverage loop: the oracle's `coverage_row`, one pixel at a time.
///
/// A line-for-line port of `glrender.rs`'s `COVERAGE_GLSL`. Any difference
/// between the two is a bug in one of them.
const COVERAGE_WGSL: &str = r#"
fn inside(w: i32) -> bool {
    if (P.rule == 0u) { return w != 0; }
    return (w % 2) != 0;
}

fn coverage(start: u32, count: u32, px: f32, py: f32) -> f32 {
    var xs: array<f32, MAXC>;
    var ds: array<i32, MAXC>;
    var cov = 0.0;
    for (var s = 0; s < 16; s = s + 1) {
        // The sub-band midpoint, exactly as the oracle picks it.
        let sy = py + (f32(s) + 0.5) * 0.0625;
        var w0 = 0;
        var n = 0;
        for (var e = 0u; e < count; e = e + 1u) {
            let ed = edges[start + e];
            var top: vec2<f32>;
            var bot: vec2<f32>;
            var dir: i32;
            if (ed.y < ed.w) { top = ed.xy; bot = ed.zw; dir = 1; }
            else             { top = ed.zw; bot = ed.xy; dir = -1; }
            // Half-open in y: a vertex shared by two edges crosses once.
            if (sy < top.y || sy >= bot.y) { continue; }
            let t = (sy - top.y) / (bot.y - top.y);
            let xc = (1.0 - t) * top.x + t * bot.x;
            if (xc < px) {
                // Left of this pixel: it only moves the winding number we start
                // from. This is why a tile needs the edges to its left.
                w0 = w0 + dir;
            } else if (xc < px + 1.0 && n < i32(MAXC)) {
                xs[n] = xc;
                ds[n] = dir;
                n = n + 1;
            }
        }
        // Insertion sort: n is 0 or 1 for almost every pixel of a real path.
        for (var i = 1; i < n; i = i + 1) {
            let kx = xs[i];
            let kd = ds[i];
            var j = i - 1;
            while (j >= 0 && xs[j] > kx) {
                xs[j + 1] = xs[j];
                ds[j + 1] = ds[j];
                j = j - 1;
            }
            xs[j + 1] = kx;
            ds[j + 1] = kd;
        }
        var w = w0;
        var prev = px;
        for (var i = 0; i < n; i = i + 1) {
            if (inside(w)) { cov = cov + (xs[i] - prev) * 0.0625; }
            prev = xs[i];
            w = w + ds[i];
        }
        if (inside(w)) { cov = cov + (px + 1.0 - prev) * 0.0625; }
    }
    return clamp(cov, 0.0, 1.0);
}
"#;

/// Paint evaluation and the ordered dither, mirroring `hane-raster::paint`.
const PAINT_WGSL: &str = r#"
var<private> BAYER: array<i32, 64> = array<i32, 64>(
     0, 32,  8, 40,  2, 34, 10, 42,
    48, 16, 56, 24, 50, 18, 58, 26,
    12, 44,  4, 36, 14, 46,  6, 38,
    60, 28, 52, 20, 62, 30, 54, 22,
     3, 35, 11, 43,  1, 33,  9, 41,
    51, 19, 59, 27, 49, 17, 57, 25,
    15, 47,  7, 39, 13, 45,  5, 37,
    63, 31, 55, 23, 61, 29, 53, 21);

fn dither_at(x: u32, y: u32) -> f32 {
    return (f32(BAYER[(y % 8u) * 8u + (x % 8u)]) + 0.5) / 64.0 - 0.5;
}

fn spread_map(t: f32) -> f32 {
    // A degenerate geometry hands over a non-finite parameter; NaN is not
    // greater than zero, so it lands on the start stop.
    if (non_finite(t)) { if (t > 0.0) { return 1.0; } return 0.0; }
    if (P.spread == 0u) { return clamp(t, 0.0, 1.0); }
    if (P.spread == 1u) { return t - floor(t); }
    let h = t * 0.5;
    let r = (h - floor(h)) * 2.0;
    if (r > 1.0) { return 2.0 - r; }
    return r;
}

fn stop_offset(i: u32) -> f32 { return P.offsets[i / 4u][i % 4u]; }

fn ramp_at(t: f32) -> vec4<f32> {
    if (P.stop_count == 0u) { return vec4<f32>(0.0); }
    let last = P.stop_count - 1u;
    if (t <= stop_offset(0u)) { return P.colors[0]; }
    if (t >= stop_offset(last)) { return P.colors[last]; }
    for (var i = 0u; i < last; i = i + 1u) {
        if (t < stop_offset(i + 1u)) {
            let a = stop_offset(i);
            let b = stop_offset(i + 1u);
            if (b <= a) { return P.colors[i + 1u]; }
            let u = (t - a) / (b - a);
            return (1.0 - u) * P.colors[i] + u * P.colors[i + 1u];
        }
    }
    return P.colors[last];
}

fn radial_t(p: vec2<f32>) -> f32 {
    if (is_nan(P.radius) || P.radius <= 0.0) { return 1.0; }
    var f = P.g1;
    let off = P.g1 - P.g0;
    let dist = length(off);
    // A focus on the rim collapses the whole gradient to a point, so SVG's
    // "pull it inside" is taken a hair short of the boundary.
    if (dist > P.radius * 0.999) { f = P.g0 + off * (P.radius * 0.999 / dist); }
    let u = p - f;
    let a = dot(u, u);
    if (a == 0.0) { return 0.0; }
    let e = f - P.g0;
    let b = dot(e, u);
    let c = dot(e, e) - P.radius * P.radius;
    let k = (-b + sqrt(b * b - a * c)) / a;
    return 1.0 / k;
}

fn paint_at(p: vec2<f32>) -> vec4<f32> {
    if (P.kind == 0u) { return P.colors[0]; }
    var t: f32;
    if (P.kind == 1u) {
        let axis = P.g1 - P.g0;
        let len2 = dot(axis, axis);
        if (len2 > 0.0) { t = dot(p - P.g0, axis) / len2; } else { t = 1.0; }
    } else {
        t = radial_t(p);
    }
    return ramp_at(spread_map(t));
}
"#;

/// The blend functions and the composite around them, mirroring
/// `hane-raster::fill`.
const BLEND_WGSL: &str = r#"
fn screen_b(s: f32, b: f32) -> f32 { return s + b - s * b; }

fn hard_light(s: f32, b: f32) -> f32 {
    if (s <= 0.5) { return 2.0 * s * b; }
    return screen_b(2.0 * s - 1.0, b);
}

fn soft_light(s: f32, b: f32) -> f32 {
    var d = sqrt(b);
    if (b <= 0.25) { d = ((16.0 * b - 12.0) * b + 4.0) * b; }
    if (s <= 0.5) { return b - (1.0 - 2.0 * s) * b * (1.0 - b); }
    return b + (2.0 * s - 1.0) * (d - b);
}

fn dodge(s: f32, b: f32) -> f32 {
    if (b <= 0.0) { return 0.0; }
    if (s >= 1.0) { return 1.0; }
    return min(b / (1.0 - s), 1.0);
}

fn burn(s: f32, b: f32) -> f32 {
    if (b >= 1.0) { return 1.0; }
    if (s <= 0.0) { return 0.0; }
    return 1.0 - min((1.0 - b) / s, 1.0);
}

fn lum_of(c: vec3<f32>) -> f32 { return 0.3 * c.r + 0.59 * c.g + 0.11 * c.b; }

fn clip_color(c0: vec3<f32>) -> vec3<f32> {
    var c = c0;
    let l = lum_of(c);
    let n = min(c.r, min(c.g, c.b));
    let x = max(c.r, max(c.g, c.b));
    if (n < 0.0 && l > n) { c = l + (c - l) * l / (l - n); }
    if (x > 1.0 && x > l) { c = l + (c - l) * (1.0 - l) / (x - l); }
    return c;
}

fn set_lum(c: vec3<f32>, l: f32) -> vec3<f32> { return clip_color(c + (l - lum_of(c))); }

fn sat_of(c: vec3<f32>) -> f32 {
    return max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b));
}

fn set_sat(c: vec3<f32>, s: f32) -> vec3<f32> {
    let n = min(c.r, min(c.g, c.b));
    let x = max(c.r, max(c.g, c.b));
    if (x <= n) { return vec3<f32>(0.0); }
    return (c - n) * s / (x - n);
}

fn blend_rgb(cs: vec3<f32>, cb: vec3<f32>) -> vec3<f32> {
    if (P.blend == 1u) { return cs * cb; }
    if (P.blend == 2u) { return vec3<f32>(screen_b(cs.r, cb.r), screen_b(cs.g, cb.g), screen_b(cs.b, cb.b)); }
    if (P.blend == 3u) { return vec3<f32>(hard_light(cb.r, cs.r), hard_light(cb.g, cs.g), hard_light(cb.b, cs.b)); }
    if (P.blend == 4u) { return min(cs, cb); }
    if (P.blend == 5u) { return max(cs, cb); }
    if (P.blend == 6u) { return vec3<f32>(dodge(cs.r, cb.r), dodge(cs.g, cb.g), dodge(cs.b, cb.b)); }
    if (P.blend == 7u) { return vec3<f32>(burn(cs.r, cb.r), burn(cs.g, cb.g), burn(cs.b, cb.b)); }
    if (P.blend == 8u) { return vec3<f32>(hard_light(cs.r, cb.r), hard_light(cs.g, cb.g), hard_light(cs.b, cb.b)); }
    if (P.blend == 9u) { return vec3<f32>(soft_light(cs.r, cb.r), soft_light(cs.g, cb.g), soft_light(cs.b, cb.b)); }
    if (P.blend == 10u) { return abs(cb - cs); }
    if (P.blend == 11u) { return cs + cb - 2.0 * cs * cb; }
    if (P.blend == 12u) { return set_lum(set_sat(cs, sat_of(cb)), lum_of(cb)); }
    if (P.blend == 13u) { return set_lum(set_sat(cb, sat_of(cs)), lum_of(cb)); }
    if (P.blend == 14u) { return set_lum(cs, lum_of(cb)); }
    if (P.blend == 15u) { return set_lum(cb, lum_of(cs)); }
    return cs;
}

// `src` and `dst` are premultiplied on the 0..255 scale, `dst` straight off the
// storage buffer and therefore exactly the integers the last draw wrote.
fn composite(src0: vec4<f32>, dst: vec4<f32>, cov: f32, noise: f32) -> vec4<f32> {
    var src = src0;
    if (P.blend != 0u && dst.a > 0.0 && src.a > 0.0) {
        let cs = src.rgb / src.a;
        let cb = dst.rgb / dst.a;
        let ab = dst.a / 255.0;
        let mixed = clamp(mix(cs, blend_rgb(cs, cb), ab), vec3<f32>(0.0), vec3<f32>(1.0));
        src = vec4<f32>(mixed * src.a, src.a);
    }
    let inv = 1.0 - src.a * cov / 255.0;
    // `floor(v + 0.5)` is Rust's `.round()` for a non-negative v. Unlike the GL
    // path this is the stored value, not a float on its way through a unorm8
    // texture write.
    return clamp(floor(src * cov + dst * inv + noise + 0.5), vec4<f32>(0.0), vec4<f32>(255.0));
}
"#;

/// The one entry point: five modes over one 16x16 workgroup per tile.
const MAIN_WGSL: &str = r#"
const MODE_CLEAR: u32 = 0u;
const MODE_CLIP_RESET: u32 = 1u;
const MODE_CLIP_PATH: u32 = 2u;
const MODE_FILL: u32 = 3u;
const MODE_GROUP: u32 = 4u;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_id) li: vec3<u32>) {
    var px: u32;
    var py: u32;
    var start = 0u;
    var count = 0u;
    if (P.mode == MODE_FILL || P.mode == MODE_CLIP_PATH) {
        // One workgroup per tile instance: the binning `hane-gpu` already did
        // is the dispatch grid, unchanged.
        let inst = insts[P.first_inst + wg.x];
        px = u32(inst.x) + li.x;
        py = u32(inst.y) + li.y;
        start = u32(inst.z);
        count = u32(inst.w);
    } else {
        px = wg.x * 16u + li.x;
        py = wg.y * 16u + li.y;
    }
    // The grid is rounded up to whole tiles, so the last row and column hang
    // off the canvas.
    if (px >= P.width || py >= P.height) { return; }
    let idx = py * P.width + px;

    if (P.mode == MODE_CLEAR) { layers[P.dst_base + idx] = 0u; return; }
    if (P.mode == MODE_CLIP_RESET) { clipmask[idx] = 1.0; return; }
    if (P.mode == MODE_CLIP_PATH) {
        // Intersection is a product accumulated in place, so nesting is one
        // more factor and needs no second mask.
        clipmask[idx] = clipmask[idx] * coverage(start, count, f32(px), f32(py));
        return;
    }

    let dst = unpack_px(layers[P.dst_base + idx]);
    if (P.mode == MODE_GROUP) {
        let src = unpack_px(layers[P.src_base + idx]);
        if (src.a <= 0.0) { return; }
        layers[P.dst_base + idx] = pack_px(composite(src * P.alpha, dst, 1.0, 0.0));
        return;
    }

    var cov = coverage(start, count, f32(px), f32(py));
    if (P.use_clip == 1u) { cov = cov * clipmask[idx]; }
    // The oracle skips the pixel entirely, so the dither must not reach it.
    if (cov <= 0.0) { return; }
    let src = paint_at(vec2<f32>(f32(px) + 0.5, f32(py) + 0.5));
    var noise = 0.0;
    if (P.kind != 0u) { noise = dither_at(px, py); }
    layers[P.dst_base + idx] = pack_px(composite(src, dst, cov, noise));
}
"#;

/// The whole module, with the one size the coverage loop is compiled around.
fn shader_source() -> String {
    format!(
        "const MAXC: u32 = {MAX_CROSSINGS}u;\n{HEAD_WGSL}{COVERAGE_WGSL}{PAINT_WGSL}{BLEND_WGSL}{MAIN_WGSL}"
    )
}

// ---------------------------------------------------------------------------
// the draw list, as plain data
// ---------------------------------------------------------------------------

/// One dispatch: its uniform block and how many workgroups run it.
///
/// Built without a device so the depth tracking and the block layout are
/// `cargo test`-able, which is the same instinct as D-010 one level down.
struct Job {
    params: [u32; PARAM_WORDS],
    groups: (u32, u32),
}

/// Turns the ordered command list into one dispatch each.
///
/// # Errors
///
/// If the scene's groups nest deeper than [`MAX_GROUP_DEPTH`]; the layer planes
/// are allocated up front, so a scene needing one more has to be turned away
/// before anything is drawn.
fn jobs(data: &DrawData) -> Result<Vec<Job>, JsValue> {
    group_depth(data)?;
    let plane = data.width * data.height;
    let full = (
        data.width.div_ceil(TILE_SIZE),
        data.height.div_ceil(TILE_SIZE),
    );
    let mut depth = 0u32;
    let mut out = Vec::with_capacity(data.ops.len());

    for op in &data.ops {
        let mut p = Params {
            width: data.width,
            height: data.height,
            dst_base: depth * plane,
            ..Params::default()
        };
        let groups = match op {
            Op::ClipReset => {
                p.mode = MODE_CLIP_RESET;
                full
            }
            Op::ClipPath { instances, rule } => {
                p.mode = MODE_CLIP_PATH;
                p.rule = rule_code(*rule) as u32;
                p.first_inst = instances.0;
                (instances.1, 1)
            }
            Op::Fill {
                instances,
                rule,
                blend,
                paint,
                clipped,
            } => {
                p.mode = MODE_FILL;
                p.rule = rule_code(*rule) as u32;
                p.blend = blend_code(*blend) as u32;
                p.use_clip = u32::from(*clipped);
                p.first_inst = instances.0;
                p.paint = **paint;
                (instances.1, 1)
            }
            Op::PushGroup => {
                depth += 1;
                p.mode = MODE_CLEAR;
                p.dst_base = depth * plane;
                full
            }
            Op::PopGroup { blend, alpha } => {
                p.mode = MODE_GROUP;
                p.blend = blend_code(*blend) as u32;
                p.alpha = *alpha;
                p.src_base = depth * plane;
                depth -= 1;
                p.dst_base = depth * plane;
                full
            }
        };
        out.push(Job {
            params: p.words(),
            groups,
        });
    }
    Ok(out)
}

/// Zeroed paint: what every mode but `MODE_FILL` carries, since those fields
/// are the only ones no other mode reads.
const NO_PAINT: PaintData = PaintData {
    kind: 0,
    spread: 0,
    g0: [0.0; 2],
    g1: [0.0; 2],
    radius: 0.0,
    stop_count: 0,
    offsets: [0.0; MAX_STOPS],
    colors: [0.0; MAX_STOPS * 4],
};

/// The uniform block, before it is flattened into words.
struct Params {
    mode: u32,
    rule: u32,
    blend: u32,
    use_clip: u32,
    alpha: f32,
    first_inst: u32,
    dst_base: u32,
    src_base: u32,
    width: u32,
    height: u32,
    paint: PaintData,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            mode: MODE_CLEAR,
            rule: 0,
            blend: 0,
            use_clip: 0,
            alpha: 0.0,
            first_inst: 0,
            dst_base: 0,
            src_base: 0,
            width: 0,
            height: 0,
            paint: NO_PAINT,
        }
    }
}

impl Params {
    /// The block as `u32` words, in the order `struct Params` declares in
    /// `HEAD_WGSL`. Floats go in by their bits: WGSL reads the same 32 bits as
    /// an `f32` because the field is declared one.
    fn words(&self) -> [u32; PARAM_WORDS] {
        let p = &self.paint;
        let mut w = [0u32; PARAM_WORDS];
        w[0] = p.g0[0].to_bits();
        w[1] = p.g0[1].to_bits();
        w[2] = p.g1[0].to_bits();
        w[3] = p.g1[1].to_bits();
        w[4] = p.radius.to_bits();
        w[5] = self.alpha.to_bits();
        w[6] = p.kind;
        w[7] = p.spread;
        w[8] = self.rule;
        w[9] = self.blend;
        w[10] = self.use_clip;
        w[11] = p.stop_count;
        w[12] = self.first_inst;
        w[13] = self.mode;
        w[14] = self.dst_base;
        w[15] = self.src_base;
        w[16] = self.width;
        w[17] = self.height;
        // 18, 19 are the pad that puts the arrays on a 16-byte boundary.
        for (i, v) in p.offsets.iter().enumerate() {
            w[20 + i] = v.to_bits();
        }
        for (i, v) in p.colors.iter().enumerate() {
            w[20 + MAX_STOPS + i] = v.to_bits();
        }
        w
    }
}

// ---------------------------------------------------------------------------
// the exports
// ---------------------------------------------------------------------------

/// One line about this machine's WebGPU, for the console and the diff page.
///
/// # Errors
///
/// If there is no `navigator.gpu`, no adapter, or no device -- the three ways a
/// browser says "not here", each of which is a report rather than a crash.
#[wasm_bindgen]
pub async fn hane_wgpu_probe() -> Result<String, JsValue> {
    let device = device().await?;
    let info = device.adapter_info();
    let (vendor, arch, desc) = (info.vendor(), info.architecture(), info.description());
    let who = [vendor, arch, desc]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let who = if who.is_empty() {
        String::from("adapter unnamed")
    } else {
        who
    };
    Ok(format!("hane webgpu: {who} -- ok"))
}

/// Which backend to render with: `"webgpu"` or `"webgl2"`.
///
/// `preferred` is the manual override. Empty or `"auto"` picks WebGPU when this
/// browser has a working adapter and falls back to WebGL2 when it does not,
/// which is the whole of the automatic rule -- WebGPU is the newer backend and
/// the one with the shorter submit path, so it wins whenever it exists.
///
/// # Errors
///
/// If `preferred` names a backend this browser cannot provide, or is not a
/// backend name at all. An override that silently did something else would be
/// worse than no override: a benchmark would report the wrong backend's number.
#[wasm_bindgen]
pub async fn hane_backend(preferred: String) -> Result<String, JsValue> {
    match preferred.as_str() {
        "" | "auto" => Ok(String::from(if device().await.is_ok() {
            "webgpu"
        } else {
            "webgl2"
        })),
        "webgpu" => device().await.map(|_| String::from("webgpu")),
        "webgl2" => Ok(String::from("webgl2")),
        other => Err(JsValue::from_str(&format!(
            "no such backend {other:?}; expected \"webgpu\", \"webgl2\" or \"auto\""
        ))),
    }
}

/// Renders fixture `index` on `backend` and hands back its pixels.
///
/// The same bytes as [`crate::glrender::hane_gl_render_fixture`]: two
/// little-endian `u16` of width and height, then premultiplied RGBA row-major
/// from the top left. One entry point for both backends is what lets the diff
/// harness compare them without knowing which one ran.
///
/// # Errors
///
/// If there is no such fixture, if `backend` is not a backend name, or if the
/// backend itself fails.
#[wasm_bindgen]
pub async fn hane_render_fixture(
    canvas: HtmlCanvasElement,
    index: usize,
    backend: String,
) -> Result<js_sys::Uint8Array, JsValue> {
    let bytes = match backend.as_str() {
        "webgl2" => crate::glrender::hane_gl_render_fixture(&canvas, index)?,
        "webgpu" => {
            let fixtures = corpus::fixtures();
            let fixture = fixtures
                .get(index)
                .ok_or_else(|| JsValue::from_str("no such fixture"))?;
            let mut pixels = render(&fixture.scene).await?;
            let (w, h) = (fixture.scene.width as u16, fixture.scene.height as u16);
            let mut out = Vec::with_capacity(pixels.len() + 4);
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.append(&mut pixels);
            out
        }
        other => {
            return Err(JsValue::from_str(&format!("no such backend {other:?}")));
        }
    };
    Ok(js_sys::Uint8Array::from(&bytes[..]))
}

// ---------------------------------------------------------------------------
// the device, and one render on it
// ---------------------------------------------------------------------------

thread_local! {
    /// The device and its pipeline, for the life of the page.
    ///
    /// Neither depends on the scene, and neither is cheap: `requestAdapter` and
    /// `requestDevice` are round trips to the browser's GPU process, and
    /// building the pipeline compiles the WGSL. Rebuilding both per render
    /// measured **twelve times** the corpus time in Firefox and two and a half
    /// in Chromium -- a benchmark of a compiler, not of a rasterizer. The GL
    /// backend keeps its context the same way (`glctx.rs`).
    ///
    /// ponytail: the GL backend still recompiles its three programs per render,
    /// so `BENCHMARKS.md` compares a cached pipeline against an uncached one.
    /// Caching those too is the same eight lines and belongs with the first
    /// real frame loop, which is where either of them stops being per-scene.
    static GPU: std::cell::RefCell<Option<(GpuDevice, GpuComputePipeline)>> =
        const { std::cell::RefCell::new(None) };
}

/// The device and pipeline, or the reason there are none. Cached; see [`GPU`].
async fn gpu() -> Result<(GpuDevice, GpuComputePipeline), JsValue> {
    if let Some(pair) = GPU.with(|g| g.borrow().clone()) {
        return Ok(pair);
    }
    let gpu = web_sys::window()
        .ok_or_else(|| JsValue::from_str("no window"))?
        .navigator()
        .gpu();
    if gpu.is_undefined() {
        return Err(JsValue::from_str("this browser has no navigator.gpu"));
    }
    let adapter = present(JsFuture::from(gpu.request_adapter()).await?)
        .ok_or_else(|| JsValue::from_str("no WebGPU adapter"))?;
    let device: GpuDevice = JsFuture::from(adapter.request_device()).await?;

    let module = device.create_shader_module(&GpuShaderModuleDescriptor::new(&shader_source()));
    let stage = GpuProgrammableStage::new(&module);
    stage.set_entry_point("main");
    let pipeline = device.create_compute_pipeline(
        &GpuComputePipelineDescriptor::new_with_gpu_auto_layout_mode(
            GpuAutoLayoutMode::Auto,
            &stage,
        ),
    );
    GPU.with(|g| *g.borrow_mut() = Some((device.clone(), pipeline.clone())));
    Ok((device, pipeline))
}

/// Just the device, for the probe.
async fn device() -> Result<GpuDevice, JsValue> {
    gpu().await.map(|(device, _)| device)
}

/// `Some` only when the promise resolved to a real object.
///
/// WebGPU says "no adapter" and "no error" with **null**, and a `JsOption`
/// counts only `undefined` as empty -- so without this the next call lands on
/// null, throws a `TypeError` inside the resumed future, and the whole render
/// hangs on a promise that will never settle. That failure is invisible: no
/// rejection, no message, just a page that never finishes.
fn present<T: wasm_bindgen::JsGeneric + AsRef<JsValue>>(v: js_sys::JsOption<T>) -> Option<T> {
    v.into_option().filter(|v| !v.as_ref().is_null())
}

/// Renders `scene` into a storage buffer and reads it back.
///
/// The buffers are built and dropped per scene, the same shape as the GL
/// submitter and for the same reason: this is a harness rendering forty-odd
/// independent pictures, not a frame loop. Only the device and the pipeline,
/// which no scene can change, outlive one render.
async fn render(scene: &Scene) -> Result<Vec<u8>, JsValue> {
    let data = DrawData::build(scene);
    let jobs = jobs(&data)?;
    let (device, pipeline) = gpu().await?;
    let queue = device.queue();

    // Validation errors are asynchronous and would otherwise arrive as a
    // console line nobody reads, behind a black picture that looks like a
    // renderer bug.
    device.push_error_scope(GpuErrorFilter::Validation);

    let pixels = (data.width as usize) * (data.height as usize);
    let levels = group_depth(&data)? + 1;
    let layer_bytes = pixels * 4 * levels;

    let edges = storage(&device, "edges", data.edges.len() * 4)?;
    let insts = storage(&device, "insts", data.instances.len() * 4)?;
    // The base canvas is plane 0, with the group levels stacked above it, so
    // the readback copies from offset 0. No clear: WebGPU zeroes a new buffer,
    // and zero is transparent premultiplied black -- which is exactly what the
    // GL path spends a `clear` on.
    let layers = device.create_buffer(&buffer_desc(
        "layers",
        layer_bytes,
        USAGE_STORAGE | USAGE_COPY_SRC,
    ))?;
    let clipmask = storage(&device, "clip", pixels * 4)?;
    let params = device.create_buffer(&buffer_desc(
        "params",
        jobs.len() * PARAM_STRIDE,
        USAGE_UNIFORM | USAGE_COPY_DST,
    ))?;
    let staging = device.create_buffer(&buffer_desc(
        "readback",
        pixels * 4,
        USAGE_MAP_READ | USAGE_COPY_DST,
    ))?;

    queue.write_buffer_with_u32_and_buffer_source(
        &edges,
        0,
        &js_sys::Float32Array::from(&data.edges[..]),
    )?;
    queue.write_buffer_with_u32_and_buffer_source(
        &insts,
        0,
        &js_sys::Float32Array::from(&data.instances[..]),
    )?;
    let mut words = Vec::with_capacity(jobs.len() * PARAM_WORDS);
    for job in &jobs {
        words.extend_from_slice(&job.params);
    }
    if !words.is_empty() {
        queue.write_buffer_with_u32_and_buffer_source(
            &params,
            0,
            &js_sys::Uint32Array::from(&words[..]),
        )?;
    }

    let layout = pipeline.get_bind_group_layout(0);

    let encoder = device.create_command_encoder();
    let pass = encoder.begin_compute_pass();
    pass.set_pipeline(&pipeline);
    for (i, job) in jobs.iter().enumerate() {
        // The whole picture is one buffer and one bind group per op, differing
        // only in which 512-byte slice of the uniform buffer they read.
        //
        // ponytail: a bind group per op. One bind group with a dynamic offset
        // would do, and is worth the extra descriptor plumbing when a frame has
        // thousands of fills rather than a fixture's dozen.
        let bind = bind_group(
            &device,
            &layout,
            &[&edges, &insts, &layers, &clipmask],
            &params,
            i,
        )?;
        pass.set_bind_group(0, Some(&bind));
        if job.groups.0 > 0 {
            pass.dispatch_workgroups_with_workgroup_count_y(job.groups.0, job.groups.1);
        }
    }
    pass.end();
    encoder.copy_buffer_to_buffer_with_u32_and_u32_and_u32(
        &layers,
        0,
        &staging,
        0,
        (pixels * 4) as u32,
    )?;
    queue.submit(&[encoder.finish()]);

    JsFuture::from(staging.map_async_with_u32_and_u32(MAP_READ, 0, (pixels * 4) as u32)).await?;
    let range = staging.get_mapped_range_with_u32_and_u32(0, (pixels * 4) as u32)?;
    let out = js_sys::Uint8Array::new(range.as_ref()).to_vec();
    staging.unmap();

    if let Some(err) = present(JsFuture::from(device.pop_error_scope()).await?) {
        return Err(JsValue::from_str(&format!("webgpu: {}", err.message())));
    }
    Ok(out)
}

/// A buffer descriptor, never smaller than one 16-byte element.
///
/// WebGPU rejects a zero-sized buffer, and an empty edge list is a real scene:
/// `degenerate_empty` has no edges at all.
fn buffer_desc(label: &str, bytes: usize, usage: u32) -> GpuBufferDescriptor {
    let d = GpuBufferDescriptor::new(bytes.max(16) as u32, usage);
    d.set_label(label);
    d
}

fn storage(device: &GpuDevice, label: &str, bytes: usize) -> Result<GpuBuffer, JsValue> {
    device.create_buffer(&buffer_desc(label, bytes, USAGE_STORAGE | USAGE_COPY_DST))
}

/// The four whole-buffer bindings plus op `i`'s slice of the uniform buffer.
fn bind_group(
    device: &GpuDevice,
    layout: &web_sys::GpuBindGroupLayout,
    buffers: &[&GpuBuffer; 4],
    params: &GpuBuffer,
    i: usize,
) -> Result<GpuBindGroup, JsValue> {
    let slot = GpuBufferBinding::new(params);
    slot.set_offset((i * PARAM_STRIDE) as u32);
    slot.set_size(PARAM_STRIDE as u32);
    let entries: Vec<GpuBindGroupEntry> = buffers
        .iter()
        .enumerate()
        .map(|(b, buf)| {
            GpuBindGroupEntry::new_with_gpu_buffer_binding(b as u32, &GpuBufferBinding::new(buf))
        })
        .chain(std::iter::once(
            GpuBindGroupEntry::new_with_gpu_buffer_binding(4, &slot),
        ))
        .collect();
    Ok(device.create_bind_group(&GpuBindGroupDescriptor::new(&entries, layout)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::{PathEl, Point};
    use hane_raster::{BlendMode, Color, Draw, FillRule, Node, Paint};

    const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<PathEl> {
        vec![
            PathEl::MoveTo(Point::new(x0, y0)),
            PathEl::LineTo(Point::new(x1, y0)),
            PathEl::LineTo(Point::new(x1, y1)),
            PathEl::LineTo(Point::new(x0, y1)),
            PathEl::ClosePath,
        ]
    }

    fn word(job: &Job, i: usize) -> u32 {
        job.params[i]
    }

    /// The mode numbers in Rust and the `MODE_` constants in the shader are two
    /// lists that must stay identical in two languages.
    #[test]
    fn modes_agree_with_the_shader() {
        for (name, code) in [
            ("MODE_CLEAR", MODE_CLEAR),
            ("MODE_CLIP_RESET", MODE_CLIP_RESET),
            ("MODE_CLIP_PATH", MODE_CLIP_PATH),
            ("MODE_FILL", MODE_FILL),
            ("MODE_GROUP", MODE_GROUP),
        ] {
            assert!(
                MAIN_WGSL.contains(&format!("const {name}: u32 = {code}u;")),
                "{name} is {code} in Rust and something else in the shader"
            );
        }
    }

    /// Every blend mode past Normal needs an arm in `blend_rgb`, or it silently
    /// renders as Normal -- the same check `glrender.rs` makes of its GLSL.
    #[test]
    fn every_blend_mode_has_a_shader_arm() {
        for i in 1..16 {
            assert!(
                BLEND_WGSL.contains(&format!("P.blend == {i}u)")),
                "no shader arm for blend code {i}"
            );
        }
    }

    /// The block the shader declares and the words Rust writes are one layout
    /// described twice. This pins the half that Rust owns.
    #[test]
    fn the_uniform_block_lands_where_the_shader_reads_it() {
        let mut sc = Scene::new(32, 32);
        sc.push(Draw {
            path: rect(0.0, 0.0, 8.0, 8.0),
            paint: Paint::Solid(WHITE),
            rule: FillRule::EvenOdd,
            blend: BlendMode::Screen,
            clip: Vec::new(),
        });
        let jobs = jobs(&DrawData::build(&sc)).expect("one fill");
        let job = &jobs[0];
        assert_eq!(word(job, 8), 1, "rule: even-odd is 1");
        assert_eq!(word(job, 9), 2, "blend: screen is 2");
        assert_eq!(word(job, 13), MODE_FILL);
        assert_eq!(word(job, 16), 32, "width");
        assert_eq!(word(job, 17), 32, "height");
        assert_eq!(word(job, 11), 1, "a solid colour is one stop");
        // The premultiplied white the shader paints with, at the head of the
        // colour array -- word 36, right after the four vec4s of offsets.
        assert_eq!(f32::from_bits(word(job, 36)), 255.0);
        assert_eq!(job.groups, (1, 1), "one tile of edges");
    }

    /// A group is a clear of the level above, the members, and a composite back
    /// down. The two plane bases are what keep the layers apart in the one
    /// buffer, and getting them the wrong way round composites a layer onto
    /// itself.
    #[test]
    fn a_group_clears_a_plane_and_composites_it_back() {
        let mut sc = Scene::new(16, 16);
        sc.push_group(
            vec![Node::Fill(Draw {
                path: rect(0.0, 0.0, 8.0, 8.0),
                paint: Paint::Solid(WHITE),
                rule: FillRule::NonZero,
                blend: BlendMode::Normal,
                clip: Vec::new(),
            })],
            BlendMode::Multiply,
            0.5,
        );
        let jobs = jobs(&DrawData::build(&sc)).expect("push, fill, pop");
        assert_eq!(jobs.len(), 3);
        let plane = 16 * 16;
        assert_eq!(word(&jobs[0], 13), MODE_CLEAR);
        assert_eq!(
            word(&jobs[0], 14),
            plane,
            "clears the layer, not the canvas"
        );
        assert_eq!(word(&jobs[1], 14), plane, "the fill goes into the layer");
        assert_eq!(word(&jobs[2], 13), MODE_GROUP);
        assert_eq!(word(&jobs[2], 15), plane, "source is the finished layer");
        assert_eq!(word(&jobs[2], 14), 0, "destination is its parent");
        assert_eq!(f32::from_bits(word(&jobs[2], 5)), 0.5, "group alpha");
        assert_eq!(word(&jobs[2], 9), 1, "multiply is 1");
    }

    /// A clip is reset over the whole canvas and then multiplied in over every
    /// tile, both of which are full-grid dispatches -- a tile the clip path
    /// misses has to come out zero, not be left alone.
    #[test]
    fn a_clip_covers_the_whole_grid() {
        let mut sc = Scene::new(64, 32);
        sc.push(Draw {
            path: rect(0.0, 0.0, 64.0, 32.0),
            paint: Paint::Solid(WHITE),
            rule: FillRule::NonZero,
            blend: BlendMode::Normal,
            clip: vec![(rect(2.0, 2.0, 6.0, 6.0), FillRule::NonZero)],
        });
        let jobs = jobs(&DrawData::build(&sc)).expect("reset, clip, fill");
        assert_eq!(word(&jobs[0], 13), MODE_CLIP_RESET);
        assert_eq!(jobs[0].groups, (4, 2), "4x2 tiles of 16");
        assert_eq!(word(&jobs[1], 13), MODE_CLIP_PATH);
        assert_eq!(jobs[1].groups, (8, 1), "one workgroup per tile instance");
        assert_eq!(word(&jobs[2], 10), 1, "the fill reads the mask");
    }

    /// The shader is assembled, not written out whole: if the coverage loop's
    /// bound stopped being spliced in, `MAXC` would be undeclared and every
    /// render would fail to compile in the browser, where nothing here would
    /// notice.
    #[test]
    fn the_shader_declares_the_crossing_bound() {
        let src = shader_source();
        assert!(src.starts_with(&format!("const MAXC: u32 = {MAX_CROSSINGS}u;")));
        assert!(src.contains("array<f32, MAXC>"));
        assert!(src.contains("@workgroup_size(16, 16, 1)"));
    }
}
