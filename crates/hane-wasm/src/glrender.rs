//! The WebGL2 submitter: it holds the context and executes what `hane-gpu`
//! decided (#19, #21, #22, #23, D-010).
//!
//! Nothing here chooses anything. `hane_gpu::DrawData` has already flattened
//! the paths, binned them into tiles, narrowed the coordinates to `f32` and
//! ordered the commands; this file creates the GL objects, uploads those bytes
//! and issues the draws. The split is what lets the part that contains the bugs
//! be tested with `cargo test`.
//!
//! # The shader is the oracle's inner loop
//!
//! `render.rs` argues why. In one line: the fragment shader walks the same
//! sixteen sample lines per pixel, intersects the same edges, sorts the
//! crossings that fall inside that pixel and sums the same analytic spans, so
//! coverage is not an approximation of the oracle's answer but the same
//! computation in `f32`.
//!
//! # Why every draw ping-pongs
//!
//! WebGL2 has no framebuffer fetch, so a shader cannot read the pixel it is
//! about to write -- and every blend mode past source-over needs exactly that.
//! Rather than keep two paths, one using fixed-function blending and one not,
//! each draw blits the target aside and reads the copy. Fixed-function blending
//! is then switched off entirely and the composite is written out in the
//! shader, which is what makes the rounding and the ordered dither the same
//! arithmetic the CPU does rather than merely a close one.
//!
//! ponytail: that is a full-target copy per draw. It is the right shape for a
//! diff harness and the wrong one for a frame with a thousand fills; the
//! upgrade is to keep fixed-function `ONE, ONE_MINUS_SRC_ALPHA` for `Normal`
//! and ping-pong only the draws that actually read the backdrop.

use crate::glctx;
use hane_gpu::{DrawData, EDGE_TEX_WIDTH, Op, PaintData};
use hane_raster::{BlendMode, FillRule, Scene, corpus};
use wasm_bindgen::prelude::*;
use web_sys::{
    HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlBuffer, WebGlFramebuffer, WebGlProgram,
    WebGlTexture, WebGlUniformLocation,
};

/// How deep isolated groups may nest.
///
/// Each level costs two full-canvas RGBA8 targets -- the layer and the copy its
/// own draws read -- so the bound is memory, not correctness. Four is past what
/// an SVG in the corpus reaches and keeps a 4K canvas under 270 MB of targets.
pub const MAX_GROUP_DEPTH: usize = 4;

/// The most crossings one pixel may have on one sample line.
///
/// Twelve edges meeting inside a single pixel on a single sample line is a
/// degenerate pile-up, not a picture; the corpus's worst case is `pentagram`,
/// at two. Overflow drops the extra crossings, which is wrong -- but it is
/// wrong in one pixel of a shape that is already unrepresentable, and the
/// alternative is an unbounded array in a fragment shader.
///
/// ponytail: silent. If a real document ever hits it, the fix is to count the
/// crossings in a first pass and fall back to a second draw for the tiles that
/// overflowed.
const MAX_CROSSINGS: usize = 12;

// ---------------------------------------------------------------------------
// the shaders
// ---------------------------------------------------------------------------

/// The tile quad, in pixels. One instance per tile, four corners each.
const VERT: &str = r#"#version 300 es
in vec2 aCorner;
in vec4 aInst;
uniform vec2 uSize;
flat out vec2 vRange;
void main() {
    vec2 p = aInst.xy + aCorner * 16.0;
    vRange = aInst.zw;
    // y is flipped: the pixmap's row 0 is the top, GL's is the bottom. The
    // readback flips it again, so nothing downstream sees the difference.
    gl_Position = vec4(p.x / uSize.x * 2.0 - 1.0, 1.0 - p.y / uSize.y * 2.0, 0.0, 1.0);
}
"#;

