//! Adaptive flattening: a curve to a polyline no further than `tolerance` away.
//!
//! This is the hottest function in the renderer -- every fill, every stroke,
//! every frame -- so the flatness test is written to be a handful of multiplies
//! with no square root and no transcendentals.
//!
//! # The bound is proved, not estimated
//!
//! Distance to a convex set is a convex function, and a Bezier is a convex
//! combination of its control points: `B(t) = sum_i B_i(t) p_i` with weights
//! that are non-negative and sum to one. So for the chord `S` joining the two
//! endpoints,
//!
//! ```text
//! dist(B(t), S)  <=  sum_i B_i(t) dist(p_i, S)
//! ```
//!
//! The endpoints lie *on* `S` and contribute nothing, leaving only the interior
//! control points. For a cubic that is `B_1 + B_2 = 3t(1-t)`, whose maximum is
//! `3/4`; for a quadratic it is `B_1 = 2t(1-t)`, maximum `1/2`. Hence
//!
//! ```text
//! max_t dist(B(t), S)  <=  3/4 max(dist(p_1, S), dist(p_2, S))    cubic
//! max_t dist(B(t), S)  <=  1/2 dist(p_1, S)                       quadratic
//! ```
//!
//! Each edge of the emitted polyline is exactly the chord of the subcurve it
//! came from, so a per-subcurve bound is a bound on the whole thing. Nothing
//! here is a heuristic that happens to work; the sampling tests confirm the
//! algebra rather than establish it.
//!
//! Measuring to the chord *segment* rather than to its infinite line is what
//! makes the degenerate cases safe. A curve that runs out along a line and back
//! -- collinear control points with a zero-length chord -- has zero
//! perpendicular deviation and is emphatically not flat; its control points are
//! far from the segment, so it subdivides.
//!
//! # Why bisection
//!
//! Halving in `t` concentrates points where the curve actually bends, which a
//! single a-priori count (Wang's formula) cannot do, and it terminates on cusps
//! for free: the control points of a subcurve approach its chord quadratically
//! in the parameter step regardless of what the derivative is doing, so a cusp
//! is not a special case at all.

use crate::{CubicBez, Point, QuadBez, Vec2};

/// Recursion cap.
///
/// Only unreachable tolerances get near it -- 16 levels resolve a curve to
/// about `2^-32` of its own size, well past the point where an f64 coordinate
/// carries meaning at any zoom. It exists so that a nonsensical request
/// degrades into a dense polyline (at most `2^16 + 1` points) instead of
/// hanging or blowing the stack.
const MAX_DEPTH: u32 = 16;

/// `(3/4)^2`, the cubic bound above, pre-squared so the test needs no `sqrt`.
const CUBIC_FACTOR: f64 = 0.5625;

/// `(1/2)^2`, the quadratic bound.
const QUAD_FACTOR: f64 = 0.25;

/// Squared distance from `p` to the segment `a`-`b`.
///
/// The segment, not the line through it: see the module comment for the
/// degenerate curve that distinguishes them.
#[inline]
fn dist2_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let ap = p - a;
    let len2 = ab.dot(ab);
    // A zero-length chord is a point, and the projection would be 0/0.
    let t = if len2 > 0.0 {
        (ap.dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let d: Vec2 = ap - ab * t;
    d.dot(d)
}

/// True when the subcurve is within tolerance of its chord, or when there is
/// nothing sensible left to test.
///
/// Written as a negated `>` so that a NaN on either side -- a non-finite
/// control point, or a NaN tolerance -- reports flat and stops. The alternative
/// is a NaN comparing false forever and recursing all the way to the cap for
/// `2^16` points of garbage.
#[inline]
// The negation is the whole point: `a <= b` and `!(a > b)` differ exactly on
// the incomparable case, and the incomparable case is the one being handled.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn within(deviation2: f64, factor: f64, tolerance2: f64) -> bool {
    !(deviation2 * factor > tolerance2)
}

