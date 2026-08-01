//! Degree reduction: a cubic as a chain of quadratics within a tolerance.
//!
//! The other direction is exact and already lives on the type --
//! [`QuadBez::to_cubic`] elevates a quadratic to the cubic that evaluates to
//! the same points. This module is only about the lossy direction, which is
//! worth having because quadratic rasterization is cheaper: a quadratic's
//! implicit form is the Loop-Blinn `u^2 - v` test, one multiply and one
//! compare per pixel, where a cubic needs a texture of precomputed
//! coefficients and a sign correction for loops and cusps.
//!
//! # What a quadratic cannot carry
//!
//! Write the cubic in monomial form about its start: `C(s) = p0 + A s + B s^2
//! plus D s^3`. A quadratic can match `p0`, `A` and `B` exactly; the whole of
//! the error is the `D` term, and `D` is the third difference of the control
//! points:
//!
//! ```text
//! D = p3 - 3 p2 + 3 p1 - p0
//! ```
//!
//! That single vector determines everything below. Three quadratics matter,
//! all interpolating both endpoints and differing only in the control point:
//!
//! | control point | matches | error |
//! |---|---|---|
//! | `p0 + 3/2 (p1 - p0)` | the tangent at `s = 0` | `4/27 \|D\|` |
//! | `p3 + 3/2 (p2 - p3)` | the tangent at `s = 1` | `4/27 \|D\|` |
//! | midpoint of those two | neither | `1/(12 sqrt 3) \|D\|` |
//!
//! These are exact maxima, not bounds. Pinning the start tangent forces the
//! difference curve to `-D s^2 (1 - s)`, whose peak is `4/27` of `|D|`;
//! splitting the difference between the two tangent constructions leaves
//! `D s (1 - s) (1/2 - s)`, which equioscillates and is therefore the best a
//! single quadratic can do -- no other control point beats `1/(12 sqrt 3)`.
//!
//! The same convexity argument as in [`flatten`](crate::CubicBez::flatten)
//! would give a bound rather than an equality here (`3/4` times the larger of
//! the two elevated-control-point offsets); the closed forms are tighter, so
//! they are what the segment count is derived from.
//!
//! # Where to split
//!
//! `D` is the third derivative up to a constant factor, and the third
//! derivative of a cubic is constant, so a subsegment covering a `t`-interval
//! of width `h` has `|D_sub| = h^3 |D|` exactly. Error therefore falls as the
//! cube of the piece width, and *equalising the error* -- not the width --
//! across pieces is what minimises the count.
//!
//! Only the first and last piece have a tangent to preserve, so only they pay
//! the `4/27` constant. Equal error means the end pieces are shorter than the
//! interior ones by `(4/27 * 12 sqrt 3)^(1/3) = 1.4548`. The count that comes
//! out is at most one segment above `((1/(12 sqrt 3)) |D| / tol)^(1/3)`, which
//! is the count an all-interior chain would need and hence a floor for any
//! scheme built from endpoint-interpolating quadratics.
//!
//! Two pieces is the minimum whenever `D` is non-zero. One quadratic can pin
//! both end tangents only by placing its control point where the two tangent
//! lines cross, and that construction is singular exactly when the tangents
//! are parallel -- which every subsegment approaches as it shrinks. It is also
//! only second-order accurate, since it has no freedom left to match `B`, so
//! it would cost `sqrt` rather than `cbrt` many segments. Paying one extra
//! segment buys robustness and an order of convergence.

use crate::{CubicBez, Point, QuadBez};

/// Segment cap, for tolerances no subdivision can reach (zero, negative, or
/// below an ulp of the coordinates). Past this the pieces are narrower than
/// f64 resolves anyway, so the request degrades into a dense chain instead of
/// allocating without bound.
const MAX_QUADS: usize = 1024;

/// `1 / (12 sqrt 3)`, the error of the midpoint construction per unit of `|D|`.
const MID_ERROR: f64 = 0.048_112_522_432_468_816;

