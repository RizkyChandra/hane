//! The P3 gate harness: one scripted frame loop over the parts that exist (#31).
//!
//! # What a "frame" is here, and what it is missing
//!
//! **There is no GPU renderer.** P2 is unbuilt, so nothing in this repository
//! can turn a bin list into pixels on a `<canvas>`. Reporting a frame time for
//! a renderer that does not exist would be worse than reporting nothing, so
//! this harness measures three phases and never adds up a number it cannot
//! stand behind:
//!
//! - **cull** -- advance the scripted camera, [`View::visible_bounds`], one
//!   [`Quadtree::query`] for the visible set, [`TileKey::visible`] for the
//!   tiles on screen, and a [`TileCache::get`] per tile. Every renderer pays
//!   this, on the CPU, before it draws anything.
//! - **encode** -- for each tile that missed: query the index for that tile's
//!   square, map the segments into the tile's own device pixels, and
//!   [`TileBinner::bin`] them. This is the CPU half of the GPU path (D-010:
//!   `hane-gpu` computes buffer contents, `hane-wasm` submits them). The
//!   submit and the shading are the missing half.
//! - **raster** -- for each tile that missed, the same segments through
//!   `hane-raster`, into real pixels the cache then stores. This is the
//!   *oracle* (D-002), written to be obviously correct rather than fast. It is
//!   not the shipping renderer and its cost is not P2's cost.
//!
//! So `cull + encode` is the CPU cost of a GPU frame with the GPU removed --
//! a lower bound on the real frame, and the number the gate has to be judged
//! against once P2 exists. `cull + raster` is a complete frame that genuinely
//! draws, by a path that was never meant to ship.
//!
//! Encode and raster are alternatives, not stages: a real frame does one or
//! the other. Both run here so the cache holds real pixels and its eviction is
//! real, and so P2 has the oracle's number to beat.
//!
//! # The document is fixed, and that is the harder reading
//!
//! `spatial_bench` and `tile_bench` grow the document with the item count so
//! density stays constant. That is the right model for measuring an index, and
//! it makes the visible set roughly constant at every `n` -- which would make
//! this harness answer "1k and 500k cost the same", true and useless.
//!
//! Here the document is a fixed [`DOC_SIDE`] square at every size, so `n` is
//! the number of objects actually competing for the screen: at 1k the viewport
//! holds a handful, at 500k it holds tens of thousands. That is the reading of
//! "100k filled shapes, p99 under 16ms" that can fail.

use std::cell::RefCell;

use hane_geom::fuzz::Rng;
use hane_geom::{CubicBez, PathEl, Point, Rect, Vec2};
use hane_gpu::TileBinner;
use hane_path::Segment;
use hane_raster::{Color, Pixmap};
use hane_scene::{Quadtree, TileCache, TileKey, View};

/// The document is this many units on a side at every scene size.
pub const DOC_SIDE: f64 = 4096.0;

/// Overdraw band in screen pixels, matching `tile_bench`.
const MARGIN: f64 = 128.0;

/// Flattening tolerance in device pixels.
///
/// The binner's contract is that this is *the same* tolerance the rasterizer
/// draws with, or a tile can be missed by up to a tolerance of curve. Both
/// phases below read this one constant for that reason.
const TOLERANCE: f64 = 0.25;

/// 64 MiB of resident tiles, matching `tile_bench` so the hit rates compare.
const BUDGET: usize = 64 << 20;

/// Frames between reversals of the zoom direction.
///
/// `tile_bench` zooms one way for its whole run; over 300 frames at 1.0078 per
/// frame that is a 10x magnification, and the second half of the run sees
/// almost nothing. Reversing keeps the load representative for the whole run
/// and crosses the power-of-two level boundaries in *both* directions, which
/// is the case the cache's level-keyed design exists for.
const ZOOM_HALF_PERIOD: u32 = 90;

/// Cubic control-point offset for a quarter ellipse: `4/3 * (sqrt(2) - 1)`.
const KAPPA: f64 = 0.552_284_749_830_793_4;

/// Appends the four cubics of an axis-aligned ellipse, as one closed ring.
///
/// Written out rather than generated from a sign table: the quadrants have to
/// chain end-to-start, and a table that gets a sign wrong produces four arcs
/// that each look right and do not join.
fn ellipse(cx: f64, cy: f64, rx: f64, ry: f64, out: &mut Vec<CubicBez>) {
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    let p = |x: f64, y: f64| Point::new(cx + x, cy + y);
    let arc = |a: Point, b: Point, c: Point, d: Point| CubicBez::new(a, b, c, d);
    let (e, n, w, s) = (p(rx, 0.0), p(0.0, ry), p(-rx, 0.0), p(0.0, -ry));
    out.push(arc(e, p(rx, ky), p(kx, ry), n));
    out.push(arc(n, p(-kx, ry), p(-rx, ky), w));
    out.push(arc(w, p(-rx, -ky), p(-kx, -ry), s));
    out.push(arc(s, p(kx, -ry), p(rx, -ky), e));
}

