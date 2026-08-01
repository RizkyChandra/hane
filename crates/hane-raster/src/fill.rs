//! Scanline anti-aliased fill by the nonzero winding rule (D-002).
//!
//! # The algorithm, and why this one
//!
//! Every path is flattened to a list of straight edges. For each pixel row the
//! rasterizer walks [`SUB_ROWS`] horizontal sample lines through it. On each
//! line it intersects every edge, sorts the crossings by `x`, accumulates the
//! winding number left to right, and adds the spans where that number is
//! nonzero into a per-row coverage accumulator. Horizontal coverage is
//! *analytic*: a span ending at `x = 4.25` gives pixel 4 exactly a quarter.
//!
//! So there is exactly one approximation in the whole pipeline -- the vertical
//! sampling -- and one place where an off-by-one could hide. That is the point.
//! This is the oracle every GPU render in P2 is diffed against, and a subtle
//! bug here would be attributed to the GPU code for months. Anything cleverer
//! (signed-area accumulation, active edge tables, incremental `x` stepping)
//! buys speed and costs the ability to read the code and see that it is right.
//!
//! ponytail: every edge is tested against every sample line, so a fill is
//! `O(rows * SUB_ROWS * edges)`. An active edge table sorted by `y` is the
//! upgrade, and it is deliberately not taken -- correctness is this crate's
//! entire job, and P2 exists because the fast path belongs on the GPU.
//!
//! # Why the vertical sampling is better than it sounds
//!
//! Sample lines sit at the *midpoint* of each sub-band. The midpoint rule is
//! exact for a linear integrand, and the total span length across a scanline is
//! linear in `y` between vertices. So the area a straight edge contributes is
//! integrated exactly; error enters only through sub-bands that contain a
//! vertex, bounded by `h^2/8` times the change in slope there. That is what
//! makes the 1% area criterion hold with room to spare rather than by luck.
//!
//! # Half-open in y
//!
//! An edge covers `[top.y, bot.y)`. A vertex shared by two edges therefore
//! contributes exactly one crossing, not zero and not two. Getting this wrong
//! is the classic source of a one-pixel bleed out of a scanline fill, and of
//! spurious pinholes where two edges meet.

use hane_geom::{CubicBez, PathEl, Point, QuadBez};

/// Vertical sample lines per pixel row.
///
/// Four is what a selection lasso can live with; this is the correctness
/// oracle, so it pays 4x for it. That is not a taste call -- the acceptance
/// criterion is total coverage within 1% of analytic area, and on a disc of
/// radius 1.5, four sub-rows measures 1.17% while sixteen measures 0.28%. The
/// test `total_coverage_matches_analytic_area` is what says so.
///
/// A power of two, so the weight below is exact in binary and `SUB_ROWS` of
/// them sum to exactly 1.0 -- which is what makes a fully covered pixel land on
/// 255 rather than 254.
const SUB_ROWS: u32 = 16;

/// The coverage one fully covered sample line contributes.
const SUB_WEIGHT: f64 = 1.0 / SUB_ROWS as f64;

/// Flattening tolerance, in pixels.
///
/// Not a parameter: an oracle that renders the same path differently depending
/// on how it was asked is not an oracle. A chord this close to its curve costs
/// about `1.33 * tolerance / radius` of the area of a circle -- 0.13% at radius
/// one -- which keeps the flattener well clear of the 1% area budget even for
/// shapes only a pixel or two across.
const FLATTEN_TOLERANCE: f64 = 1e-3;

/// A straight fill edge, oriented so that `top.y < bot.y`.
///
/// `dir` remembers which way the path actually ran through it, because that is
/// what the winding number counts. Sorting the endpoints up front is what makes
/// the crossing test a single half-open range check.
struct Edge {
    top: Point,
    bot: Point,
    dir: i32,
}

/// A straight (non-premultiplied) 8-bit RGBA colour.
///
/// Straight rather than premultiplied because this is what a caller has: a
/// colour and an opacity. The premultiplication happens once, inside the
/// blend, where the coverage is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, where 255 is opaque.
    pub a: u8,
}