/// A full-screen triangle, for compositing a finished group.
const VERT_FULL: &str = r#"#version 300 es
void main() {
    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// The coverage loop: the oracle's `coverage_row`, one pixel at a time.
const COVERAGE_GLSL: &str = r#"
flat in vec2 vRange;
uniform sampler2D uEdges;
uniform int uRule;

bool inside(int w) {
    return uRule == 0 ? w != 0 : (w % 2) != 0;
}

float coverage(float px, float py) {
    int start = int(vRange.x);
    int count = int(vRange.y);
    float xs[MAXC];
    int ds[MAXC];
    float cov = 0.0;
    for (int s = 0; s < 16; s++) {
        // The sub-band midpoint, exactly as the oracle picks it.
        float sy = py + (float(s) + 0.5) * 0.0625;
        int w0 = 0;
        int n = 0;
        for (int e = 0; e < count; e++) {
            int idx = start + e;
            vec4 ed = texelFetch(uEdges, ivec2(idx % EDGEW, idx / EDGEW), 0);
            vec2 top, bot;
            int dir;
            if (ed.y < ed.w) { top = ed.xy; bot = ed.zw; dir = 1; }
            else             { top = ed.zw; bot = ed.xy; dir = -1; }
            // Half-open in y: a vertex shared by two edges crosses once.
            if (sy < top.y || sy >= bot.y) continue;
            float t = (sy - top.y) / (bot.y - top.y);
            float xc = (1.0 - t) * top.x + t * bot.x;
            if (xc < px) {
                // Left of this pixel: it only moves the winding number we
                // start from. This is why a tile needs the edges to its left.
                w0 += dir;
            } else if (xc < px + 1.0 && n < MAXC) {
                xs[n] = xc;
                ds[n] = dir;
                n++;
            }
        }
        // Insertion sort: n is 0 or 1 for almost every pixel of a real path,
        // and a comparison sort of twelve is not worth a network.
        for (int i = 1; i < n; i++) {
            float kx = xs[i];
            int kd = ds[i];
            int j = i - 1;
            while (j >= 0 && xs[j] > kx) {
                xs[j + 1] = xs[j];
                ds[j + 1] = ds[j];
                j--;
            }
            xs[j + 1] = kx;
            ds[j + 1] = kd;
        }
        int w = w0;
        float prev = px;
        for (int i = 0; i < n; i++) {
            if (inside(w)) cov += (xs[i] - prev) * 0.0625;
            prev = xs[i];
            w += ds[i];
        }
        if (inside(w)) cov += (px + 1.0 - prev) * 0.0625;
    }
    return clamp(cov, 0.0, 1.0);
}
"#;

/// Paint evaluation and the ordered dither, mirroring `hane-raster::paint`.
const PAINT_GLSL: &str = r#"
uniform int uKind;
uniform int uSpread;
uniform vec2 uG0;
uniform vec2 uG1;
uniform float uRadius;
uniform int uStopCount;
uniform float uStopOffset[16];
uniform vec4 uStopColor[16];

const int BAYER[64] = int[64](
     0, 32,  8, 40,  2, 34, 10, 42,
    48, 16, 56, 24, 50, 18, 58, 26,
    12, 44,  4, 36, 14, 46,  6, 38,
    60, 28, 52, 20, 62, 30, 54, 22,
     3, 35, 11, 43,  1, 33,  9, 41,
    51, 19, 59, 27, 49, 17, 57, 25,
    15, 47,  7, 39, 13, 45,  5, 37,
    63, 31, 55, 23, 61, 29, 53, 21);

float ditherAt(int x, int y) {
    return (float(BAYER[(y % 8) * 8 + (x % 8)]) + 0.5) / 64.0 - 0.5;
}

float spreadMap(float t) {
    // A degenerate geometry hands over a non-finite parameter; NaN is not
    // greater than zero, so it lands on the start stop.
    if (isinf(t) || isnan(t)) return t > 0.0 ? 1.0 : 0.0;
    if (uSpread == 0) return clamp(t, 0.0, 1.0);
    if (uSpread == 1) return t - floor(t);
    float h = t * 0.5;
    float r = (h - floor(h)) * 2.0;
    return r > 1.0 ? 2.0 - r : r;
}

vec4 rampAt(float t) {
    if (uStopCount == 0) return vec4(0.0);
    int last = uStopCount - 1;
    if (t <= uStopOffset[0]) return uStopColor[0];
    if (t >= uStopOffset[last]) return uStopColor[last];
    for (int i = 0; i < last; i++) {
        if (t < uStopOffset[i + 1]) {
            float a = uStopOffset[i];
            float b = uStopOffset[i + 1];
            if (b <= a) return uStopColor[i + 1];
            float u = (t - a) / (b - a);
            return (1.0 - u) * uStopColor[i] + u * uStopColor[i + 1];
        }
    }
    return uStopColor[last];
}

float radialT(vec2 p) {
    if (isnan(uRadius) || uRadius <= 0.0) return 1.0;
    vec2 f = uG1;
    vec2 off = uG1 - uG0;
    float dist = length(off);
    // A focus on the rim collapses the whole gradient to a point, so SVG's
    // "pull it inside" is taken a hair short of the boundary.
    if (dist > uRadius * 0.999) f = uG0 + off * (uRadius * 0.999 / dist);
    vec2 u = p - f;
    float a = dot(u, u);
    if (a == 0.0) return 0.0;
    vec2 e = f - uG0;
    float b = dot(e, u);
    float c = dot(e, e) - uRadius * uRadius;
    float k = (-b + sqrt(b * b - a * c)) / a;
    return 1.0 / k;
}

vec4 paintAt(vec2 p) {
    if (uKind == 0) return uStopColor[0];
    float t;
    if (uKind == 1) {
        vec2 axis = uG1 - uG0;
        float len2 = dot(axis, axis);
        t = len2 > 0.0 ? dot(p - uG0, axis) / len2 : 1.0;
    } else {
        t = radialT(p);
    }
    return rampAt(spreadMap(t));
}
"#;

/// The blend functions and the composite around them, mirroring
/// `hane-raster::fill`.
const BLEND_GLSL: &str = r#"
uniform int uBlend;

float screenB(float s, float b) { return s + b - s * b; }

float hardLight(float s, float b) {
    return s <= 0.5 ? 2.0 * s * b : screenB(2.0 * s - 1.0, b);
}

float softLight(float s, float b) {
    float d = b <= 0.25 ? ((16.0 * b - 12.0) * b + 4.0) * b : sqrt(b);
    return s <= 0.5 ? b - (1.0 - 2.0 * s) * b * (1.0 - b) : b + (2.0 * s - 1.0) * (d - b);
}

float dodge(float s, float b) {
    if (b <= 0.0) return 0.0;
    if (s >= 1.0) return 1.0;
    return min(b / (1.0 - s), 1.0);
}

float burn(float s, float b) {
    if (b >= 1.0) return 1.0;
    if (s <= 0.0) return 0.0;
    return 1.0 - min((1.0 - b) / s, 1.0);
}

float lumOf(vec3 c) { return 0.3 * c.r + 0.59 * c.g + 0.11 * c.b; }

vec3 clipColor(vec3 c) {
    float l = lumOf(c);
    float n = min(c.r, min(c.g, c.b));
    float x = max(c.r, max(c.g, c.b));
    if (n < 0.0 && l > n) c = l + (c - l) * l / (l - n);
    if (x > 1.0 && x > l) c = l + (c - l) * (1.0 - l) / (x - l);
    return c;
}

vec3 setLum(vec3 c, float l) { return clipColor(c + (l - lumOf(c))); }

float satOf(vec3 c) {
    return max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b));
}