/// `(4/27 / MID_ERROR).cbrt()`: how much shorter an end piece must be to match
/// an interior piece's error.
const END_SHRINK: f64 = 1.454_831_514_628_962;

/// The control point of the quadratic that leaves `anchor` along the same
/// tangent as the cubic does.
///
/// `3/2` because elevating a quadratic places the cubic's control point two
/// thirds of the way from the endpoint to the quadratic's; this inverts that.
#[inline]
fn tangent_control(anchor: Point, control: Point) -> Point {
    anchor + (control - anchor) * 1.5
}

/// The control point of the quadratic over the first `width` of `c`, which
/// leaves `c.p0` along the cubic's own start tangent.
///
/// Read off the *original* control points rather than the split-off
/// subsegment's. The two agree exactly in real arithmetic -- a subsegment of
/// width `w` has `p1 - p0` scaled by `w` -- but the subsegment's control point
/// comes out of a lerp, and `(1 - t) * 1e9 + t * (1e9 + 3)` loses most of the
/// three to cancellation. Scaling the original difference keeps the direction
/// to within one rounding whatever the coordinates are.
#[inline]
fn head_control(c: CubicBez, width: f64) -> Point {
    c.p0 + (c.p1 - c.p0) * (1.5 * width)
}

/// The mirror image: the control point of the quadratic over the last `width`
/// of `c`, arriving along the cubic's end tangent.
#[inline]
fn tail_control(c: CubicBez, width: f64) -> Point {
    c.p3 + (c.p2 - c.p3) * (1.5 * width)
}

/// The best quadratic through both endpoints, sharing neither tangent.
#[inline]
fn mid_quad(c: CubicBez) -> QuadBez {
    let ctrl = tangent_control(c.p0, c.p1).midpoint(tangent_control(c.p3, c.p2));
    QuadBez::new(c.p0, ctrl, c.p3)
}