fn flatten_cubic(c: CubicBez, tolerance2: f64, depth: u32, out: &mut Vec<Point>) {
    let d2 = dist2_to_segment(c.p1, c.p0, c.p3).max(dist2_to_segment(c.p2, c.p0, c.p3));
    if depth == 0 || within(d2, CUBIC_FACTOR, tolerance2) {
        out.push(c.p3);
        return;
    }
    // ponytail: plain bisection, so the count lands on a power of two per
    // branch and can exceed the optimal parameterisation by up to 2x. Upgrade
    // to an analytic error-metric parameterisation (Levien's) if profiling ever
    // shows the point count, rather than the traversal, is what costs.
    let (l, r) = c.split(0.5);
    flatten_cubic(l, tolerance2, depth - 1, out);
    flatten_cubic(r, tolerance2, depth - 1, out);
}

fn flatten_quad(q: QuadBez, tolerance2: f64, depth: u32, out: &mut Vec<Point>) {
    let d2 = dist2_to_segment(q.p1, q.p0, q.p2);
    if depth == 0 || within(d2, QUAD_FACTOR, tolerance2) {
        out.push(q.p2);
        return;
    }
    let (l, r) = q.split(0.5);
    flatten_quad(l, tolerance2, depth - 1, out);
    flatten_quad(r, tolerance2, depth - 1, out);
}

impl CubicBez {
    /// Appends a polyline approximating this segment, every point of the curve
    /// within `tolerance` of it.
    ///
    /// Both endpoints are emitted, bit-exact, so a straight line comes back as
    /// exactly two points. A path walker joining segments already has the start
    /// point in hand and should skip the first one.
    ///
    /// `tolerance` is in document units. Flattening is specified in device
    /// pixels, so divide by [`Affine::max_scale`](crate::Affine::max_scale) of
    /// the view transform first: at a 4x zoom, half a pixel of error is an
    /// eighth of a document unit.
    pub fn flatten(self, tolerance: f64, out: &mut Vec<Point>) {
        out.push(self.p0);
        // Squared once here to keep a `sqrt` out of the recursion. The
        // underflow below ~1e-154 turns an already-unsatisfiable tolerance into
        // zero, which the depth cap absorbs.
        flatten_cubic(self, tolerance * tolerance, MAX_DEPTH, out);
    }
}