vec3 setSat(vec3 c, float s) {
    float n = min(c.r, min(c.g, c.b));
    float x = max(c.r, max(c.g, c.b));
    if (x <= n) return vec3(0.0);
    return (c - n) * s / (x - n);
}

vec3 blendRGB(vec3 cs, vec3 cb) {
    if (uBlend == 1) return cs * cb;
    if (uBlend == 2) return vec3(screenB(cs.r, cb.r), screenB(cs.g, cb.g), screenB(cs.b, cb.b));
    if (uBlend == 3) return vec3(hardLight(cb.r, cs.r), hardLight(cb.g, cs.g), hardLight(cb.b, cs.b));
    if (uBlend == 4) return min(cs, cb);
    if (uBlend == 5) return max(cs, cb);
    if (uBlend == 6) return vec3(dodge(cs.r, cb.r), dodge(cs.g, cb.g), dodge(cs.b, cb.b));
    if (uBlend == 7) return vec3(burn(cs.r, cb.r), burn(cs.g, cb.g), burn(cs.b, cb.b));
    if (uBlend == 8) return vec3(hardLight(cs.r, cb.r), hardLight(cs.g, cb.g), hardLight(cs.b, cb.b));
    if (uBlend == 9) return vec3(softLight(cs.r, cb.r), softLight(cs.g, cb.g), softLight(cs.b, cb.b));
    if (uBlend == 10) return abs(cb - cs);
    if (uBlend == 11) return cs + cb - 2.0 * cs * cb;
    if (uBlend == 12) return setLum(setSat(cs, satOf(cb)), lumOf(cb));
    if (uBlend == 13) return setLum(setSat(cb, satOf(cs)), lumOf(cb));
    if (uBlend == 14) return setLum(cs, lumOf(cb));
    if (uBlend == 15) return setLum(cb, lumOf(cs));
    return cs;
}

/// `src` and `dst` are premultiplied on the 0..255 scale, `dst` snapped to the
/// integers it came from so the rounding below lands where the CPU's does.
vec4 composite(vec4 src, vec4 dst, float cov, float noise) {
    if (uBlend != 0 && dst.a > 0.0 && src.a > 0.0) {
        vec3 cs = src.rgb / src.a;
        vec3 cb = dst.rgb / dst.a;
        float ab = dst.a / 255.0;
        src.rgb = clamp(mix(cs, blendRGB(cs, cb), ab), 0.0, 1.0) * src.a;
    }
    float inv = 1.0 - src.a * cov / 255.0;
    // `floor(v + 0.5)` is Rust's `.round()` for a non-negative v, and the
    // divide lands on an exact unorm8 value so the framebuffer stores it whole.
    return clamp(floor(src * cov + dst * inv + noise + 0.5), 0.0, 255.0) / 255.0;
}
"#;

