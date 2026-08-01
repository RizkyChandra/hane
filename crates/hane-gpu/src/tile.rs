//! Screen-tile binning: which segments each tile of the viewport needs.
//!
//! Pure geometry in, bin assignments out. No GL call reaches here (D-010), so
//! the part of the rasterizer that actually contains the bugs is a `cargo test`
//! away rather than a browser away.
//!
//! WebGL2 has no compute shaders (D-003), so this runs on the CPU inside the
//! frame budget. That rules out anything clever: one flatten, one slab walk per
//! line, one counting sort, all over buffers that live across frames.

use hane_geom::{Point, Rect};
use hane_path::Segment;

/// Tile edge length in device pixels.
///
/// 8 and 16 are within 10% of each other on binning cost against modelled fill,
/// and everything outside that range is clearly worse. 16 takes it on the two
/// things the fill model does not see: half the bin entries to upload per frame,
/// and a quarter of the tiles for the P3 cache and for whatever per-tile draw
/// state D-003 forces. Re-measure before moving it -- `cargo run --release
/// --example bin_bench`, and `BENCHMARKS.md`, "Tile binning".
pub const TILE_SIZE: u32 = 16;

/// Assigns path segments to the screen tiles they cross.
///
/// Holds its working buffers so a per-frame re-bin allocates nothing after the
/// first frame. One binner covers one viewport size; make a new one when the
/// canvas resizes.
pub struct TileBinner {
    size: u32,
    cols: u32,
    rows: u32,
    /// The last segment index written into each tile. A curve that leaves a
    /// tile and comes back must not be listed twice, and comparing against the
    /// segment index does that without clearing the array between segments.
    /// `u32::MAX` is "untouched", which is why segment counts are capped below
    /// it.
    stamp: Vec<u32>,
    /// `(tile, segment)` in segment order, before the counting sort.
    pairs: Vec<(u32, u32)>,
    /// Per-tile start offsets into `segs`, prefix-summed. `tile_count + 1` long.
    starts: Vec<u32>,
    /// Segment indices, grouped by tile, ascending within each tile.
    segs: Vec<u32>,
    /// The tiles with at least one segment, ascending. Downstream iterates this
    /// and never sees an empty tile.
    occupied: Vec<u32>,
    /// Flattening scratch, reused across segments.
    flat: Vec<Point>,
}