impl QuadBez {
    /// Appends a polyline approximating this segment, every point of the curve
    /// within `tolerance` of it.
    ///
    /// See [`CubicBez::flatten`]; the only difference is the tighter constant a
    /// quadratic admits, which is why quadratics are not simply promoted to
    /// cubics here.
    pub fn flatten(self, tolerance: f64, out: &mut Vec<Point>) {
        out.push(self.p0);
        flatten_quad(self, tolerance * tolerance, MAX_DEPTH, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Affine;
    use crate::fuzz::{Rng, check};

    /// The worst distance from the curve to the polyline, by dense sampling.
    ///
    /// This is the acceptance criterion itself, computed the slow honest way:
    /// it knows nothing about the subdivision that produced `poly`.
    fn max_deviation(c: CubicBez, poly: &[Point], samples: u32) -> f64 {
        let mut worst = 0.0f64;
        for i in 0..=samples {
            let p = c.eval(f64::from(i) / f64::from(samples));
            let mut best = f64::INFINITY;
            for w in poly.windows(2) {
                best = best.min(dist2_to_segment(p, w[0], w[1]));
            }
            worst = worst.max(best.sqrt());
        }
        worst
    }

    fn quad_max_deviation(q: QuadBez, poly: &[Point], samples: u32) -> f64 {
        max_deviation(q.to_cubic(), poly, samples)
    }

    fn flatten(c: CubicBez, tol: f64) -> Vec<Point> {
        let mut out = Vec::new();
        c.flatten(tol, &mut out);
        out
    }

    /// The largest coordinate magnitude in the control polygon. Tolerances have
    /// to be relative to this: the fuzz generator reaches 2^57, where an
    /// absolute 1e-9 is far below one ulp and no implementation can pass.
    fn scale(c: CubicBez) -> f64 {
        [c.p0, c.p1, c.p2, c.p3]
            .iter()
            .fold(0.0f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
    }

    #[test]
    fn endpoints_are_bit_exact() {
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        );
        for tol in [1.0, 1e-2, 1e-6] {
            let poly = flatten(c, tol);
            assert_eq!(poly[0], c.p0);
            assert_eq!(*poly.last().unwrap(), c.p3);
        }
    }

    #[test]
    fn a_straight_line_emits_two_points() {
        let line = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(3.0, 3.0),
        );
        assert_eq!(flatten(line, 1e-9).len(), 2);

        // Collinear but unevenly spaced -- the curve is still the segment, just
        // traversed at a varying rate. A second-difference flatness test would
        // subdivide this one; a distance-to-chord test does not.
        let uneven = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(0.1, 0.1),
            Point::new(2.9, 2.9),
            Point::new(3.0, 3.0),
        );
        assert_eq!(flatten(uneven, 1e-9).len(), 2);

        // Degenerate: a point.
        let p = Point::new(7.0, -3.0);
        assert_eq!(flatten(CubicBez::new(p, p, p, p), 1e-9).len(), 2);

        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(5.0, 0.0),
            Point::new(9.0, 0.0),
        );
        let mut out = Vec::new();
        q.flatten(1e-9, &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_collinear_curve_that_reverses_is_not_flat() {
        // Zero-length chord, zero perpendicular deviation, and the curve
        // travels a long way along x and back. Measuring to the chord *line*
        // would call this flat and be wrong by 1.44 units, the curve's furthest
        // excursion (max of 15t(1-t)(1-2t), at t = 0.2113).
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(5.0, 0.0),
            Point::new(-5.0, 0.0),
            Point::new(0.0, 0.0),
        );
        let poly = flatten(c, 0.01);
        assert!(poly.len() > 2);
        assert!(max_deviation(c, &poly, 4000) <= 0.01 + 1e-12);
    }

    #[test]
    fn deviation_stays_within_tolerance_on_random_cubics() {
        check(
            "cubic flatten bound",
            2_000,
            |r| (r.cubic(), r.below(4)),
            |&(c, shift)| {
                // Relative to the curve's own size, and coarse enough that the
                // dense sampling below stays affordable.
                let tol = scale(c) * f64::from(1u32 << shift) / 1024.0;
                if tol == 0.0 {
                    return flatten(c, 0.0).len() == 2; // the all-zero curve
                }
                let poly = flatten(c, tol);
                // Slack for the rounding in `eval` and in the distance itself,
                // which is relative to the coordinate magnitude, not to `tol`.
                let slack = 64.0 * f64::EPSILON * scale(c);
                max_deviation(c, &poly, 500) <= tol + slack
            },
        );
    }

    #[test]
    fn deviation_stays_within_tolerance_on_random_quads() {
        check(
            "quad flatten bound",
            2_000,
            |r| (r.quad(), r.below(4)),
            |&(q, shift)| {
                let c = q.to_cubic();
                let tol = scale(c) * f64::from(1u32 << shift) / 1024.0;
                if tol == 0.0 {
                    let mut out = Vec::new();
                    q.flatten(0.0, &mut out);
                    return out.len() == 2;
                }
                let mut poly = Vec::new();
                q.flatten(tol, &mut poly);
                let slack = 64.0 * f64::EPSILON * scale(c);
                quad_max_deviation(q, &poly, 500) <= tol + slack
            },
        );
    }