/// Reads the copy of the target this draw is compositing onto.
const DST_GLSL: &str = r#"
uniform sampler2D uDst;
vec4 dstAt() {
    return floor(texelFetch(uDst, ivec2(gl_FragCoord.xy), 0) * 255.0 + 0.5);
}
"#;

/// The fill pass: coverage, clip, paint, composite.
const FRAG_FILL: &str = r#"
uniform vec2 uSize;
uniform sampler2D uClip;
uniform int uUseClip;
out vec4 fragColor;

void main() {
    // The pixmap's pixel indices. gl_FragCoord is at the pixel centre and y
    // runs the other way.
    float px = floor(gl_FragCoord.x);
    float py = floor(uSize.y - gl_FragCoord.y);
    vec4 dst = dstAt();

    float cov = coverage(px, py);
    if (uUseClip == 1) cov *= texelFetch(uClip, ivec2(gl_FragCoord.xy), 0).r;
    if (cov <= 0.0) {
        // The oracle skips the pixel entirely, so the dither must not reach it.
        fragColor = dst / 255.0;
        return;
    }

    vec4 src = paintAt(vec2(px + 0.5, py + 0.5));
    float noise = uKind == 0 ? 0.0 : ditherAt(int(px), int(py));
    fragColor = composite(src, dst, cov, noise);
}
"#;

/// The clip pass: coverage alone, multiplied into the mask by the blend unit.
const FRAG_MASK: &str = r#"
uniform vec2 uSize;
out vec4 fragColor;

void main() {
    float px = floor(gl_FragCoord.x);
    float py = floor(uSize.y - gl_FragCoord.y);
    fragColor = vec4(coverage(px, py));
}
"#;

/// The group pass: one finished layer composited onto its parent.
const FRAG_GROUP: &str = r#"
uniform sampler2D uSrc;
uniform float uAlpha;
out vec4 fragColor;

void main() {
    vec4 dst = dstAt();
    vec4 src = floor(texelFetch(uSrc, ivec2(gl_FragCoord.xy), 0) * 255.0 + 0.5);
    if (src.a <= 0.0) {
        fragColor = dst / 255.0;
        return;
    }
    fragColor = composite(src * uAlpha, dst, 1.0, 0.0);
}
"#;

/// The header every fragment shader shares: precision, and the two sizes the
/// coverage loop is compiled around.
fn frag_header() -> String {
    format!(
        "#version 300 es\nprecision highp float;\nprecision highp int;\n\
         precision highp sampler2D;\nconst int MAXC = {MAX_CROSSINGS};\n\
         const int EDGEW = {EDGE_TEX_WIDTH};\n"
    )
}

// ---------------------------------------------------------------------------
// the exports
// ---------------------------------------------------------------------------

/// How many fixtures the corpus holds.
#[wasm_bindgen]
pub fn hane_fixture_count() -> usize {
    corpus::fixtures().len()
}

/// The name of fixture `index`, or an empty string if there is no such fixture.
#[wasm_bindgen]
pub fn hane_fixture_name(index: usize) -> String {
    corpus::fixtures()
        .get(index)
        .map_or_else(String::new, |f| f.name.to_string())
}

/// Renders fixture `index` on the GPU and hands back its pixels.
///
/// Premultiplied RGBA, row-major from the top left -- the `Pixmap` contract, so
/// the bytes can be diffed against a golden without a conversion that would
/// lose precision at low alpha and hide the difference the diff is for.
///
/// The four bytes of the header are the width and height, little-endian `u16`
/// each, so a caller that has not asked the corpus for the size still knows
/// what shape the buffer is.
#[wasm_bindgen]
pub fn hane_gl_render_fixture(
    canvas: &HtmlCanvasElement,
    index: usize,
) -> Result<Vec<u8>, JsValue> {
    let fixtures = corpus::fixtures();
    let fixture = fixtures
        .get(index)
        .ok_or_else(|| JsValue::from_str("no such fixture"))?;
    let mut pixels = render(canvas, &fixture.scene)?;
    let (w, h) = (fixture.scene.width as u16, fixture.scene.height as u16);
    let mut out = Vec::with_capacity(pixels.len() + 4);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.append(&mut pixels);
    Ok(out)
}

/// Renders `scene` into an offscreen target and reads it back.
fn render(canvas: &HtmlCanvasElement, scene: &Scene) -> Result<Vec<u8>, JsValue> {
    let gl = glctx::context(canvas)?;
    // Renderable float targets are what the clip mask is. The probe reports
    // this; asking again is idempotent and keeps the renderer honest when it is
    // driven without the probe having run.
    if gl.get_extension("EXT_color_buffer_float")?.is_none() {
        return Err(JsValue::from_str(
            "EXT_color_buffer_float is required for the clip mask",
        ));
    }
    let data = DrawData::build(scene);
    let gpu = Gpu::new(&gl, &data)?;
    gpu.run(&data)?;
    gpu.read_back(&data)
}