/// A rectangular 8-bit RGBA raster target.
///
/// Pixels are **premultiplied**, row-major from the top left, four bytes each.
/// Premultiplied is the form that composites correctly without a divide, and
/// P2 diffs raw bytes against this buffer, so the storage form is part of the
/// contract rather than an implementation detail.
#[derive(Clone, Debug)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Pixmap {
    /// A transparent pixmap of `width` by `height` pixels.
    ///
    /// Panics if the buffer would not fit in a `usize`, which on a 32-bit
    /// target -- `wasm32`, the one that ships -- is reachable with a large
    /// enough canvas. Wrapping the multiply instead would hand back a buffer
    /// too small for its own dimensions.
    pub fn new(width: u32, height: u32) -> Self {
        let len = usize::try_from(u64::from(width) * u64::from(height) * 4)
            .expect("pixmap is larger than this target's address space");
        Self {
            width,
            height,
            data: vec![0; len],
        }
    }

    /// The width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixel bytes: premultiplied RGBA, row-major from the top left.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Fills `path` with `color`, anti-aliased, by the nonzero winding rule.
    ///
    /// Every subpath is closed implicitly, whether or not it ends in
    /// [`PathEl::ClosePath`] -- an open subpath has no inside otherwise, and
    /// every renderer agrees on this for fills.
    ///
    /// Coordinates are in pixels, with the pixel `(i, j)` covering
    /// `[i, i+1) x [j, j+1)`. Anything outside the pixmap is clipped away.
    pub fn fill_path(&mut self, path: &[PathEl], color: Color) {
        let edges = build_edges(path);
        if edges.is_empty() || self.width == 0 {
            return;
        }

        // Only the rows the path can reach. `floor`/`ceil` because a row is
        // touched as soon as any part of it is, and both ends are clamped to
        // the pixmap before the cast so the range is always valid.
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for e in &edges {
            lo = lo.min(e.top.y);
            hi = hi.max(e.bot.y);
        }
        let height = f64::from(self.height);
        let y0 = lo.floor().clamp(0.0, height) as u32;
        let y1 = hi.ceil().clamp(0.0, height) as u32;

        let width = self.width as usize;
        let mut acc = vec![0.0; width];
        let mut crossings = Vec::new();

        for y in y0..y1 {
            coverage_row(&edges, y, &mut acc, &mut crossings);
            let row = y as usize * width * 4;
            for (x, &cov) in acc.iter().enumerate() {
                if cov <= 0.0 {
                    continue;
                }
                let i = row + x * 4;
                blend(&mut self.data[i..i + 4], color, cov);
            }
        }
    }
}

/// Flattens a path into fill edges.
///
/// Curves are flattened to within [`FLATTEN_TOLERANCE`]; the flattener emits
/// both endpoints bit-exactly, so consecutive segments meet with no gap for a
/// crossing to leak through.
fn build_edges(path: &[PathEl]) -> Vec<Edge> {
    let mut out = Vec::new();
    let mut poly = Vec::new();
    let mut cur = Point::ORIGIN;
    let mut start = Point::ORIGIN;

    for &el in path {
        match el {
            PathEl::MoveTo(p) => {
                // Close whatever came before. Before the first subpath that is
                // ORIGIN to ORIGIN, which `push_edge` drops as horizontal, so
                // no "is a subpath open" flag is needed.
                push_edge(&mut out, cur, start);
                start = p;
                cur = p;
            }
            PathEl::LineTo(p) => {
                push_edge(&mut out, cur, p);
                cur = p;
            }
            PathEl::QuadTo(c, p) => {
                poly.clear();
                QuadBez::new(cur, c, p).flatten(FLATTEN_TOLERANCE, &mut poly);
                push_polyline(&mut out, &poly);
                cur = p;
            }
            PathEl::CurveTo(c0, c1, p) => {
                poly.clear();
                CubicBez::new(cur, c0, c1, p).flatten(FLATTEN_TOLERANCE, &mut poly);
                push_polyline(&mut out, &poly);
                cur = p;
            }
            PathEl::ClosePath => {
                push_edge(&mut out, cur, start);
                cur = start;
            }
        }
    }
    push_edge(&mut out, cur, start);
    out
}

fn push_polyline(out: &mut Vec<Edge>, poly: &[Point]) {
    for w in poly.windows(2) {
        push_edge(out, w[0], w[1]);
    }
}

fn push_edge(out: &mut Vec<Edge>, a: Point, b: Point) {
    // A horizontal edge crosses no sample line and contributes nothing. A
    // non-finite one would put a phantom crossing into the sort and fill half a
    // scanline with it, so a malformed path loses its bad edges and keeps the
    // rest rather than poisoning the whole shape.
    if a.y == b.y || !a.is_finite() || !b.is_finite() {
        return;
    }
    let (top, bot, dir) = if a.y < b.y { (a, b, 1) } else { (b, a, -1) };
    out.push(Edge { top, bot, dir });
}