/// A synthetic scene and everything one frame of it touches.
///
/// One instance per scene size. Held across frames so that the per-frame path
/// allocates nothing after the first frame, which is what a real engine does
/// and what keeps an allocator's growth out of the percentiles.
pub struct Bench {
    tree: Quadtree,
    /// Four cubics per shape, at `id * 4`.
    curves: Vec<CubicBez>,
    cache: TileCache,
    binner: TileBinner,
    /// Side in device pixels the `binner` was built for.
    binner_side: u32,
    view: View,
    screen: Rect,
    frame: u32,
    /// Screen pixels of pan per frame. The one knob that decides how much of
    /// the cache a frame invalidates, and therefore what the p99 is.
    pan: f64,
    /// Ids returned by the viewport cull, this frame.
    visible: Vec<u32>,
    /// Tiles on screen, this frame.
    keys: Vec<TileKey>,
    /// The subset of `keys` the cache did not have.
    misses: Vec<TileKey>,
    /// Scratch for the per-tile index query.
    items: Vec<u32>,
    /// Scratch for the per-tile segment list, in tile-local device pixels.
    segs: Vec<Segment>,
    /// Scratch for the per-shape path handed to the oracle.
    path: Vec<PathEl>,
}

impl Bench {
    /// Builds a scene of `n` shapes and a view of a `width` by `height`
    /// viewport, centred on the document at 1:1, panning `pan` screen pixels
    /// per frame.
    #[must_use]
    pub fn new(n: u32, seed: u64, width: u32, height: u32, pan: f64) -> Self {
        let mut rng = Rng::new(seed);
        let mut boxes = Vec::with_capacity(n as usize);
        let mut curves = Vec::with_capacity(n as usize * 4);
        for id in 0..n {
            let (cx, cy) = (rng.unit() * DOC_SIDE, rng.unit() * DOC_SIDE);
            // Diameters of 4 to 40 units: the size range `spatial_bench` and
            // `tile_bench` use, so a shape here is the same object those
            // tables describe.
            let (rx, ry) = (rng.unit() * 18.0 + 2.0, rng.unit() * 18.0 + 2.0);
            boxes.push((id, Rect::new(cx - rx, cy - ry, cx + rx, cy + ry)));
            ellipse(cx, cy, rx, ry, &mut curves);
        }
        let screen = Rect::new(0.0, 0.0, f64::from(width), f64::from(height));
        let mut view = View::new();
        view.anchor_at(Point::new(DOC_SIDE / 2.0, DOC_SIDE / 2.0), screen.center());
        Self {
            tree: Quadtree::bulk_load(Rect::new(0.0, 0.0, DOC_SIDE, DOC_SIDE), &boxes),
            curves,
            cache: TileCache::new(BUDGET),
            binner: TileBinner::new(1, 1),
            binner_side: 0,
            view,
            screen,
            frame: 0,
            pan,
            visible: Vec::new(),
            keys: Vec::new(),
            misses: Vec::new(),
            items: Vec::new(),
            segs: Vec::new(),
            path: Vec::new(),
        }
    }

    /// The scripted navigation: one frame of it.
    ///
    /// The zoom is `tile_bench`'s -- 1.0078 per frame, a power-of-two level
    /// boundary every ~90 frames -- but reversing, so the run crosses a level
    /// in both directions instead of magnifying 10x and then seeing nothing.
    ///
    /// The pan turns steadily rather than running straight. That is not
    /// decoration: at 40 px per frame a straight line leaves a 4096-unit
    /// document after 100 frames and the rest of the run measures empty space.
    /// A circle of `300 * pan / 2pi` pixels stays inside it at any speed and
    /// only retraces its own tiles at the very end of the run.
    ///
    /// Purely a function of the frame counter, so a run is repeatable to the
    /// tile and a Chrome run and a Firefox run measure identical work.
    fn step(&mut self) {
        let centre = self.screen.center();
        let inward = (self.frame / ZOOM_HALF_PERIOD).is_multiple_of(2);
        self.view
            .zoom_about(centre, if inward { 1.0078 } else { 1.0 / 1.0078 });
        // One turn per 300 frames. `sin`/`cos` are the compiled-in libm on
        // wasm32, so both browsers walk exactly the same path.
        let angle = f64::from(self.frame) * (core::f64::consts::TAU / 300.0);
        self.view
            .pan_by(Vec2::new(-self.pan * angle.cos(), -self.pan * angle.sin()));
        self.frame += 1;
    }