// ---------------------------------------------------------------------------
// the GL objects
// ---------------------------------------------------------------------------

/// One render's worth of GL state.
///
/// Built and dropped per scene. That is the wrong shape for a frame loop and
/// the right one for a harness that renders forty-odd independent pictures at
/// four different sizes; P3's cache is where reuse belongs.
struct Gpu<'a> {
    gl: &'a Gl,
    fill: Program,
    mask: Program,
    group: Program,
    instances: WebGlBuffer,
    /// Layer targets, one per nesting level plus the base, and the scratch copy
    /// each level's draws read from.
    layers: Vec<Target>,
    scratch: Vec<Target>,
    clip: Target,
    size: (i32, i32),
    depth: std::cell::Cell<usize>,
}

/// A colour texture and the framebuffer that renders into it.
struct Target {
    texture: WebGlTexture,
    fbo: WebGlFramebuffer,
}

/// A compiled program and the uniform locations it is driven through.
struct Program {
    program: WebGlProgram,
    u: Vec<(&'static str, Option<WebGlUniformLocation>)>,
}

impl Program {
    fn loc(&self, name: &str) -> Option<&WebGlUniformLocation> {
        self.u
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, l)| l.as_ref())
    }
}

/// The tile quad corner, and the per-tile instance. Bound before linking so
/// every program agrees, since attribute pointers are global state.
const ATTR_CORNER: u32 = 0;
/// See [`ATTR_CORNER`].
const ATTR_INST: u32 = 1;

const UNIFORMS: &[&str] = &[
    "uSize",
    "uEdges",
    "uDst",
    "uClip",
    "uSrc",
    "uUseClip",
    "uRule",
    "uBlend",
    "uKind",
    "uSpread",
    "uG0",
    "uG1",
    "uRadius",
    "uAlpha",
    "uStopCount",
    "uStopOffset[0]",
    "uStopColor[0]",
];