impl TileBinner {
    /// A binner for a `width` by `height` device-pixel viewport.
    ///
    /// The grid is rounded up, so the right and bottom tiles may hang off the
    /// edge of the viewport; that costs nothing and keeps the clip rectangle a
    /// whole number of tiles.
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_tile_size(width, height, TILE_SIZE)
    }

    /// A binner on a tile size other than [`TILE_SIZE`].
    ///
    /// This is the knob the constant was chosen with, kept so the number stays
    /// re-measurable on hardware that is not the machine in `BENCHMARKS.md`.
    /// Production code wants [`TileBinner::new`].
    ///
    /// # Panics
    ///
    /// If `size` is not a power of two. That is what lets the walk turn its
    /// divisions into an exact multiply by a reciprocal, and every tile size
    /// worth measuring is one anyway.
    pub fn with_tile_size(width: u32, height: u32, size: u32) -> Self {
        assert!(size.is_power_of_two(), "tile size must be a power of two");
        let cols = width.div_ceil(size);
        let rows = height.div_ceil(size);
        let tiles = (cols as usize) * (rows as usize);
        Self {
            size,
            cols,
            rows,
            stamp: vec![u32::MAX; tiles],
            pairs: Vec::new(),
            starts: vec![0; tiles + 1],
            segs: Vec::new(),
            occupied: Vec::new(),
            flat: Vec::new(),
        }
    }

    /// Bins `segments`, replacing whatever the previous call produced.
    ///
    /// Coordinates are device pixels: apply the view transform first. Anything
    /// off the viewport is dropped, so this culls as it bins.
    ///
    /// `tolerance` is the flattening tolerance in device pixels, and **must be
    /// the one the rasterizer will draw with**. Bins are exact for the polyline
    /// a curve flattens to, not for the ideal curve; matching tolerances is what
    /// makes that the same thing. A tile can otherwise be missed by up to
    /// `tolerance` of curve.
    ///
    /// Deterministic: the same slice gives byte-identical bins, because the
    /// walk has no floating-point accumulation and the sort is a counting sort
    /// over segments already in index order.
    pub fn bin(&mut self, segments: &[Segment], tolerance: f64) {
        self.pairs.clear();
        self.segs.clear();
        self.occupied.clear();
        let tile_count = (self.cols as usize) * (self.rows as usize);
        self.starts.clear();
        self.starts.resize(tile_count + 1, 0);
        // The stamps are segment indices, so last frame's would suppress this
        // frame's segment 0. Refilling is two memsets over a few tens of
        // kilobytes; a generation counter would save them and buy a wraparound
        // bug.
        self.stamp.fill(u32::MAX);
        if tile_count == 0 {
            return;
        }
        assert!(
            segments.len() < u32::MAX as usize,
            "segment index must stay below the u32::MAX stamp sentinel"
        );

        let clip = Rect::new(
            0.0,
            0.0,
            f64::from(self.cols * self.size),
            f64::from(self.rows * self.size),
        );
        for (i, seg) in segments.iter().enumerate() {
            let i = i as u32;
            self.flat.clear();
            match *seg {
                Segment::Line(a, b) => {
                    self.flat.push(a);
                    self.flat.push(b);
                }
                Segment::Quad(q) => q.flatten(tolerance, &mut self.flat),
                Segment::Cubic(c) => c.flatten(tolerance, &mut self.flat),
            }
            for w in 0..self.flat.len().saturating_sub(1) {
                let (a, b) = (self.flat[w], self.flat[w + 1]);
                if let Some((a, b)) = clip_to(a, b, clip) {
                    walk(a, b, self.size, self.cols, self.rows, &mut |tile| {
                        // The stamp both de-duplicates and marks first touch,
                        // so `occupied` comes out ascending only after the
                        // prefix-sum scan below -- not here.
                        let t = tile as usize;
                        if self.stamp[t] != i {
                            self.stamp[t] = i;
                            self.pairs.push((tile, i));
                        }
                    });
                }
            }
        }

        // Counting sort into CSR. The one O(tile_count) sweep in the frame; it
        // buys both the prefix sums and the ascending occupied list, so no
        // comparison sort and no per-tile `Vec` is needed.
        for &(tile, _) in &self.pairs {
            self.starts[tile as usize + 1] += 1;
        }
        let mut total = 0;
        for tile in 0..tile_count {
            let n = self.starts[tile + 1];
            if n != 0 {
                self.occupied.push(tile as u32);
            }
            total += n;
            self.starts[tile + 1] = total;
        }
        self.segs.resize(total as usize, 0);
        // `starts[tile]` doubles as the write cursor and is restored to the
        // group start by the time the group is full.
        for &(tile, seg) in &self.pairs {
            let cursor = &mut self.starts[tile as usize];
            self.segs[*cursor as usize] = seg;
            *cursor += 1;
        }
        // Undo the cursor walk: every start is now its successor's start.
        self.starts.copy_within(..tile_count, 1);
        self.starts[0] = 0;
    }

    /// The non-empty tiles and their segments, in ascending tile order.
    ///
    /// Empty tiles are not listed at all, so a mostly-blank frame iterates
    /// nothing.
    pub fn tiles(&self) -> impl Iterator<Item = (u32, &[u32])> {
        self.occupied.iter().map(move |&tile| {
            let t = tile as usize;
            let (a, b) = (self.starts[t] as usize, self.starts[t + 1] as usize);
            (tile, &self.segs[a..b])
        })
    }

    /// The device-pixel rectangle a tile id covers.
    pub fn tile_rect(&self, tile: u32) -> Rect {
        let x = f64::from((tile % self.cols) * self.size);
        let y = f64::from((tile / self.cols) * self.size);
        Rect::new(x, y, x + f64::from(self.size), y + f64::from(self.size))
    }

    /// The grid dimensions in tiles.
    pub fn grid(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }
}