/// Accumulates coverage in `[0, 1]` for pixel row `y` into `acc`, which is
/// overwritten rather than added to.
///
/// `crossings` is scratch owned by the caller only to keep an allocation out of
/// the row loop.
fn coverage_row(edges: &[Edge], y: u32, acc: &mut [f64], crossings: &mut Vec<(f64, i32)>) {
    acc.fill(0.0);
    for sub in 0..SUB_ROWS {
        let sy = f64::from(y) + (f64::from(sub) + 0.5) * SUB_WEIGHT;

        crossings.clear();
        for e in edges {
            // Half-open: see the module comment.
            if sy < e.top.y || sy >= e.bot.y {
                continue;
            }
            let t = (sy - e.top.y) / (e.bot.y - e.top.y);
            // The symmetric lerp, whose weights are exactly 1 and 0 at the
            // ends, so a crossing at a vertex lands on the vertex.
            crossings.push(((1.0 - t) * e.top.x + t * e.bot.x, e.dir));
        }
        // `total_cmp`, not `partial_cmp().unwrap()`: the edges are finite so a
        // NaN cannot arrive, and a total order means no version of this can
        // ever panic on the sort.
        crossings.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mut winding = 0;
        for w in crossings.windows(2) {
            winding += w[0].1;
            // The nonzero rule, and the single line that even-odd (#12)
            // replaces. Spans between consecutive crossings are disjoint by
            // construction, so coverage can never double-count a pixel.
            if winding == 0 {
                continue;
            }
            add_span(acc, w[0].0, w[1].0, SUB_WEIGHT);
        }
    }
}

/// Adds the horizontal span `[a, b)` to one row at `weight`, the partial pixels
/// at each end carrying their exact fraction.
fn add_span(acc: &mut [f64], a: f64, b: f64, weight: f64) {
    let a = a.max(0.0);
    let b = b.min(acc.len() as f64);
    if b <= a {
        return;
    }
    // `b <= acc.len()` and `b > a >= 0`, so both indices are in range.
    let first = a.floor() as usize;
    let last = b.ceil() as usize - 1;
    for (i, cell) in acc[first..=last].iter_mut().enumerate() {
        let px = (first + i) as f64;
        let lo = a.max(px);
        let hi = b.min(px + 1.0);
        if hi > lo {
            *cell += weight * (hi - lo);
        }
    }
}