impl<'a> Gpu<'a> {
    fn new(gl: &'a Gl, data: &DrawData) -> Result<Self, JsValue> {
        let head = frag_header();
        let fill = program(
            gl,
            VERT,
            &format!("{head}{COVERAGE_GLSL}{PAINT_GLSL}{BLEND_GLSL}{DST_GLSL}{FRAG_FILL}"),
        )?;
        let mask = program(gl, VERT, &format!("{head}{COVERAGE_GLSL}{FRAG_MASK}"))?;
        let group = program(
            gl,
            VERT_FULL,
            &format!("{head}{BLEND_GLSL}{DST_GLSL}{FRAG_GROUP}"),
        )?;

        let (w, h) = (data.width as i32, data.height as i32);
        // One layer per nesting level the scene actually reaches, and no more.
        let levels = group_depth(data)? + 1;
        let mut layers = Vec::with_capacity(levels);
        let mut scratch = Vec::with_capacity(levels);
        for _ in 0..levels {
            layers.push(make_target(gl, w, h, Gl::RGBA8)?);
            scratch.push(make_target(gl, w, h, Gl::RGBA8)?);
        }
        let clip = make_target(gl, w, h, Gl::R16F)?;

        let gpu = Self {
            gl,
            fill,
            mask,
            group,
            instances: gl
                .create_buffer()
                .ok_or_else(|| JsValue::from_str("no instance buffer"))?,
            layers,
            scratch,
            clip,
            size: (w, h),
            depth: std::cell::Cell::new(0),
        };

        // The unit quad, four corners in a strip, shared by every tile.
        let quad = gl
            .create_buffer()
            .ok_or_else(|| JsValue::from_str("no quad buffer"))?;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&quad));
        upload_f32(gl, &[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
        // Locations are bound before linking, so `aCorner` is 0 and `aInst` is
        // 1 in every program -- otherwise the pointer one program set would be
        // silently pointing at the other's buffer.
        gl.enable_vertex_attrib_array(ATTR_CORNER);
        gl.vertex_attrib_pointer_with_i32(ATTR_CORNER, 2, Gl::FLOAT, false, 0, 0);

        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&gpu.instances));
        upload_f32(gl, &data.instances);

        // The edge texture: one RGBA32F texel per edge.
        let (ew, eh) = data.edge_texture_size();
        let edges = gl
            .create_texture()
            .ok_or_else(|| JsValue::from_str("no edge texture"))?;
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&edges));
        nearest(gl);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_array_buffer_view(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA32F as i32,
            ew.max(1) as i32,
            eh.max(1) as i32,
            0,
            Gl::RGBA,
            Gl::FLOAT,
            Some(&js_sys::Float32Array::from(&data.edges[..])),
        )?;

        gl.disable(Gl::DEPTH_TEST);
        gl.viewport(0, 0, w, h);
        // The canvas is never presented -- everything happens in the offscreen
        // targets -- but a zero-sized drawing buffer upsets some drivers.
        gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&gpu.layers[0].fbo));
        gl.disable(Gl::BLEND);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        Ok(gpu)
    }

    fn run(&self, data: &DrawData) -> Result<(), JsValue> {
        let gl = self.gl;
        let size = (data.width as f32, data.height as f32);
        for op in &data.ops {
            match op {
                Op::ClipReset => {
                    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.clip.fbo));
                    gl.disable(Gl::BLEND);
                    gl.clear_color(1.0, 1.0, 1.0, 1.0);
                    gl.clear(Gl::COLOR_BUFFER_BIT);
                }
                Op::ClipPath { instances, rule } => {
                    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.clip.fbo));
                    gl.use_program(Some(&self.mask.program));
                    gl.uniform2f(self.mask.loc("uSize"), size.0, size.1);
                    gl.uniform1i(self.mask.loc("uEdges"), 0);
                    gl.uniform1i(self.mask.loc("uRule"), rule_code(*rule));
                    // dst = src * dst: intersection is a product, so nesting is
                    // one more factor and needs no second mask.
                    gl.enable(Gl::BLEND);
                    gl.blend_func(Gl::ZERO, Gl::SRC_COLOR);
                    self.draw_tiles(*instances);
                    gl.disable(Gl::BLEND);
                }
                Op::Fill {
                    instances,
                    rule,
                    blend,
                    paint,
                    clipped,
                } => {
                    let level = self.depth.get();
                    self.copy_aside(level);
                    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.layers[level].fbo));
                    gl.use_program(Some(&self.fill.program));
                    gl.uniform2f(self.fill.loc("uSize"), size.0, size.1);
                    gl.uniform1i(self.fill.loc("uEdges"), 0);
                    gl.uniform1i(self.fill.loc("uDst"), 1);
                    gl.uniform1i(self.fill.loc("uClip"), 2);
                    gl.uniform1i(self.fill.loc("uUseClip"), i32::from(*clipped));
                    gl.uniform1i(self.fill.loc("uRule"), rule_code(*rule));
                    gl.uniform1i(self.fill.loc("uBlend"), blend_code(*blend));
                    self.set_paint(paint);
                    gl.active_texture(Gl::TEXTURE1);
                    gl.bind_texture(Gl::TEXTURE_2D, Some(&self.scratch[level].texture));
                    gl.active_texture(Gl::TEXTURE2);
                    gl.bind_texture(Gl::TEXTURE_2D, Some(&self.clip.texture));
                    self.draw_tiles(*instances);
                }
                Op::PushGroup => {
                    let level = self.depth.get() + 1;
                    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.layers[level].fbo));
                    gl.disable(Gl::BLEND);
                    gl.clear_color(0.0, 0.0, 0.0, 0.0);
                    gl.clear(Gl::COLOR_BUFFER_BIT);
                    self.depth.set(level);
                }
                Op::PopGroup { blend, alpha } => {
                    let level = self.depth.get();
                    self.depth.set(level - 1);
                    let parent = level - 1;
                    self.copy_aside(parent);
                    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.layers[parent].fbo));
                    gl.use_program(Some(&self.group.program));
                    // Unit 3, not 0: the edge texture owns unit 0 for the life
                    // of the render, and rebinding it here would leave every
                    // later fill sampling a layer as its edge list.
                    gl.uniform1i(self.group.loc("uSrc"), 3);
                    gl.uniform1i(self.group.loc("uDst"), 1);
                    gl.uniform1i(self.group.loc("uBlend"), blend_code(*blend));
                    gl.uniform1f(self.group.loc("uAlpha"), *alpha);
                    gl.active_texture(Gl::TEXTURE3);
                    gl.bind_texture(Gl::TEXTURE_2D, Some(&self.layers[level].texture));
                    gl.active_texture(Gl::TEXTURE1);
                    gl.bind_texture(Gl::TEXTURE_2D, Some(&self.scratch[parent].texture));
                    gl.draw_arrays(Gl::TRIANGLES, 0, 3);
                }
            }
        }
        Ok(())
    }

    /// Blits layer `level` into its scratch copy, which is what the next draw
    /// reads as its backdrop. See the module docs on why every draw does this.
    fn copy_aside(&self, level: usize) {
        let gl = self.gl;
        let (w, h) = (self.width(), self.height());
        gl.bind_framebuffer(Gl::READ_FRAMEBUFFER, Some(&self.layers[level].fbo));
        gl.bind_framebuffer(Gl::DRAW_FRAMEBUFFER, Some(&self.scratch[level].fbo));
        gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, Gl::COLOR_BUFFER_BIT, Gl::NEAREST);
        gl.bind_framebuffer(Gl::FRAMEBUFFER, None);
    }

    fn draw_tiles(&self, (first, count): (u32, u32)) {
        if count == 0 {
            return;
        }
        let gl = self.gl;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.instances));
        gl.enable_vertex_attrib_array(ATTR_INST);
        // No base-instance in WebGL2, so the range is expressed as a byte
        // offset into the one instance buffer.
        gl.vertex_attrib_pointer_with_i32(ATTR_INST, 4, Gl::FLOAT, false, 16, (first * 16) as i32);
        gl.vertex_attrib_divisor(ATTR_INST, 1);
        gl.draw_arrays_instanced(Gl::TRIANGLE_STRIP, 0, 4, count as i32);
    }

    fn set_paint(&self, p: &PaintData) {
        let gl = self.gl;
        gl.uniform1i(self.fill.loc("uKind"), p.kind as i32);
        gl.uniform1i(self.fill.loc("uSpread"), p.spread as i32);
        gl.uniform2f(self.fill.loc("uG0"), p.g0[0], p.g0[1]);
        gl.uniform2f(self.fill.loc("uG1"), p.g1[0], p.g1[1]);
        gl.uniform1f(self.fill.loc("uRadius"), p.radius);
        gl.uniform1i(self.fill.loc("uStopCount"), p.stop_count as i32);
        gl.uniform1fv_with_f32_array(self.fill.loc("uStopOffset[0]"), &p.offsets);
        gl.uniform4fv_with_f32_array(self.fill.loc("uStopColor[0]"), &p.colors);
    }

    fn width(&self) -> i32 {
        self.size.0
    }

    fn height(&self) -> i32 {
        self.size.1
    }

    /// The finished base layer, flipped back to the pixmap's row order.
    fn read_back(&self, data: &DrawData) -> Result<Vec<u8>, JsValue> {
        let gl = self.gl;
        let (w, h) = (data.width as usize, data.height as usize);
        let mut buf = vec![0u8; w * h * 4];
        gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&self.layers[0].fbo));
        gl.read_pixels_with_opt_u8_array(
            0,
            0,
            w as i32,
            h as i32,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(&mut buf),
        )?;
        // GL's row 0 is the bottom; the pixmap's is the top.
        let mut out = Vec::with_capacity(buf.len());
        for row in buf.chunks_exact(w * 4).rev() {
            out.extend_from_slice(row);
        }
        Ok(out)
    }
}