    #[test]
    fn every_emitted_point_is_finite_and_on_the_curve_ends() {
        check(
            "flatten well formed",
            2_000,
            |r| (r.cubic(), r.below(4)),
            |&(c, shift)| {
                let tol = (scale(c) + 1.0) * f64::from(1u32 << shift) / 1024.0;
                let poly = flatten(c, tol);
                poly.len() >= 2
                    && poly[0] == c.p0
                    && *poly.last().unwrap() == c.p3
                    && poly.iter().all(|p| p.is_finite())
            },
        );
    }

    #[test]
    fn count_scales_as_the_inverse_square_root_of_tolerance() {
        // A curve with real bend in it, flattened over five decades. A decade
        // of tolerance ideally costs sqrt(10) = 3.16x the segments; bisection
        // quantises each branch to a power of two, so the measured ratio
        // wobbles around it rather than sitting on it.
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(100.0, 300.0),
            Point::new(300.0, -200.0),
            Point::new(400.0, 100.0),
        );
        let counts: Vec<f64> = (0..6)
            .map(|i| (flatten(c, 1.0 / 10f64.powi(i)).len() - 1) as f64)
            .collect();
        for w in counts.windows(2) {
            let ratio = w[1] / w[0];
            assert!((1.6..=6.3).contains(&ratio), "ratio {ratio} in {counts:?}");
        }
        // Over the whole five decades the quantisation averages out, so the
        // total is within a factor of two of the ideal 10^2.5 = 316.
        let overall = counts[5] / counts[0];
        assert!((158.0..=632.0).contains(&overall), "{counts:?}");
    }

    #[test]
    fn finer_tolerance_never_emits_fewer_points() {
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(10.0, 8.0),
            Point::new(-6.0, 8.0),
            Point::new(4.0, 0.0),
        );
        let mut prev = 0;
        for i in 0..12 {
            let n = flatten(c, 10.0 / f64::from(1u32 << i)).len();
            assert!(n >= prev, "tolerance {i} gave {n} after {prev}");
            prev = n;
        }
    }

    #[test]
    fn cusps_terminate() {
        // An exact cusp: the derivative vanishes at t = 0.5 and the tangent
        // reverses. Naive flatness tests that divide by |B'| never terminate
        // here.
        let cusp = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(-1.0, 1.0),
            Point::new(0.0, 0.0),
        );
        // A near-cusp, where the curve turns almost but not quite through zero.
        let near = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(-1.0, 1.0),
            Point::new(1e-9, 1e-12),
        );
        for c in [cusp, near] {
            for tol in [1.0, 1e-3, 1e-6] {
                let poly = flatten(c, tol);
                assert!(poly.len() < 5_000, "{} points at tol {tol}", poly.len());
                assert!(max_deviation(c, &poly, 4000) <= tol + 1e-12);
            }
        }
    }

    #[test]
    fn the_depth_cap_degrades_gracefully() {
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1e6, 3e6),
            Point::new(3e6, -2e6),
            Point::new(4e6, 1e6),
        );
        // Unsatisfiable: zero tolerance on a curve millions of units across.
        // It terminates twice over -- at the cap, and earlier than that where a
        // subcurve gets short enough that its control points land exactly on
        // its own chord and the deviation is a true zero.
        for tol in [0.0, 1e-300] {
            let poly = flatten(c, tol);
            assert!(poly.len() > 1_000, "{} points", poly.len());
            assert!(poly.len() <= (1 << MAX_DEPTH) + 1, "{} points", poly.len());
            assert_eq!(poly[0], c.p0);
            assert_eq!(*poly.last().unwrap(), c.p3);
            assert!(poly.iter().all(|p| p.is_finite()));
        }
        // A negative tolerance is a caller bug; squaring makes it behave as its
        // magnitude, which is the least surprising of the wrong answers.
        assert_eq!(flatten(c, -1e4).len(), flatten(c, 1e4).len());
    }

    #[test]
    fn nan_input_stops_at_the_chord() {
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(f64::NAN, 1.0),
            Point::new(2.0, 1.0),
            Point::new(3.0, 0.0),
        );
        // Not necessarily two: `f64::max` ignores a NaN operand, so the other
        // control point still gets a vote at the first level. What matters is
        // that it stops within a couple of splits instead of running to the cap.
        assert!(flatten(c, 0.1).len() <= 8);
        // A NaN tolerance is a caller bug, but it must not cost 65k points.
        let good = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        );
        assert_eq!(flatten(good, f64::NAN).len(), 2);
        // An enormous tolerance squares to infinity rather than misbehaving.
        assert_eq!(flatten(good, 1e200).len(), 2);
    }

    #[test]
    fn device_pixel_tolerance_converts_through_max_scale() {
        // The intended usage: the renderer wants half a device pixel, the view
        // is zoomed 8x, so the document-space tolerance is 0.5 / 8.
        let view = Affine::rotate(0.7) * Affine::scale(8.0);
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(30.0, 90.0),
            Point::new(90.0, -60.0),
            Point::new(120.0, 30.0),
        );
        let px = 0.5;
        let poly = flatten(c, px / view.max_scale());

        // Measure the error where it is specified: on screen.
        let device = CubicBez::new(view * c.p0, view * c.p1, view * c.p2, view * c.p3);
        let screen: Vec<Point> = poly.iter().map(|&p| view * p).collect();
        let dev = max_deviation(device, &screen, 4000);
        assert!(dev <= px + 1e-9, "{dev} device pixels");
        // And it is not wastefully far under, either: the conversion is not
        // silently making the tolerance tiny.
        assert!(dev > px / 100.0, "{dev} device pixels");
    }

    #[test]
    fn a_quadratic_beats_the_equivalent_cubic_on_count() {
        // The tighter constant is the reason quadratics are not promoted.
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(50.0, 120.0),
            Point::new(200.0, 0.0),
        );
        let mut direct = Vec::new();
        q.flatten(0.01, &mut direct);
        let promoted = flatten(q.to_cubic(), 0.01);
        assert!(direct.len() <= promoted.len());
        assert!(quad_max_deviation(q, &direct, 4000) <= 0.01 + 1e-12);
    }

    #[test]
    fn flatten_appends_rather_than_replacing() {
        let mut out = vec![Point::new(-1.0, -1.0)];
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        );
        c.flatten(0.1, &mut out);
        assert_eq!(out[0], Point::new(-1.0, -1.0));
        assert_eq!(out[1], c.p0);
    }

    #[test]
    fn segment_distance_is_to_the_segment_not_the_line() {
        let (a, b) = (Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        // Beyond the far end: distance to the endpoint, not zero.
        assert!((dist2_to_segment(Point::new(13.0, 0.0), a, b) - 9.0).abs() < 1e-12);
        assert!((dist2_to_segment(Point::new(-4.0, 0.0), a, b) - 16.0).abs() < 1e-12);
        // Perpendicular, inside the span.
        assert!((dist2_to_segment(Point::new(5.0, 2.0), a, b) - 4.0).abs() < 1e-12);
        // Degenerate segment.
        assert!((dist2_to_segment(Point::new(3.0, 4.0), a, a) - 25.0).abs() < 1e-12);
    }

    #[test]
    fn the_bound_is_the_one_the_module_claims() {
        // Spot-check the algebra: 3/4 of the worst control-point distance is a
        // genuine upper bound on the curve's deviation, on random curves.
        check("cubic chord bound", 2_000, Rng::cubic, |&c| {
            let d = dist2_to_segment(c.p1, c.p0, c.p3)
                .max(dist2_to_segment(c.p2, c.p0, c.p3))
                .sqrt();
            let bound = 0.75 * d;
            let mut worst = 0.0f64;
            for i in 0..=200 {
                let p = c.eval(f64::from(i) / 200.0);
                worst = worst.max(dist2_to_segment(p, c.p0, c.p3).sqrt());
            }
            worst <= bound + 64.0 * f64::EPSILON * scale(c)
        });
    }
}