/// Source-over of `color` at `coverage` onto one premultiplied pixel.
fn blend(dst: &mut [u8], color: Color, coverage: f64) {
    // Clamped because two spans meeting exactly inside a pixel can sum to an
    // ulp over one. Unclamped that makes the blend below a slightly *more* than
    // convex combination, which can brighten a pixel past the colour it was
    // filled with -- invisible in a byte, but this is an oracle and P2 diffs it.
    let a = f64::from(color.a) / 255.0 * coverage.clamp(0.0, 1.0);
    let src = [color.r, color.g, color.b, 255];
    for (d, s) in dst.iter_mut().zip(src) {
        // Rounding, not truncating. It is what makes full coverage of an opaque
        // colour land on exactly that colour, and 254/255 the commonest
        // off-by-one in a rasterizer.
        *d = (f64::from(s) * a + f64::from(*d) * (1.0 - a)).round() as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::check;

    const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    /// The alpha byte at `(x, y)`, which for an opaque fill onto a transparent
    /// pixmap *is* the coverage byte.
    fn cov(pm: &Pixmap, x: u32, y: u32) -> u8 {
        pm.data()[((y * pm.width() + x) * 4 + 3) as usize]
    }

    /// Total coverage over the whole pixmap, in pixels.
    fn total(pm: &Pixmap) -> f64 {
        pm.data()
            .iter()
            .skip(3)
            .step_by(4)
            .map(|&a| f64::from(a) / 255.0)
            .sum()
    }

    fn poly(points: &[(f64, f64)]) -> Vec<PathEl> {
        let mut els = vec![PathEl::MoveTo(Point::new(points[0].0, points[0].1))];
        els.extend(
            points[1..]
                .iter()
                .map(|&(x, y)| PathEl::LineTo(Point::new(x, y))),
        );
        els.push(PathEl::ClosePath);
        els
    }

    fn filled(w: u32, h: u32, path: &[PathEl]) -> Pixmap {
        let mut pm = Pixmap::new(w, h);
        pm.fill_path(path, WHITE);
        pm
    }

    /// The exact area of a simple polygon, by the shoelace formula. Only valid
    /// where the polygon does not cross itself -- which is exactly where it is
    /// used below.
    fn shoelace(points: &[(f64, f64)]) -> f64 {
        let mut sum = 0.0;
        for i in 0..points.len() {
            let (x0, y0) = points[i];
            let (x1, y1) = points[(i + 1) % points.len()];
            sum += x0 * y1 - x1 * y0;
        }
        (sum / 2.0).abs()
    }

    /// A regular `n`-gon, counter-clockwise unless `reverse`.
    ///
    /// Used instead of a circle wherever an *exact* area is needed: the
    /// polygon's area is known in closed form, so the measurement tests the
    /// rasterizer rather than the flattener or the cubic circle approximation.
    fn ngon(cx: f64, cy: f64, r: f64, n: u32, reverse: bool) -> Vec<(f64, f64)> {
        let mut out: Vec<(f64, f64)> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * f64::from(i) / f64::from(n);
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        if reverse {
            out.reverse();
        }
        out
    }

    // ---------------------------------------------------------- axis-aligned

    #[test]
    fn a_pixel_aligned_rect_fills_exactly_with_no_antialiasing() {
        let pm = filled(
            8,
            8,
            &poly(&[(2.0, 1.0), (6.0, 1.0), (6.0, 5.0), (2.0, 5.0)]),
        );
        for y in 0..8 {
            for x in 0..8 {
                let inside = (2..6).contains(&x) && (1..5).contains(&y);
                assert_eq!(
                    cov(&pm, x, y),
                    if inside { 255 } else { 0 },
                    "at ({x}, {y})"
                );
            }
        }
        // And the colour itself survives full coverage untouched.
        assert_eq!(&pm.data()[(3 * 8 + 2) * 4..(3 * 8 + 2) * 4 + 4], &[255; 4]);
    }

    #[test]
    fn a_unit_square_is_one_pixel() {
        let pm = filled(
            4,
            4,
            &poly(&[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0)]),
        );
        assert_eq!(cov(&pm, 1, 1), 255);
        assert!((total(&pm) - 1.0).abs() < 1e-12, "{}", total(&pm));
    }

    #[test]
    fn an_axis_aligned_edge_off_the_pixel_grid_is_a_clean_fraction() {
        // x from 1.25 to 2.75: pixel 1 gets 3/4, pixel 2 gets 3/4.
        let pm = filled(
            4,
            4,
            &poly(&[(1.25, 0.0), (2.75, 0.0), (2.75, 4.0), (1.25, 4.0)]),
        );
        for y in 0..4 {
            assert_eq!(cov(&pm, 0, y), 0);
            assert_eq!(cov(&pm, 1, y), 191); // round(0.75 * 255) = 191
            assert_eq!(cov(&pm, 2, y), 191);
            assert_eq!(cov(&pm, 3, y), 0);
        }
        // y from 0.25 to 3.75 likewise, on a column this time.
        let pm = filled(
            4,
            4,
            &poly(&[(0.0, 0.25), (4.0, 0.25), (4.0, 3.75), (0.0, 3.75)]),
        );
        assert_eq!(cov(&pm, 0, 0), 191);
        assert_eq!(cov(&pm, 0, 1), 255);
        assert_eq!(cov(&pm, 0, 3), 191);
    }

    // ------------------------------------------------------------- the ramp

    #[test]
    fn a_45_degree_edge_produces_the_analytic_ramp() {
        // The triangle below y = x inside an 8x8 box. The pixel on the diagonal
        // is cut exactly in half, and every pixel below it is full.
        let pm = filled(8, 8, &poly(&[(0.0, 0.0), (8.0, 8.0), (0.0, 8.0)]));
        for k in 0..8u32 {
            // Exactly half: 16 midpoint samples of a linear ramp average to
            // 1/2 with no rounding at all, so this is 128, not 127 or 129.
            assert_eq!(cov(&pm, k, k), 128, "diagonal pixel {k}");
            for x in 0..k {
                assert_eq!(cov(&pm, x, k), 255, "interior ({x}, {k})");
            }
            for x in (k + 1)..8 {
                assert_eq!(cov(&pm, x, k), 0, "exterior ({x}, {k})");
            }
        }
        // The analytic area is 32: 28 whole pixels below the diagonal plus 8
        // half ones on it. The only departure is the byte itself -- a half
        // rounds to 128, not 127.5 -- so the total is 28 + 8 * 128/255, exactly.
        let want = 28.0 + 8.0 * 128.0 / 255.0;
        assert!((total(&pm) - want).abs() < 1e-9, "{} vs {want}", total(&pm));
    }

    #[test]
    fn a_shallow_edge_ramps_monotonically() {
        // A 4:1 slope, where each successive pixel along the edge takes a
        // predictable step. Nothing here should be flat or reverse.
        let pm = filled(16, 8, &poly(&[(0.0, 0.0), (16.0, 4.0), (0.0, 4.0)]));
        let row: Vec<u8> = (0..16).map(|x| cov(&pm, x, 1)).collect();
        for w in row.windows(2) {
            assert!(w[1] <= w[0], "coverage must fall to the right: {row:?}");
        }
        assert_eq!(row[0], 255);
        assert_eq!(row[15], 0);
    }

    // ---------------------------------------------------------- nonzero rule

    #[test]
    fn nonzero_fills_a_doubly_wound_overlap_solid() {
        // Two squares wound the same way. Their overlap has winding 2, which
        // the nonzero rule fills; even-odd (#12) would punch a hole in it.
        let mut path = poly(&[(1.0, 1.0), (7.0, 1.0), (7.0, 7.0), (1.0, 7.0)]);
        path.extend(poly(&[(4.0, 4.0), (10.0, 4.0), (10.0, 10.0), (4.0, 10.0)]));
        let pm = filled(12, 12, &path);
        assert_eq!(cov(&pm, 5, 5), 255, "the overlap must be solid");
        assert_eq!(cov(&pm, 2, 2), 255);
        assert_eq!(cov(&pm, 8, 8), 255);
        assert_eq!(cov(&pm, 9, 2), 0);
    }

    #[test]
    fn a_self_intersecting_pentagram_fills_solid() {
        // The literal figure-eight case: one closed subpath that crosses
        // itself, whose centre has winding 2. Under nonzero the star is solid;
        // under even-odd the middle pentagon would be empty.
        let r = 20.0;
        let pts: Vec<(f64, f64)> = (0..5)
            .map(|i| {
                // Every second vertex, which is what turns a pentagon into a
                // pentagram traced in one stroke.
                let a =
                    std::f64::consts::TAU * f64::from(i) * 2.0 / 5.0 - std::f64::consts::FRAC_PI_2;
                (25.0 + r * a.cos(), 25.0 + r * a.sin())
            })
            .collect();
        let pm = filled(50, 50, &poly(&pts));
        assert_eq!(cov(&pm, 25, 25), 255, "the centre must be solid");
        // A point inside a star arm but outside the middle pentagon.
        assert_eq!(cov(&pm, 25, 8), 255);
        // A notch between two arms.
        assert_eq!(cov(&pm, 5, 5), 0);
    }

    #[test]
    fn concentric_circles_wound_oppositely_leave_a_hole() {
        let mut path = poly(&ngon(25.0, 25.0, 20.0, 64, false));
        path.extend(poly(&ngon(25.0, 25.0, 10.0, 64, true)));
        let pm = filled(50, 50, &path);
        assert_eq!(cov(&pm, 25, 25), 0, "opposite winding must leave a hole");
        assert_eq!(cov(&pm, 25, 10), 255, "the ring itself must be solid");

        // Total coverage is the annulus, not the disc.
        let want = shoelace(&ngon(25.0, 25.0, 20.0, 64, false))
            - shoelace(&ngon(25.0, 25.0, 10.0, 64, false));
        let got = total(&pm);
        assert!((got - want).abs() / want < 0.01, "{got} vs {want}");
    }

    #[test]
    fn concentric_circles_wound_alike_do_not() {
        let mut path = poly(&ngon(25.0, 25.0, 20.0, 64, false));
        path.extend(poly(&ngon(25.0, 25.0, 10.0, 64, false)));
        let pm = filled(50, 50, &path);
        assert_eq!(cov(&pm, 25, 25), 255, "same winding must stay solid");
        let want = shoelace(&ngon(25.0, 25.0, 20.0, 64, false));
        let got = total(&pm);
        assert!((got - want).abs() / want < 0.01, "{got} vs {want}");
    }

    #[test]
    fn winding_direction_does_not_change_the_result() {
        let pts = ngon(16.0, 16.0, 12.0, 7, false);
        let a = filled(32, 32, &poly(&pts));
        let mut rev = pts.clone();
        rev.reverse();
        let b = filled(32, 32, &poly(&rev));
        assert_eq!(a.data(), b.data());
    }

    // ------------------------------------------------------- exact endpoints

    #[test]
    fn coverage_is_exact_at_zero_and_full() {
        // A shape with every kind of edge: axis-aligned, diagonal, and curved.
        let path = vec![
            PathEl::MoveTo(Point::new(4.0, 4.0)),
            PathEl::LineTo(Point::new(28.0, 4.0)),
            PathEl::CurveTo(
                Point::new(34.0, 12.0),
                Point::new(34.0, 20.0),
                Point::new(28.0, 28.0),
            ),
            PathEl::LineTo(Point::new(10.0, 22.0)),
            PathEl::ClosePath,
        ];
        let pm = filled(40, 40, &path);
        // Deep interior: exactly full, never 254.
        assert_eq!(cov(&pm, 16, 12), 255);
        // Far outside: exactly empty, never 1.
        assert_eq!(cov(&pm, 38, 38), 0);
        assert_eq!(cov(&pm, 1, 1), 0);
        // No pixel outside the path's own bounding box is touched at all.
        for y in 0..40 {
            for x in 0..40 {
                if !(3..=35).contains(&x) || !(3..=29).contains(&y) {
                    assert_eq!(cov(&pm, x, y), 0, "bled to ({x}, {y})");
                }
            }
        }
        // Both extremes actually occur, so the assertions above are not vacuous.
        assert!(pm.data().iter().skip(3).step_by(4).any(|&a| a == 255));
        assert!(pm.data().iter().skip(3).step_by(4).any(|&a| a == 0));
    }

    #[test]
    fn a_pixmap_sized_rect_saturates_every_pixel() {
        // The whole canvas, edges exactly on the boundary. Nothing may clip to
        // 254, and nothing may fall off the far edge.
        let pm = filled(
            16,
            16,
            &poly(&[(0.0, 0.0), (16.0, 0.0), (16.0, 16.0), (0.0, 16.0)]),
        );
        assert!(
            pm.data().iter().all(|&b| b == 255),
            "a full-canvas fill must be uniformly opaque"
        );
    }

    // ------------------------------------------------------ sub-pixel shapes

    #[test]
    fn sub_pixel_shapes_render_at_proportional_coverage() {
        for &side in &[0.5, 0.25, 0.125] {
            let pm = filled(
                4,
                4,
                &poly(&[
                    (1.0, 1.0),
                    (1.0 + side, 1.0),
                    (1.0 + side, 1.0 + side),
                    (1.0, 1.0 + side),
                ]),
            );
            let got = f64::from(cov(&pm, 1, 1)) / 255.0;
            assert!(got > 0.0, "a {side}-pixel square vanished");
            // Within the 1/16 vertical quantum plus a byte of rounding.
            assert!(
                (got - side * side).abs() < side * SUB_WEIGHT + 1.0 / 255.0,
                "{side}: got {got}, want {}",
                side * side
            );
        }
    }

    #[test]
    fn a_sub_pixel_shape_spanning_a_pixel_boundary_splits_between_them() {
        // Half a pixel wide, straddling x = 2. Each side gets a quarter.
        let pm = filled(
            4,
            4,
            &poly(&[(1.75, 0.0), (2.25, 0.0), (2.25, 4.0), (1.75, 4.0)]),
        );
        assert_eq!(cov(&pm, 1, 2), 64); // round(0.25 * 255)
        assert_eq!(cov(&pm, 2, 2), 64);
        assert_eq!(cov(&pm, 3, 2), 0);
    }

    // ----------------------------------------------------------- total area

    #[test]
    fn total_coverage_matches_analytic_area() {
        // Regular polygons over two decades of size, at offsets that put the
        // vertices nowhere near the pixel grid. The comparison is against the
        // shoelace area of the polygon actually submitted, so this measures the
        // rasterizer and nothing else.
        let mut worst: f64 = 0.0;
        for &(r, n) in &[
            (1.5, 32u32),
            (2.5, 32),
            (5.0, 48),
            (12.0, 64),
            (40.0, 128),
            (2.0, 3),
            (7.0, 5),
        ] {
            for &(dx, dy) in &[(0.0, 0.0), (0.37, 0.11), (0.5, 0.5), (0.93, 0.64)] {
                let c = 64.0 + dx;
                let pts = ngon(c, 64.0 + dy, r, n, false);
                let pm = filled(128, 128, &poly(&pts));
                let want = shoelace(&pts);
                let err = (total(&pm) - want).abs() / want;
                worst = worst.max(err);
                assert!(
                    err < 0.01,
                    "r={r} n={n} offset=({dx}, {dy}): {} vs {want} ({:.3}%)",
                    total(&pm),
                    err * 100.0
                );
            }
        }
        // Not merely under 1% -- comfortably so. The measured worst case is
        // 0.28%, on the smallest disc here; at four sub-rows it would be 1.17%
        // and the criterion above would fail outright. This tighter bound is
        // the tripwire that says so before that happens.
        assert!(
            worst < 0.005,
            "worst relative area error {:.4}%",
            worst * 100.0
        );
    }

    #[test]
    fn a_rotated_square_keeps_its_area() {
        // 30 degrees, so no edge is axis-aligned or diagonal.
        let (c, s) = (30f64.to_radians().cos(), 30f64.to_radians().sin());
        let pts: Vec<(f64, f64)> = [(-10.0, -10.0), (10.0, -10.0), (10.0, 10.0), (-10.0, 10.0)]
            .iter()
            .map(|&(x, y): &(f64, f64)| (32.0 + x * c - y * s, 32.0 + x * s + y * c))
            .collect();
        let pm = filled(64, 64, &poly(&pts));
        let err = (total(&pm) - 400.0).abs() / 400.0;
        assert!(err < 0.01, "{} vs 400 ({:.3}%)", total(&pm), err * 100.0);
    }

    #[test]
    fn a_cubic_circle_keeps_its_area() {
        // Curves, not lines: four cubics with the standard kappa. That
        // approximation is itself about 0.02% small in radius, so this is a
        // looser bound than the polygon tests by construction.
        let (cx, cy, r) = (32.0, 32.0, 25.0);
        let k = 0.552_284_749_830_793_6 * r;
        let path = vec![
            PathEl::MoveTo(Point::new(cx + r, cy)),
            PathEl::CurveTo(
                Point::new(cx + r, cy + k),
                Point::new(cx + k, cy + r),
                Point::new(cx, cy + r),
            ),
            PathEl::CurveTo(
                Point::new(cx - k, cy + r),
                Point::new(cx - r, cy + k),
                Point::new(cx - r, cy),
            ),
            PathEl::CurveTo(
                Point::new(cx - r, cy - k),
                Point::new(cx - k, cy - r),
                Point::new(cx, cy - r),
            ),
            PathEl::CurveTo(
                Point::new(cx + k, cy - r),
                Point::new(cx + r, cy - k),
                Point::new(cx + r, cy),
            ),
            PathEl::ClosePath,
        ];
        let pm = filled(64, 64, &path);
        let want = std::f64::consts::PI * r * r;
        let err = (total(&pm) - want).abs() / want;
        assert!(err < 0.01, "{} vs {want} ({:.3}%)", total(&pm), err * 100.0);
        assert_eq!(cov(&pm, 32, 32), 255);
        assert_eq!(cov(&pm, 2, 2), 0);
    }

    #[test]
    fn a_quadratic_and_the_cubic_it_elevates_to_agree() {
        let (a, c, b) = (
            Point::new(5.0, 30.0),
            Point::new(30.0, 2.0),
            Point::new(55.0, 30.0),
        );
        let quad = vec![PathEl::MoveTo(a), PathEl::QuadTo(c, b), PathEl::ClosePath];
        let q = QuadBez::new(a, c, b).to_cubic();
        let cubic = vec![
            PathEl::MoveTo(a),
            PathEl::CurveTo(q.p1, q.p2, b),
            PathEl::ClosePath,
        ];
        assert_eq!(filled(64, 40, &quad).data(), filled(64, 40, &cubic).data());
    }

    // ------------------------------------------------------------- blending

    #[test]
    fn a_translated_shape_keeps_its_area() {
        // Translation by a non-integer amount redistributes coverage between
        // pixels but must not create or destroy any.
        let base = ngon(32.0, 32.0, 11.0, 24, false);
        let want = shoelace(&base);
        for &d in &[0.0, 0.1, 0.3, 0.5, 0.7, 0.9] {
            let pts: Vec<(f64, f64)> = base.iter().map(|&(x, y)| (x + d, y + d)).collect();
            let got = total(&filled(64, 64, &poly(&pts)));
            assert!((got - want).abs() / want < 0.01, "d={d}: {got} vs {want}");
        }
    }

    #[test]
    fn a_translucent_fill_composites_source_over() {
        let half = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 128,
        };
        let square = poly(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]);
        let mut pm = Pixmap::new(4, 4);
        pm.fill_path(&square, half);
        // Premultiplied: red and alpha both 128 * 1.0.
        assert_eq!(&pm.data()[0..4], &[128, 0, 0, 128]);
        // Over itself: 128 + 128 * (1 - 128/255) = 191.5 -> 192.
        pm.fill_path(&square, half);
        assert_eq!(&pm.data()[0..4], &[192, 0, 0, 192]);
        // An opaque fill on top replaces it exactly.
        pm.fill_path(&square, WHITE);
        assert_eq!(&pm.data()[0..4], &[255; 4]);
    }

    #[test]
    fn a_fully_transparent_colour_changes_nothing() {
        let mut pm = Pixmap::new(4, 4);
        pm.fill_path(
            &poly(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]),
            Color {
                r: 255,
                g: 255,
                b: 255,
                a: 0,
            },
        );
        assert!(pm.data().iter().all(|&b| b == 0));
    }

    // ------------------------------------------------------------ degenerate

    #[test]
    fn degenerate_input_is_harmless() {
        let cases: Vec<Vec<PathEl>> = vec![
            vec![],
            vec![PathEl::ClosePath],
            vec![PathEl::MoveTo(Point::new(1.0, 1.0))],
            // A single point, and a zero-area horizontal sliver.
            poly(&[(1.0, 1.0), (1.0, 1.0), (1.0, 1.0)]),
            poly(&[(0.0, 2.0), (4.0, 2.0), (2.0, 2.0)]),
            // Entirely off the canvas, in every direction.
            poly(&[(-50.0, -50.0), (-40.0, -50.0), (-40.0, -40.0)]),
            poly(&[(100.0, 100.0), (110.0, 100.0), (110.0, 110.0)]),
            // Non-finite coordinates: the bad edges drop, nothing panics.
            poly(&[(f64::NAN, 1.0), (3.0, 1.0), (3.0, 3.0)]),
            poly(&[(f64::INFINITY, 1.0), (3.0, 1.0), (3.0, 3.0)]),
        ];
        for path in &cases {
            let pm = filled(8, 8, path);
            assert!(total(&pm) < 8.0, "{path:?} filled unexpectedly much");
        }
        // A zero-sized pixmap must not index anything.
        let mut pm = Pixmap::new(0, 0);
        pm.fill_path(&poly(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0)]), WHITE);
        assert!(pm.data().is_empty());
    }

    #[test]
    fn a_path_hanging_off_the_edge_is_clipped_not_wrapped() {
        // Half on, half off, on all four sides at once.
        let pm = filled(
            8,
            8,
            &poly(&[(-4.0, -4.0), (4.0, -4.0), (4.0, 4.0), (-4.0, 4.0)]),
        );
        for y in 0..8 {
            for x in 0..8 {
                let inside = x < 4 && y < 4;
                assert_eq!(cov(&pm, x, y), if inside { 255 } else { 0 }, "({x}, {y})");
            }
        }
        assert!((total(&pm) - 16.0).abs() < 1e-9);
    }

    #[test]
    fn an_unclosed_subpath_is_closed_implicitly() {
        let open = vec![
            PathEl::MoveTo(Point::new(1.0, 1.0)),
            PathEl::LineTo(Point::new(5.0, 1.0)),
            PathEl::LineTo(Point::new(5.0, 5.0)),
            PathEl::LineTo(Point::new(1.0, 5.0)),
        ];
        let mut closed = open.clone();
        closed.push(PathEl::ClosePath);
        assert_eq!(filled(8, 8, &open).data(), filled(8, 8, &closed).data());
        // And a following MoveTo closes it just as ClosePath would.
        let mut moved = open.clone();
        moved.push(PathEl::MoveTo(Point::new(7.0, 7.0)));
        assert_eq!(filled(8, 8, &moved).data(), filled(8, 8, &closed).data());
    }

    // ------------------------------------------------------------- properties

    #[test]
    fn random_triangles_match_their_shoelace_area() {
        check(
            "triangle area",
            2_000,
            |r| {
                let mut p = || (r.unit() * 48.0 + 4.0, r.unit() * 48.0 + 4.0);
                [p(), p(), p()]
            },
            |&pts| {
                let want = shoelace(&pts);
                // Slivers are excluded on purpose: below a few pixels of area
                // the 1/16 vertical quantum and the 1/255 byte are a large
                // fraction of the answer, and the criterion is about shapes,
                // not about measuring the quantisation.
                if want < 20.0 {
                    return true;
                }
                let got = total(&filled(56, 56, &poly(&pts)));
                (got - want).abs() / want < 0.01
            },
        );
    }

    #[test]
    fn random_polygons_never_exceed_full_coverage() {
        // Total coverage can never beat the bounding box, and no byte can wrap.
        // This is the invariant that a double-counted span would break.
        check(
            "coverage bound",
            1_000,
            |r| {
                let n = 3 + r.below(6) as usize;
                (0..n)
                    .map(|_| (r.unit() * 40.0 - 4.0, r.unit() * 40.0 - 4.0))
                    .collect::<Vec<_>>()
            },
            |pts| {
                let pm = filled(32, 32, &poly(pts));
                let bbox = {
                    let xs = pts.iter().map(|p| p.0);
                    let ys = pts.iter().map(|p| p.1);
                    let (x0, x1) = (
                        xs.clone().fold(f64::INFINITY, f64::min).max(0.0),
                        xs.fold(f64::NEG_INFINITY, f64::max).min(32.0),
                    );
                    let (y0, y1) = (
                        ys.clone().fold(f64::INFINITY, f64::min).max(0.0),
                        ys.fold(f64::NEG_INFINITY, f64::max).min(32.0),
                    );
                    ((x1 - x0).max(0.0) + 2.0) * ((y1 - y0).max(0.0) + 2.0)
                };
                total(&pm) <= bbox
            },
        );
    }

    #[test]
    fn an_integer_translation_shifts_the_image_exactly() {
        // The strongest statement of "no off-by-one": moving a shape by whole
        // pixels must move its raster by whole pixels, bit for bit.
        let base = ngon(20.0, 20.0, 9.0, 13, false);
        let a = filled(48, 48, &poly(&base));
        let shifted: Vec<(f64, f64)> = base.iter().map(|&(x, y)| (x + 5.0, y + 3.0)).collect();
        let b = filled(48, 48, &poly(&shifted));
        for y in 0..45u32 {
            for x in 0..43u32 {
                assert_eq!(cov(&a, x, y), cov(&b, x + 5, y + 3), "({x}, {y})");
            }
        }
    }

    #[test]
    fn subdividing_an_edge_changes_nothing() {
        // Extra collinear vertices must not perturb a single byte -- if the
        // half-open rule were wrong, each one would drop or double a crossing.
        let plain = poly(&[(3.0, 2.5), (17.5, 4.0), (12.0, 19.0)]);
        let dense = poly(&[
            (3.0, 2.5),
            (10.25, 3.25),
            (17.5, 4.0),
            (14.75, 11.5),
            (12.0, 19.0),
            (7.5, 10.75),
        ]);
        assert_eq!(filled(24, 24, &plain).data(), filled(24, 24, &dense).data());
    }
}