/// How deep the scene's groups nest, refused past [`MAX_GROUP_DEPTH`].
///
/// Checked here rather than mid-frame: the targets are allocated up front, and
/// a scene that would need a fifth one has to be turned away before anything
/// has been drawn.
fn group_depth(data: &DrawData) -> Result<usize, JsValue> {
    let (mut depth, mut max) = (0usize, 0usize);
    for op in &data.ops {
        match op {
            Op::PushGroup => {
                depth += 1;
                max = max.max(depth);
            }
            Op::PopGroup { .. } => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if max > MAX_GROUP_DEPTH {
        return Err(JsValue::from_str(&format!(
            "groups nest {max} deep, the bound is {MAX_GROUP_DEPTH}"
        )));
    }
    Ok(max)
}

fn rule_code(rule: FillRule) -> i32 {
    match rule {
        FillRule::NonZero => 0,
        FillRule::EvenOdd => 1,
    }
}

/// The blend mode as the shader's `uBlend`, in the order `BlendMode` declares.
fn blend_code(mode: BlendMode) -> i32 {
    match mode {
        BlendMode::Normal => 0,
        BlendMode::Multiply => 1,
        BlendMode::Screen => 2,
        BlendMode::Overlay => 3,
        BlendMode::Darken => 4,
        BlendMode::Lighten => 5,
        BlendMode::ColorDodge => 6,
        BlendMode::ColorBurn => 7,
        BlendMode::HardLight => 8,
        BlendMode::SoftLight => 9,
        BlendMode::Difference => 10,
        BlendMode::Exclusion => 11,
        BlendMode::Hue => 12,
        BlendMode::Saturation => 13,
        BlendMode::Color => 14,
        BlendMode::Luminosity => 15,
    }
}

fn nearest(gl: &Gl) {
    for (p, v) in [
        (Gl::TEXTURE_MIN_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_MAG_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
        (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
    ] {
        gl.tex_parameteri(Gl::TEXTURE_2D, p, v as i32);
    }
}

fn make_target(gl: &Gl, w: i32, h: i32, format: u32) -> Result<Target, JsValue> {
    let texture = gl
        .create_texture()
        .ok_or_else(|| JsValue::from_str("no texture"))?;
    gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
    nearest(gl);
    gl.tex_storage_2d(Gl::TEXTURE_2D, 1, format, w.max(1), h.max(1));
    let fbo = gl
        .create_framebuffer()
        .ok_or_else(|| JsValue::from_str("no framebuffer"))?;
    gl.bind_framebuffer(Gl::FRAMEBUFFER, Some(&fbo));
    gl.framebuffer_texture_2d(
        Gl::FRAMEBUFFER,
        Gl::COLOR_ATTACHMENT0,
        Gl::TEXTURE_2D,
        Some(&texture),
        0,
    );
    let status = gl.check_framebuffer_status(Gl::FRAMEBUFFER);
    if status != Gl::FRAMEBUFFER_COMPLETE {
        return Err(JsValue::from_str(&format!(
            "framebuffer incomplete (0x{status:x}) for format 0x{format:x}"
        )));
    }
    Ok(Target { texture, fbo })
}

fn upload_f32(gl: &Gl, data: &[f32]) {
    // A view over wasm memory, which `bufferData` copies out of immediately --
    // nothing here allocates or outlives the call.
    let view = js_sys::Float32Array::from(data);
    gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &view, Gl::STATIC_DRAW);
}

fn program(gl: &Gl, vs: &str, fs: &str) -> Result<Program, JsValue> {
    let v = shader(gl, Gl::VERTEX_SHADER, vs)?;
    let f = shader(gl, Gl::FRAGMENT_SHADER, fs)?;
    let program = gl
        .create_program()
        .ok_or_else(|| JsValue::from_str("no program"))?;
    gl.attach_shader(&program, &v);
    gl.attach_shader(&program, &f);
    gl.bind_attrib_location(&program, ATTR_CORNER, "aCorner");
    gl.bind_attrib_location(&program, ATTR_INST, "aInst");
    gl.link_program(&program);
    if !gl
        .get_program_parameter(&program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        let log = gl.get_program_info_log(&program).unwrap_or_default();
        return Err(JsValue::from_str(&format!("link failed: {log}")));
    }
    let u = UNIFORMS
        .iter()
        .map(|&n| (n, gl.get_uniform_location(&program, n)))
        .collect();
    Ok(Program { program, u })
}

fn shader(gl: &Gl, kind: u32, source: &str) -> Result<web_sys::WebGlShader, JsValue> {
    let s = gl
        .create_shader(kind)
        .ok_or_else(|| JsValue::from_str("no shader"))?;
    gl.shader_source(&s, source);
    gl.compile_shader(&s);
    if !gl
        .get_shader_parameter(&s, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        let log = gl.get_shader_info_log(&s).unwrap_or_default();
        return Err(JsValue::from_str(&format!("compile failed: {log}")));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shader's `uBlend` numbering and `BlendMode`'s declaration order are
    /// two lists that must stay identical, in two languages, and only this
    /// notices when one moves.
    #[test]
    fn blend_codes_are_dense_and_ordered() {
        const ALL: [BlendMode; 16] = [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::Darken,
            BlendMode::Lighten,
            BlendMode::ColorDodge,
            BlendMode::ColorBurn,
            BlendMode::HardLight,
            BlendMode::SoftLight,
            BlendMode::Difference,
            BlendMode::Exclusion,
            BlendMode::Hue,
            BlendMode::Saturation,
            BlendMode::Color,
            BlendMode::Luminosity,
        ];
        for (i, m) in ALL.into_iter().enumerate() {
            assert_eq!(blend_code(m), i as i32, "{m:?}");
            // Every code past 0 has an arm in `blendRGB`, or the mode silently
            // renders as Normal.
            if i > 0 {
                assert!(
                    BLEND_GLSL.contains(&format!("uBlend == {i})")),
                    "no shader arm for {m:?}"
                );
            }
        }
    }

    /// A fixture the harness cannot name is a fixture it cannot ask for.
    #[test]
    fn fixtures_are_reachable_by_index() {
        let n = hane_fixture_count();
        assert!(n >= 28, "corpus shrank to {n}");
        assert!(!hane_fixture_name(0).is_empty());
        assert_eq!(
            hane_fixture_name(n),
            "",
            "past the end is empty, not a panic"
        );
    }

    /// The stop arrays are uploaded whole, so their length has to be exactly
    /// what the shader declares or the driver rejects the call.
    #[test]
    fn the_stop_uniform_arrays_match_the_shader() {
        let n = hane_gpu::MAX_STOPS;
        assert!(PAINT_GLSL.contains(&format!("uStopOffset[{n}]")));
        assert!(PAINT_GLSL.contains(&format!("uStopColor[{n}]")));
    }
}