/// Liang-Barsky. `None` when the segment misses `clip` entirely, which is the
/// cull: a line from -1e9 to 1e9 would otherwise be walked tile by tile.
///
/// Non-finite input is dropped rather than clamped. A NaN coordinate fails
/// every comparison below and would silently walk the wrong tiles.
fn clip_to(p0: Point, p1: Point, clip: Rect) -> Option<(Point, Point)> {
    // The overwhelming majority of a real frame is already on screen, and the
    // four divisions below are the single most expensive thing in the walk.
    // NaN fails this test and falls through to the finite check.
    if clip.contains(p0) && clip.contains(p1) {
        return Some((p0, p1));
    }
    if !(p0.x.is_finite() && p0.y.is_finite() && p1.x.is_finite() && p1.y.is_finite()) {
        return None;
    }
    let (dx, dy) = (p1.x - p0.x, p1.y - p0.y);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-dx, p0.x - clip.x0),
        (dx, clip.x1 - p0.x),
        (-dy, p0.y - clip.y0),
        (dy, clip.y1 - p0.y),
    ] {
        if p == 0.0 {
            // Parallel to this edge: in or out for the whole segment.
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    // Clamped after the lerp because a segment spanning 1e300 loses the last
    // bits of the parameter and can land a hair outside; the walk indexes an
    // array with these.
    let clamp = |p: Point| Point::new(p.x.clamp(clip.x0, clip.x1), p.y.clamp(clip.y0, clip.y1));
    Some((clamp(p0.lerp(p1, t0)), clamp(p0.lerp(p1, t1))))
}

/// Calls `emit` once per tile the clipped segment `a`-`b` passes through.
///
/// Column-slab walk rather than a stepping DDA: within one column the segment
/// is a straight monotone piece, so the rows it touches are exactly those
/// between the piece's two endpoints. That is exact by construction and has no
/// step accumulating error across a thousand-tile span.
///
/// Tiles are half-open, `[c * TILE, (c + 1) * TILE)`, so a segment running
/// exactly along a tile boundary belongs to the tile on the right or below --
/// the same convention pixel ownership uses.
fn walk(a: Point, b: Point, tile: u32, cols: u32, rows: u32, emit: &mut impl FnMut(u32)) {
    let size = f64::from(tile);
    // Exact, not an approximation: the tile size is a power of two, so its
    // reciprocal is too, and the multiply agrees with the divide bit for bit.
    let inv_size = 1.0 / size;
    let index = |v: f64, n: u32| ((v * inv_size) as u32).min(n - 1);
    let (lo, hi) = if a.x <= b.x { (a.x, b.x) } else { (b.x, a.x) };
    let (c0, c1) = (index(lo, cols), index(hi, cols));
    let dx = b.x - a.x;
    // Reciprocal once instead of a divide per slab edge. The parameter is
    // clamped to `[0, 1]` either way, so the ulp a reciprocal costs cannot push
    // the lerp past an endpoint.
    let inv_dx = 1.0 / dx;
    let at = |x: f64| a.lerp(b, ((x - a.x) * inv_dx).clamp(0.0, 1.0)).y;
    // The right edge of one slab is the left edge of the next, so each interior
    // boundary is evaluated once and carried forward.
    let mut ya = if dx == 0.0 { a.y.min(b.y) } else { at(lo) };
    for c in c0..=c1 {
        // The segment's own x-range, cut down to this column.
        let sx1 = (f64::from(c + 1) * size).min(hi);
        // Vertical segments live in one column, where the slab says nothing
        // about y and the whole span is the answer.
        let yb = if dx == 0.0 { a.y.max(b.y) } else { at(sx1) };
        let r0 = index(ya.min(yb), rows);
        let r1 = index(ya.max(yb), rows);
        for r in r0..=r1 {
            emit(r * cols + c);
        }
        ya = yb;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};
    use hane_geom::{CubicBez, QuadBez};

    const TOL: f64 = 0.25;

    /// Bins collected as a plain sorted list, for comparing against an oracle.
    fn bins(binner: &TileBinner) -> Vec<(u32, Vec<u32>)> {
        binner.tiles().map(|(t, s)| (t, s.to_vec())).collect()
    }

    /// Does the segment `a`-`b` meet `rect`? Independent of `walk`: it clips,
    /// which is a different question asked a different way.
    fn hits(a: Point, b: Point, rect: Rect) -> bool {
        clip_to(a, b, rect).is_some()
    }

    /// Every tile of the grid whose rectangle the polyline meets, brute force.
    ///
    /// Tiles are shrunk by a hair on the far edges so the oracle uses the same
    /// half-open convention as `walk`. Random coordinates land on a boundary
    /// essentially never, so this only matters for the hand-written cases.
    fn oracle(binner: &TileBinner, poly: &[Point]) -> Vec<u32> {
        let (cols, rows) = binner.grid();
        (0..cols * rows)
            .filter(|&t| {
                let r = binner.tile_rect(t);
                let r = Rect::new(r.x0, r.y0, r.x1 - 1e-9, r.y1 - 1e-9);
                poly.windows(2).any(|w| hits(w[0], w[1], r))
            })
            .collect()
    }

    /// A point in and a little around a 256x256 viewport, so roughly a third of
    /// the generated geometry is partly off-screen.
    fn screen_point(rng: &mut Rng) -> Point {
        Point::new(
            rng.below(400) as f64 - 72.0 + rng.unit(),
            rng.below(400) as f64 - 72.0 + rng.unit(),
        )
    }

    #[test]
    fn lines_land_in_exactly_the_tiles_they_cross() {
        check(
            "line bins match brute force",
            2000,
            |r| (screen_point(r), screen_point(r)),
            |&(a, b)| {
                let mut binner = TileBinner::new(256, 256);
                binner.bin(&[Segment::Line(a, b)], TOL);
                let got: Vec<u32> = binner.tiles().map(|(t, _)| t).collect();
                got == oracle(&binner, &[a, b])
            },
        );
    }

    #[test]
    fn curves_land_in_exactly_the_tiles_their_polyline_crosses() {
        check(
            "cubic bins match brute force over the flattened curve",
            500,
            |r| {
                CubicBez::new(
                    screen_point(r),
                    screen_point(r),
                    screen_point(r),
                    screen_point(r),
                )
            },
            |&c| {
                let mut binner = TileBinner::new(256, 256);
                binner.bin(&[Segment::Cubic(c)], TOL);
                let got: Vec<u32> = binner.tiles().map(|(t, _)| t).collect();
                let mut poly = Vec::new();
                c.flatten(TOL, &mut poly);
                got == oracle(&binner, &poly)
            },
        );
    }

    #[test]
    fn a_curve_reentering_a_tile_is_listed_once() {
        // A hairpin: out of the first tile and back into it.
        let c = CubicBez::new(
            Point::new(4.0, 4.0),
            Point::new(200.0, 4.0),
            Point::new(200.0, 12.0),
            Point::new(4.0, 12.0),
        );
        let mut binner = TileBinner::new(256, 256);
        binner.bin(&[Segment::Cubic(c)], TOL);
        for (_, segs) in binner.tiles() {
            assert_eq!(segs, &[0], "tile listed segment 0 more than once");
        }
    }

    #[test]
    fn empty_tiles_are_not_listed() {
        let mut binner = TileBinner::new(256, 256);
        binner.bin(
            &[Segment::Line(Point::new(1.0, 1.0), Point::new(2.0, 2.0))],
            TOL,
        );
        assert_eq!(bins(&binner), vec![(0, vec![0])]);
        assert_eq!(binner.grid(), (16, 16));
    }

    #[test]
    fn offscreen_segments_bin_nowhere() {
        let mut binner = TileBinner::new(256, 256);
        for seg in [
            // Entirely left, entirely below, and vast in both directions.
            Segment::Line(Point::new(-500.0, 100.0), Point::new(-10.0, 100.0)),
            Segment::Line(Point::new(100.0, 400.0), Point::new(200.0, 900.0)),
            Segment::Line(Point::new(-1e9, -1e9), Point::new(-1e8, -1e9)),
            // Non-finite input must be dropped, not walked.
            Segment::Line(Point::new(f64::NAN, 1.0), Point::new(2.0, 2.0)),
            Segment::Line(Point::new(f64::INFINITY, 1.0), Point::new(2.0, 2.0)),
        ] {
            binner.bin(&[seg], TOL);
            assert_eq!(binner.tiles().count(), 0, "{seg:?} should bin nowhere");
        }
    }

    #[test]
    fn a_segment_crossing_the_viewport_edge_keeps_its_visible_tiles() {
        let mut binner = TileBinner::new(256, 256);
        // Horizontal across the whole width of tile row 0, starting far off to
        // the left: all 16 columns of row 0, nothing else.
        binner.bin(
            &[Segment::Line(Point::new(-1e6, 8.0), Point::new(1e6, 8.0))],
            TOL,
        );
        let got: Vec<u32> = binner.tiles().map(|(t, _)| t).collect();
        assert_eq!(got, (0..16).collect::<Vec<u32>>());
    }

    #[test]
    fn a_boundary_line_belongs_to_the_tile_below_it() {
        let mut binner = TileBinner::new(64, 64);
        // Exactly along y = 16, the top edge of tile row 1.
        binner.bin(
            &[Segment::Line(Point::new(2.0, 16.0), Point::new(6.0, 16.0))],
            TOL,
        );
        assert_eq!(bins(&binner), vec![(4, vec![0])]);
    }

    #[test]
    fn tiles_and_segments_come_back_ascending() {
        let mut binner = TileBinner::new(128, 128);
        // Three segments, deliberately covering overlapping tiles in an order
        // that would expose an unstable sort.
        let segs = [
            Segment::Line(Point::new(100.0, 100.0), Point::new(10.0, 10.0)),
            Segment::Line(Point::new(10.0, 100.0), Point::new(100.0, 10.0)),
            Segment::Line(Point::new(50.0, 5.0), Point::new(50.0, 120.0)),
        ];
        binner.bin(&segs, TOL);
        let got = bins(&binner);
        assert!(
            got.windows(2).all(|w| w[0].0 < w[1].0),
            "tiles not ascending"
        );
        assert!(
            got.iter().all(|(_, s)| s.windows(2).all(|w| w[0] < w[1])),
            "segment lists not ascending and de-duplicated"
        );
        assert!(
            got.iter().any(|(_, s)| s.len() == 3),
            "no tile got all three"
        );
    }

    #[test]
    fn binning_is_stable_across_runs_and_across_binners() {
        let mut rng = Rng::new(7);
        let segs: Vec<Segment> = (0..200)
            .map(|i| match i % 3 {
                0 => Segment::Line(screen_point(&mut rng), screen_point(&mut rng)),
                1 => Segment::Quad(QuadBez::new(
                    screen_point(&mut rng),
                    screen_point(&mut rng),
                    screen_point(&mut rng),
                )),
                _ => Segment::Cubic(CubicBez::new(
                    screen_point(&mut rng),
                    screen_point(&mut rng),
                    screen_point(&mut rng),
                    screen_point(&mut rng),
                )),
            })
            .collect();

        let mut a = TileBinner::new(256, 256);
        a.bin(&segs, TOL);
        let first = bins(&a);
        // Re-binning the same input on a dirty binner must not inherit
        // anything from the previous frame.
        a.bin(&segs[..1], TOL);
        a.bin(&segs, TOL);
        assert_eq!(bins(&a), first);

        let mut b = TileBinner::new(256, 256);
        b.bin(&segs, TOL);
        assert_eq!(bins(&b), first);
        assert!(
            first.len() > 50,
            "test data covers too few tiles to mean much"
        );
    }

    #[test]
    fn a_degenerate_viewport_bins_nothing() {
        let mut binner = TileBinner::new(0, 0);
        binner.bin(
            &[Segment::Line(Point::new(1.0, 1.0), Point::new(2.0, 2.0))],
            TOL,
        );
        assert_eq!(binner.tiles().count(), 0);
    }
}