    /// Advances the camera and culls. Returns the number of tiles that missed.
    ///
    /// The visible-set query is done and thrown away rather than used: a tiled
    /// renderer draws from the tile grid, but it still needs the visible set
    /// for hit testing, selection bounds and the layer panel, and it is part of
    /// what every frame pays. [`Bench::visible`] reports what it returned.
    pub fn cull(&mut self) -> u32 {
        self.step();
        self.visible.clear();
        let area = self.view.visible_bounds(self.screen, MARGIN);
        self.tree.query(area, &mut self.visible);

        self.keys.clear();
        TileKey::visible(&self.view, self.screen, MARGIN, &mut self.keys);
        self.misses.clear();
        for i in 0..self.keys.len() {
            let key = self.keys[i];
            if self.cache.get(key).is_none() {
                self.misses.push(key);
            }
        }
        self.misses.len() as u32
    }

    /// Gathers the segments of one missed tile into `self.segs`, in that
    /// tile's own device pixels, and returns the tile's side in those pixels.
    ///
    /// The tile grid is axis-aligned in the *document* (see `hane-scene`'s
    /// `tile` module), so this mapping is a translate and a uniform scale with
    /// no rotation in it -- the view's rotation decides which tiles are drawn
    /// and how they are composited, never what is inside one.
    fn gather(&mut self, key: TileKey) -> u32 {
        let rect = key.doc_rect();
        // A power of two, so the scale is exact and a shape on a tile boundary
        // lands on the same coordinate from either side.
        let scale = 2f64.powi(i32::from(key.level));
        self.items.clear();
        self.tree.query(rect, &mut self.items);
        self.segs.clear();
        let map = |p: Point| Point::new((p.x - rect.x0) * scale, (p.y - rect.y0) * scale);
        for &id in &self.items {
            let base = id as usize * 4;
            for c in &self.curves[base..base + 4] {
                self.segs.push(Segment::Cubic(CubicBez::new(
                    map(c.p0),
                    map(c.p1),
                    map(c.p2),
                    map(c.p3),
                )));
            }
        }
        (rect.width() * scale).round() as u32
    }

    /// Bins every missed tile. Returns the number of segments binned.
    ///
    /// The CPU half of a GPU frame. What is *not* here is the upload and the
    /// draw, because there is nothing to upload to yet.
    pub fn encode(&mut self) -> u32 {
        let mut segments = 0;
        for i in 0..self.misses.len() {
            let side = self.gather(self.misses[i]);
            if self.binner_side != side {
                self.binner = TileBinner::new(side, side);
                self.binner_side = side;
            }
            self.binner.bin(&self.segs, TOLERANCE);
            segments += self.segs.len() as u32;
        }
        segments
    }

    /// Renders every missed tile with the CPU oracle and stores it.
    ///
    /// This is what fills the cache, so the hit rate and the eviction the other
    /// phases see are the real ones. Its *time* is the oracle's, not a
    /// renderer's -- see the module docs.
    pub fn raster(&mut self) -> u32 {
        for i in 0..self.misses.len() {
            let key = self.misses[i];
            let side = self.gather(key);
            let mut pixmap = Pixmap::new(side, side);
            for shape in 0..self.segs.len() / 4 {
                self.path.clear();
                let quads = &self.segs[shape * 4..shape * 4 + 4];
                self.path.push(PathEl::MoveTo(quads[0].start()));
                for seg in quads {
                    let Segment::Cubic(c) = *seg else { continue };
                    self.path.push(PathEl::CurveTo(c.p1, c.p2, c.p3));
                }
                self.path.push(PathEl::ClosePath);
                // Colour from the shape index so the fills are not all the
                // same value; the rasterizer's cost does not depend on it, but
                // a golden image of a solid grey block would hide a bug.
                let v = (shape as u32).wrapping_mul(2_654_435_761);
                let colour = Color {
                    r: v as u8,
                    g: (v >> 8) as u8,
                    b: (v >> 16) as u8,
                    a: 160,
                };
                pixmap.fill_path(&self.path, colour);
            }
            self.cache.insert(key, pixmap.data().to_vec());
        }
        self.misses.len() as u32
    }

    /// Items the viewport cull returned on the last [`Bench::cull`].
    #[must_use]
    pub fn visible(&self) -> u32 {
        self.visible.len() as u32
    }

    /// Tiles on screen on the last [`Bench::cull`].
    #[must_use]
    pub fn tiles(&self) -> u32 {
        self.keys.len() as u32
    }

    /// The tile cache's hit rate over the whole run so far.
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        self.cache.hit_rate()
    }

    /// Resident tile bytes.
    #[must_use]
    pub fn bytes(&self) -> u32 {
        self.cache.bytes() as u32
    }
}