impl CubicBez {
    /// Appends quadratic segments approximating this cubic, every point of
    /// each within `tolerance` of the curve, and returns how many were
    /// appended.
    ///
    /// The chain starts at `p0` and ends at `p3` bit-exactly, consecutive
    /// segments share their joint bit-exactly, and the first and last segment
    /// leave and arrive along the cubic's own end tangents -- so a stroke join
    /// computed from the chain agrees with one computed from the cubic.
    ///
    /// `tolerance` is in document units, like
    /// [`flatten`](CubicBez::flatten): divide by
    /// [`Affine::max_scale`](crate::Affine::max_scale) of the view transform
    /// to specify it in device pixels.
    ///
    /// ```
    /// use hane_geom::{CubicBez, Point};
    ///
    /// let c = CubicBez::new(
    ///     Point::new(0.0, 0.0),
    ///     Point::new(1.0, 2.0),
    ///     Point::new(3.0, -1.0),
    ///     Point::new(4.0, 1.0),
    /// );
    /// let mut quads = Vec::new();
    /// let n = c.to_quads(1e-3, &mut quads);
    /// assert_eq!(n, quads.len());
    /// assert_eq!(quads[0].start(), c.start());
    /// assert_eq!(quads[n - 1].end(), c.end());
    /// ```
    pub fn to_quads(self, tolerance: f64, out: &mut Vec<QuadBez>) -> usize {
        // The third difference: the whole of the error, and zero exactly when
        // the cubic already is a quadratic. Written as a difference of
        // differences so a curve translated far from the origin does not lose
        // it to cancellation.
        let third = ((self.p3 - self.p0) - (self.p2 - self.p1) * 3.0).length();
        // Negated so a NaN control point takes this branch instead of driving
        // the count to the cap and emitting 1024 NaN segments.
        //
        // The negation is the point: `<= 0.0` and `!(> 0.0)` differ exactly on
        // the incomparable case, and that is the case being handled -- the same
        // trick as `flatten`'s `within`.
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if !(third > 0.0) {
            out.push(QuadBez::new(self.p0, head_control(self, 1.0), self.p3));
            return 1;
        }

        let n = if tolerance > 0.0 {
            // Interior pieces of this width sit exactly on the tolerance; the
            // two end pieces are `END_SHRINK` narrower and match them. The
            // cast saturates, so an unreachable tolerance lands on the cap
            // rather than wrapping.
            let raw = (MID_ERROR * third / tolerance).cbrt() + 2.0 - 2.0 / END_SHRINK;
            (raw.ceil().max(2.0) as usize).min(MAX_QUADS)
        } else {
            MAX_QUADS
        };

        let interior = 1.0 / ((n - 2) as f64 + 2.0 / END_SHRINK);
        let end = interior / END_SHRINK;

        // Splitting the remainder each time, rather than taking independent
        // subsegments, is what makes the joints bit-exact: `split` hands both
        // halves the same de Casteljau point.
        let mut rest = self;
        let mut prev = 0.0;
        for k in 1..n {
            let t = end + (k - 1) as f64 * interior;
            // Rescale into the remainder's own parameter domain. `prev` is at
            // most `1 - end`, so the denominator cannot vanish; the clamp only
            // absorbs the last ulp of the accumulated cut positions.
            let local = ((t - prev) / (1.0 - prev)).clamp(0.0, 1.0);
            let (l, r) = rest.split(local);
            out.push(if k == 1 {
                QuadBez::new(l.p0, head_control(self, end), l.p3)
            } else {
                mid_quad(l)
            });
            rest = r;
            prev = t;
        }
        out.push(QuadBez::new(rest.p0, tail_control(self, end), rest.p3));
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vec2;
    use crate::fuzz::{Rng, check};

    fn curves() -> [CubicBez; 5] {
        [
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(1.0, 2.0),
                Point::new(3.0, -1.0),
                Point::new(4.0, 1.0),
            ),
            // A loop: the tangents at the ends point in wildly different
            // directions and cross behind the curve.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(10.0, 8.0),
                Point::new(-6.0, 8.0),
                Point::new(4.0, 0.0),
            ),
            // A cusp, where the derivative vanishes mid-curve.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(3.0, 3.0),
                Point::new(3.0, 3.0),
                Point::new(6.0, 0.0),
            ),
            // An S with an inflection: the end tangents are nearly parallel,
            // which is where a tangent-intersection construction would blow up.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(4.0, 1.0),
                Point::new(-1.0, 2.0),
                Point::new(3.0, 3.0),
            ),
            // Coordinates near 1e9, where absolute tolerances are meaningless.
            CubicBez::new(
                Point::new(1.0e9, 0.0),
                Point::new(1.0e9 + 3.0, 5.0),
                Point::new(1.0e9 - 2.0, 9.0),
                Point::new(1.0e9 + 1.0, 12.0),
            ),
        ]
    }

    /// The largest coordinate in play. Nothing measured on a curve can be
    /// resolved below an ulp of this, so every threshold below carries a term
    /// proportional to it -- the same reasoning as `split.rs`'s `close`.
    fn coord_scale(c: CubicBez) -> f64 {
        [c.p0, c.p1, c.p2, c.p3]
            .iter()
            .fold(0.0f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
    }

    /// The one-sided Hausdorff distance from the chain to the cubic, by dense
    /// sampling: the subdivision rule claims a tolerance, and this measures
    /// whether it holds without reusing any of the rule's arithmetic.
    fn chain_to_curve(c: CubicBez, quads: &[QuadBez]) -> f64 {
        let mut worst: f64 = 0.0;
        for q in quads {
            for i in 0..=64 {
                let s = f64::from(i) / 64.0;
                worst = worst.max(c.nearest(q.eval(s)).1);
            }
        }
        worst
    }

    /// The other side: no part of the cubic is left uncovered by the chain.
    fn curve_to_chain(c: CubicBez, quads: &[QuadBez]) -> f64 {
        let mut worst: f64 = 0.0;
        for i in 0..=2048 {
            let t = f64::from(i) / 2048.0;
            let p = c.eval(t);
            let d = quads
                .iter()
                .map(|q| q.nearest(p).1)
                .fold(f64::INFINITY, f64::min);
            worst = worst.max(d);
        }
        worst
    }

    #[test]
    fn quad_to_cubic_is_exact() {
        // The elevation already exists on the type; this pins that it really
        // is exact rather than close, over the degenerate shapes too.
        check("quad to cubic elevation", 2000, Rng::quad, |&q| {
            let c = q.to_cubic();
            if q.p0 != c.p0 || q.p2 != c.p3 {
                return false;
            }
            // Relative to the control points, not to the value: fuzz
            // coordinates run past 1e17, and a value near zero between two
            // huge controls is pure cancellation.
            let scale = coord_scale(c);
            (0..=32).all(|i| {
                let t = f64::from(i) / 32.0;
                q.eval(t).distance(c.eval(t)) <= 8.0 * f64::EPSILON * scale
            })
        });
    }

    #[test]
    fn endpoints_and_joints_are_bit_exact() {
        for c in curves() {
            for tol in [1.0, 1e-2, 1e-5] {
                let mut q = Vec::new();
                let n = c.to_quads(tol, &mut q);
                assert_eq!(n, q.len());
                assert_eq!(q[0].start(), c.start(), "tol={tol}");
                assert_eq!(q[n - 1].end(), c.end(), "tol={tol}");
                for w in q.windows(2) {
                    assert_eq!(w[0].end(), w[1].start(), "tol={tol}");
                }
            }
        }
    }

    #[test]
    fn end_tangents_are_preserved() {
        for c in curves() {
            for tol in [1.0, 1e-3, 1e-6] {
                let mut q = Vec::new();
                let n = c.to_quads(tol, &mut q);
                for (curve_t, quad) in [(0.0, q[0]), (1.0, q[n - 1])] {
                    let want = c.deriv_at(curve_t).normalize();
                    let got = quad.deriv_at(curve_t);
                    assert!(want.dot(got) > 0.0, "tol={tol} t={curve_t}");
                    // The control point is placed on the tangent ray exactly,
                    // but reading the direction back out of it costs the
                    // cancellation in `q1 - p0`: at 1e9 coordinates an offset
                    // of half a unit only carries seven digits. So what is
                    // asserted is that the control point sits on the ray to
                    // within the resolution of its own coordinates, which is
                    // all a stored point can promise.
                    let slack = 8.0 * f64::EPSILON * coord_scale(c);
                    assert!(want.cross(got).abs() <= slack, "tol={tol} t={curve_t}");
                }
            }
        }
    }

    #[test]
    fn dense_sampling_confirms_the_tolerance() {
        for c in curves() {
            let scale = c.bounding_box().size().length();
            for rel in [1e-2, 1e-4, 1e-6] {
                let tol = rel * scale;
                let mut q = Vec::new();
                c.to_quads(tol, &mut q);
                let out = chain_to_curve(c, &q);
                let back = curve_to_chain(c, &q);
                // Slack for the sampling grid, plus a term for the rounding
                // the successive splits accumulate -- at 1e9 coordinates a
                // 1e-5 tolerance is already within a hundred ulps.
                let slack = tol * 0.01 + 16.0 * f64::EPSILON * coord_scale(c);
                assert!(out <= tol + slack, "rel={rel} chain->curve {out} > {tol}");
                assert!(back <= tol + slack, "rel={rel} curve->chain {back} > {tol}");
            }
        }
    }

    #[test]
    fn count_is_near_minimal() {
        for c in curves() {
            let third = ((c.p3 - c.p0) - (c.p2 - c.p1) * 3.0).length();
            let scale = c.bounding_box().size().length();
            for rel in [1e-2, 1e-4, 1e-6] {
                let tol = rel * scale;
                let mut q = Vec::new();
                let n = c.to_quads(tol, &mut q);
                // The floor: an endpoint-interpolating quadratic cannot beat
                // `MID_ERROR * |D|`, so no chain of them clears `tol` with
                // fewer than this many equal-error pieces.
                let floor = (MID_ERROR * third / tol).cbrt();
                assert!(n as f64 >= floor - 1.0, "rel={rel} n={n} floor={floor}");
                // Pinning both end tangents costs at most one extra segment.
                assert!(
                    n as f64 <= floor.ceil().max(2.0) + 1.0,
                    "rel={rel} n={n} floor={floor}"
                );
            }
        }
    }

    #[test]
    fn tighter_tolerance_never_emits_fewer() {
        let c = curves()[1];
        let mut prev = 0;
        for k in 0..12 {
            let mut q = Vec::new();
            let n = c.to_quads(0.5f64.powi(k), &mut q);
            assert!(n >= prev, "k={k} n={n} prev={prev}");
            prev = n;
        }
    }

    #[test]
    fn a_cubic_that_is_a_quadratic_comes_back_as_one() {
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(3.0, 6.0),
            Point::new(9.0, 0.0),
        );
        let mut out = Vec::new();
        // The elevation is exact, so the third difference is zero and the
        // reduction is the identity -- one segment, not a chain.
        assert_eq!(q.to_cubic().to_quads(1e-12, &mut out), 1);
        for i in 0..=32 {
            let t = f64::from(i) / 32.0;
            assert!(out[0].eval(t).distance(q.eval(t)) < 1e-12, "t={t}");
        }
    }

    #[test]
    fn straight_and_degenerate_curves_do_not_explode() {
        let p = Point::new(2.0, 3.0);
        let mut out = Vec::new();
        assert_eq!(CubicBez::new(p, p, p, p).to_quads(1e-6, &mut out), 1);
        assert_eq!(out[0], QuadBez::new(p, p, p));

        // A degree-3 straight line: still one segment, and still on the line.
        out.clear();
        let line = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(3.0, 3.0),
        );
        assert_eq!(line.to_quads(1e-9, &mut out), 1);
        assert!(out[0].p1.x - out[0].p1.y == 0.0);

        // A NaN must not drive the count to the cap.
        out.clear();
        let nan = Point::new(f64::NAN, 0.0);
        assert_eq!(CubicBez::new(p, nan, p, p).to_quads(1e-6, &mut out), 1);
    }

    #[test]
    fn unreachable_tolerances_stop_at_the_cap() {
        let c = curves()[1];
        for tol in [0.0, -1.0, 1e-300] {
            let mut out = Vec::new();
            assert_eq!(c.to_quads(tol, &mut out), MAX_QUADS);
            assert_eq!(out[0].start(), c.start());
            assert_eq!(out[MAX_QUADS - 1].end(), c.end());
        }
    }

    #[test]
    fn fuzzed_curves_stay_within_a_relative_tolerance() {
        check("cubic to quads", 400, Rng::cubic, |&c| {
            let scale = c.bounding_box().size().length();
            if !scale.is_finite() || scale == 0.0 {
                return true;
            }
            let tol = 1e-3 * scale;
            let mut q = Vec::new();
            let n = c.to_quads(tol, &mut q);
            // The bound above keeps this small: `|D| <= 8 * scale`, so the
            // count never approaches the cap and the tolerance is reachable.
            if n > 16 || q[0].start() != c.start() || q[n - 1].end() != c.end() {
                return false;
            }
            let limit = tol * 1.01 + 16.0 * f64::EPSILON * coord_scale(c);
            (0..n).all(|i| {
                (0..=16).all(|j| {
                    let s = f64::from(j) / 16.0;
                    c.nearest(q[i].eval(s)).1 <= limit
                })
            })
        });
    }

    #[test]
    fn a_transformed_curve_needs_the_same_count() {
        // Segment count is a property of the shape, not the placement: a
        // translation leaves the third difference alone.
        check("count under translation", 300, Rng::cubic, |&c| {
            let scale = c.bounding_box().size().length();
            if !scale.is_finite() || scale == 0.0 {
                return true;
            }
            let shift = Vec2::new(scale, -scale);
            let moved = CubicBez::new(c.p0 + shift, c.p1 + shift, c.p2 + shift, c.p3 + shift);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let (na, nb) = (
                c.to_quads(1e-3 * scale, &mut a),
                moved.to_quads(1e-3 * scale, &mut b),
            );
            na == nb
        });
    }
}