thread_local! {
    /// The live scene. One at a time: the browser page runs the sizes in
    /// sequence and 500k shapes is already ~200 MiB of linear memory.
    static BENCH: RefCell<Option<Bench>> = const { RefCell::new(None) };
}

fn with<R: Default>(f: impl FnOnce(&mut Bench) -> R) -> R {
    BENCH.with_borrow_mut(|slot| slot.as_mut().map_or_else(R::default, f))
}

/// Builds a scene of `n` shapes for a `width` by `height` viewport, panning
/// `pan` screen pixels per frame, replacing any previous one. Returns `n`.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_init(n: u32, seed: u32, width: u32, height: u32, pan: f64) -> u32 {
    // Dropped before the new one is built, so peak memory is one scene rather
    // than two -- at 500k shapes the difference is a few hundred megabytes.
    BENCH.with_borrow_mut(|slot| *slot = None);
    let bench = Bench::new(n, u64::from(seed), width, height, pan);
    BENCH.with_borrow_mut(|slot| *slot = Some(bench));
    n
}

/// One frame's cull phase. Returns the number of tiles that missed the cache.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_cull() -> u32 {
    with(Bench::cull)
}

/// One frame's encode phase. Returns the number of segments binned.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_encode() -> u32 {
    with(Bench::encode)
}

/// One frame's raster phase. Returns the number of tiles rendered.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_raster() -> u32 {
    with(Bench::raster)
}

/// Items the last cull returned.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_visible() -> u32 {
    with(|b| b.visible())
}

/// Tiles on screen at the last cull.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_tiles() -> u32 {
    with(|b| b.tiles())
}

/// The tile cache's hit rate so far, in `[0, 1]`.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_hit_rate() -> f64 {
    with(|b| b.hit_rate())
}

/// Resident tile bytes.
#[unsafe(no_mangle)]
pub extern "C" fn hane_bench_bytes() -> u32 {
    with(|b| b.bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole loop, small enough to run in a debug test: the phases have to
    /// agree on how many tiles missed, the cache has to fill, and the second
    /// lap over ground already visited has to hit.
    #[test]
    fn a_scripted_run_fills_the_cache_and_then_hits() {
        let mut b = Bench::new(2_000, 7, 640, 360, 4.0);
        let mut first_frame_misses = 0;
        for frame in 0..40 {
            let misses = b.cull();
            if frame == 0 {
                // Nothing is resident yet, so every tile on screen must miss.
                assert_eq!(misses, b.tiles(), "frame 0 should miss everything");
                first_frame_misses = misses;
            }
            assert_eq!(b.raster(), misses, "raster must draw exactly the misses");
            // Encode sees the same tiles; it runs after raster only in this
            // assertion, and must still report work for each of them.
            assert_eq!(b.encode() > 0, misses > 0);
        }
        assert!(first_frame_misses > 0);
        assert!(b.visible() > 0, "the viewport should hold some shapes");
        assert!(b.bytes() > 0, "the cache should be holding tiles");
        // The pan is slow and the zoom reverses, so the great majority of the
        // run revisits tiles it has already drawn. Anything below this and the
        // caching is not working at all.
        assert!(b.hit_rate() > 0.5, "hit rate was {}", b.hit_rate());
    }

    /// The camera is a pure function of the frame counter, so two runs of the
    /// same size see the same tiles in the same order. Without this the p99 of
    /// a Chrome run and a Firefox run are not measuring the same work.
    #[test]
    fn the_script_is_repeatable() {
        let trace = |_: ()| {
            let mut b = Bench::new(500, 3, 320, 200, 12.0);
            (0..25)
                .map(|_| {
                    let m = b.cull();
                    b.raster();
                    (m, b.tiles(), b.visible())
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(trace(()), trace(()));
    }

    /// The four cubics close into a ring: each one starts where the last ended,
    /// exactly. A gap here is a hole in every fill.
    #[test]
    fn the_ellipse_closes() {
        let mut out = Vec::new();
        ellipse(10.0, -3.0, 7.0, 2.0, &mut out);
        assert_eq!(out.len(), 4);
        for i in 0..4 {
            let (a, b) = (out[i], out[(i + 1) % 4]);
            assert_eq!(a.p3, b.p0, "cubic {i} does not meet its successor");
        }
        // And it is the ellipse it claims to be, at the four axis crossings.
        let bbox = out
            .iter()
            .fold(Rect::EMPTY, |acc, c| acc.union(c.bounding_box()));
        assert!((bbox.x0 - 3.0).abs() < 1e-12 && (bbox.x1 - 17.0).abs() < 1e-12);
        assert!((bbox.y0 + 5.0).abs() < 1e-12 && (bbox.y1 + 1.0).abs() < 1e-12);
    }
}
