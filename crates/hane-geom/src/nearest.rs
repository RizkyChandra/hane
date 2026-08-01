//! The nearest point on a segment to a given point.
//!
//! Click-to-select on a path outline and stroke hit testing both reduce to
//! this, so it runs on an input-latency budget and has to be right on the
//! first try -- a hit test that picks the wrong branch of an S-curve is a
//! selection that jumps to the wrong place.
//!
//! # Why not Newton from a seed
//!
//! `|B(t) - p|^2` is a degree-6 polynomial in `t` and routinely has more than
//! one local minimum: any S-curve has two, a loop can have three. Newton or
//! gradient descent from a single seed converges to whichever one it started
//! nearest and returns it with full confidence. So the search here finds *all*
//! the stationary points before it compares any distances.
//!
//! # The method
//!
//! The stationary points are the roots of `(B(t) - p) . B'(t)`, a quintic. It
//! is built directly in the Bernstein basis, where the variation-diminishing
//! property applies: a Bernstein polynomial has no more roots in its interval
//! than its coefficients have sign changes. So de Casteljau subdivision
//! isolates them: a subinterval whose coefficients never cross zero holds no
//! crossing at all, which makes the distance monotone there and its minimum
//! one of the two ends, and a subinterval with a single change holds exactly
//! one crossing and is bracketed. Each bracket is refined by bisection on the
//! derivative evaluated from the curve itself rather than from the subdivided
//! coefficients, which is the accurate way to compute it.
//!
//! The endpoints are candidates too, and they are exact: `eval(0)` and
//! `eval(1)` are bit-exact, so a point that projects past the end of the
//! segment gets `t` exactly 0.0 or 1.0 back, not a hair inside.

use crate::{CubicBez, Point, QuadBez, Vec2};

/// Subdivision depth cap for root isolation.
///
/// Only a tangency -- a double root, where the curve grazes a circle around
/// `p` -- fails to resolve to one sign change, and no amount of subdivision
/// separates one of those. 30 levels pin it to a parameter interval of 1e-9,
/// and the distance is flat to second order there, so the remaining error in
/// the *distance* is far below anything measurable.
const MAX_DEPTH: u32 = 30;

/// Bisection step cap. 60 halvings of `[0, 1]` reach the spacing of f64 near
/// the middle of the range; a root pinned against 0 would otherwise keep
/// halving into the denormals for another thousand steps to no purpose.
const MAX_BISECT: u32 = 60;

/// Half the derivative of `|B(t) - p|^2`, which is zero exactly at the
/// stationary points. The factor of two is dropped because only the sign and
/// the root location matter.
#[inline]
fn half_grad(c: CubicBez, p: Point, t: f64) -> f64 {
    (c.eval(t) - p).dot(c.deriv_at(t))
}

/// Bernstein coefficients of `(B(t) - p) . B'(t)` on `[0, 1]`.
///
/// The product of a degree-3 and a degree-2 Bernstein polynomial is degree 5,
/// with `c_k = sum_{i+j=k} C(3,i) C(2,j) / C(5,k) * a_i . b_j`.
fn stationary_coeffs(c: CubicBez, p: Point) -> [f64; 6] {
    const C3: [f64; 4] = [1.0, 3.0, 3.0, 1.0];
    const C2: [f64; 3] = [1.0, 2.0, 1.0];
    const C5: [f64; 6] = [1.0, 5.0, 10.0, 10.0, 5.0, 1.0];

    let a: [Vec2; 4] = [c.p0 - p, c.p1 - p, c.p2 - p, c.p3 - p];
    let b = c.deriv_control();
    let mut out = [0.0; 6];
    for (i, ai) in a.into_iter().enumerate() {
        for (j, bj) in b.into_iter().enumerate() {
            out[i + j] += C3[i] * C2[j] * ai.dot(bj);
        }
    }
    for (o, n) in out.iter_mut().zip(C5) {
        *o /= n;
    }
    out
}

/// The two halves of a Bernstein polynomial split at the midpoint of its
/// interval, by de Casteljau.
fn split_half(c: [f64; 6]) -> ([f64; 6], [f64; 6]) {
    let (mut left, mut right) = ([0.0; 6], [0.0; 6]);
    let mut w = c;
    for (k, (l, r)) in left.iter_mut().zip(right.iter_mut().rev()).enumerate() {
        let last = 5 - k;
        *l = w[0];
        *r = w[last];
        for i in 0..last {
            w[i] = 0.5 * (w[i] + w[i + 1]);
        }
    }
    (left, right)
}

/// Sign changes in the coefficient sequence, skipping exact zeros.
///
/// By the variation-diminishing property this is an upper bound on the number
/// of roots in the interval, so a single change means a single root and the
/// interval can go straight to a bracketed solve.
fn sign_changes(c: &[f64; 6]) -> usize {
    let mut changes = 0;
    let mut prev = 0.0;
    for &v in c {
        if v != 0.0 {
            if prev != 0.0 && (v < 0.0) != (prev < 0.0) {
                changes += 1;
            }
            prev = v;
        }
    }
    changes
}

/// Replaces `best` with `t` when it is closer. Squared distance throughout:
/// it orders identically to the distance and costs no square root.
fn consider(c: CubicBez, p: Point, t: f64, best: &mut (f64, f64)) {
    let d2 = c.eval(t).distance_squared(p);
    // Strict, so among exactly-tied minima the smallest `t` survives -- and
    // the whole search visits `t` in increasing order.
    if d2 < best.1 {
        *best = (t, d2);
    }
}

/// Refines an interval whose coefficients say it holds a single crossing.
///
/// `lo_negative` is the sign the polynomial takes just inside `t0`, read off
/// the first non-zero coefficient rather than from an evaluation at `t0`
/// itself: a stationary point sitting exactly on the end evaluates to zero
/// there, and a zero gives the bisection no side to start from. That case is
/// not exotic -- it is every cusp, and every curve whose start happens to be
/// its own closest approach.
fn refine(c: CubicBez, p: Point, t0: f64, t1: f64, lo_negative: bool, best: &mut (f64, f64)) {
    let (mut lo, mut hi) = (t0, t1);
    // ponytail: plain bisection, ~60 curve evaluations per crossing. A Newton
    // step guarded by the same bracket converges in a handful; add it if hit
    // testing ever shows up in a profile.
    for _ in 0..MAX_BISECT {
        let mid = 0.5 * (lo + hi);
        // Adjacent floats: nothing left to halve.
        if mid <= lo || mid >= hi {
            break;
        }
        if (half_grad(c, p, mid) < 0.0) == lo_negative {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // The ends too, in increasing order with the rest. They are stationary
    // points in their own right whenever a crossing lands exactly on one, and
    // they are the answer if the coefficients turned out to disagree with a
    // direct evaluation about a sign -- in which case the bisection above just
    // walks to one end.
    consider(c, p, t0, best);
    consider(c, p, lo, best);
    consider(c, p, hi, best);
    consider(c, p, t1, best);
}

/// Isolates the roots in `[t0, t1]` and folds each one into `best`.
fn search(
    c: CubicBez,
    p: Point,
    coeffs: [f64; 6],
    t0: f64,
    t1: f64,
    depth: u32,
    best: &mut (f64, f64),
) {
    // The coefficients bound the polynomial's range, so a sequence that never
    // crosses zero means the half-gradient keeps one sign: the distance is
    // monotone over the whole subinterval and its minimum is at whichever end
    // the sign points to. Reporting that end is what makes the pruning safe.
    // Dropping the interval outright would lose a minimum sitting exactly on a
    // subdivision boundary -- the left half is then non-increasing and the
    // right half non-decreasing, so both would prune away the one point that
    // matters. That is not a rare case: it is what a click exactly on the
    // curve at t = 0.5 produces.
    if coeffs.iter().all(|&v| v >= 0.0) {
        consider(c, p, t0, best);
        return;
    }
    if coeffs.iter().all(|&v| v <= 0.0) {
        consider(c, p, t1, best);
        return;
    }
    // Both signs occur by now, so there is a first non-zero coefficient and it
    // gives the sign the polynomial takes just inside `t0`.
    let lo_negative = coeffs.iter().find(|v| **v != 0.0).is_some_and(|v| *v < 0.0);
    if sign_changes(&coeffs) == 1 || depth == 0 {
        refine(c, p, t0, t1, lo_negative, best);
        return;
    }
    // Subdivision cannot increase the total sign count, so at most two of the
    // children can still hold two changes each: this recursion is linear in
    // the depth, not exponential.
    let (left, right) = split_half(coeffs);
    let mid = 0.5 * (t0 + t1);
    search(c, p, left, t0, mid, depth - 1, best);
    search(c, p, right, mid, t1, depth - 1, best);
}

impl CubicBez {
    /// The parameter of the point on this segment nearest to `p`, and the
    /// distance to it.
    ///
    /// Global, not local: every stationary point of the distance is found and
    /// compared, so an S-curve whose two arms both bend towards `p` returns
    /// the arm that is actually closer. Both endpoints are candidates, and a
    /// `t` of 0.0 or 1.0 comes back exactly -- `eval` is bit-exact there, so
    /// the reported point is the segment's own endpoint and adjacent segments
    /// agree about it.
    ///
    /// Exact ties resolve to the smaller `t`.
    ///
    /// ```
    /// use hane_geom::{CubicBez, Point};
    ///
    /// let c = CubicBez::new(
    ///     Point::new(0.0, 0.0),
    ///     Point::new(1.0, 0.0),
    ///     Point::new(2.0, 0.0),
    ///     Point::new(3.0, 0.0),
    /// );
    /// let (t, d) = c.nearest(Point::new(1.5, 4.0));
    /// assert!((t - 0.5).abs() < 1e-9);
    /// assert!((d - 4.0).abs() < 1e-9);
    /// ```
    pub fn nearest(self, p: Point) -> (f64, f64) {
        // Seeded from t = 0 rather than from infinity so that a NaN input
        // gives a NaN distance back instead of silently reporting infinity.
        let mut best = (0.0, self.p0.distance_squared(p));
        search(
            self,
            p,
            stationary_coeffs(self, p),
            0.0,
            1.0,
            MAX_DEPTH,
            &mut best,
        );
        // Last, so the visiting order is increasing in `t` and the tie rule
        // above holds.
        consider(self, p, 1.0, &mut best);
        (best.0, best.1.sqrt())
    }
}

impl QuadBez {
    /// The parameter of the point on this segment nearest to `p`, and the
    /// distance to it. See [`CubicBez::nearest`].
    ///
    /// ```
    /// use hane_geom::{Point, QuadBez};
    ///
    /// let q = QuadBez::new(
    ///     Point::new(0.0, 0.0),
    ///     Point::new(1.0, 1.0),
    ///     Point::new(2.0, 0.0),
    /// );
    /// let (t, _) = q.nearest(Point::new(-3.0, 0.0));
    /// assert_eq!(t, 0.0);
    /// ```
    pub fn nearest(self, p: Point) -> (f64, f64) {
        // ponytail: degree elevation, which preserves the parameterisation
        // exactly, so the cubic solver answers for the quad too and there is
        // one search to get right instead of two. It perturbs the control
        // points by a rounding step; the distance below is measured on the
        // quad itself so the returned pair stays self-consistent. A native
        // cubic-in-t solve would only matter if quads ever became the hot
        // path, which is unlikely -- SVG and fonts both arrive as cubics.
        let (t, _) = self.to_cubic().nearest(p);
        (t, self.eval(t).distance(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuzz::check;

    /// The nearest point found by brute force, for comparison.
    fn dense(c: CubicBez, p: Point, samples: u32) -> (f64, f64) {
        let mut best = (0.0, f64::INFINITY);
        for i in 0..=samples {
            let t = f64::from(i) / f64::from(samples);
            let d2 = c.eval(t).distance_squared(p);
            if d2 < best.1 {
                best = (t, d2);
            }
        }
        (best.0, best.1.sqrt())
    }

    /// The largest coordinate in play, which is what a relative tolerance has
    /// to be measured against: rounding in `eval` is proportional to it.
    fn magnitude(ps: &[Point]) -> f64 {
        ps.iter()
            .fold(1.0f64, |m, q| m.max(q.x.abs()).max(q.y.abs()))
    }

    fn dense_quad(q: QuadBez, p: Point, samples: u32) -> f64 {
        let mut best = f64::INFINITY;
        for i in 0..=samples {
            let t = f64::from(i) / f64::from(samples);
            best = best.min(q.eval(t).distance_squared(p));
        }
        best.sqrt()
    }

    /// Two local minima: the curve leaves to the right, swings up and comes
    /// back, so a query point above the middle is near-equidistant from both
    /// arms and a seeded Newton picks whichever arm it started on.
    fn s_curve() -> CubicBez {
        CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(4.0, 6.0),
            Point::new(-2.0, 6.0),
            Point::new(2.0, 0.0),
        )
    }

    fn wiggly() -> CubicBez {
        CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        )
    }

    #[test]
    fn straight_line_matches_the_analytic_projection() {
        // Evenly spaced collinear control points, so B(t) is the line
        // segment p0 + t * (p3 - p0) and the projection is in closed form.
        let c = CubicBez::new(
            Point::new(1.0, 2.0),
            Point::new(3.0, 5.0),
            Point::new(5.0, 8.0),
            Point::new(7.0, 11.0),
        );
        let d = c.p3 - c.p0;
        for i in -5..=25 {
            for j in -3..=3 {
                let base = f64::from(i) / 20.0;
                // Off the line as well as along it, so the projection is a
                // real one and not just a point already on the curve.
                let p = c.p0 + d * base + d.perp().normalize() * f64::from(j);
                let want_t = ((p - c.p0).dot(d) / d.dot(d)).clamp(0.0, 1.0);
                let want_d = (c.p0 + d * want_t).distance(p);
                let (t, dist) = c.nearest(p);
                assert!((t - want_t).abs() < 1e-9, "p={p:?} t={t} want={want_t}");
                assert!((dist - want_d).abs() < 1e-9, "p={p:?} d={dist}");
            }
        }
    }

    #[test]
    fn a_point_on_the_curve_has_zero_distance() {
        for c in [wiggly(), s_curve()] {
            for i in 0..=100 {
                let t0 = f64::from(i) / 100.0;
                let p = c.eval(t0);
                let (t, d) = c.nearest(p);
                assert!(d < 1e-9, "t0={t0} d={d}");
                // The parameter itself only pins down where the curve is not
                // flat: near a cusp or a self-intersection several t can sit
                // within 1e-9 of the same point. `s_curve` has neither, so
                // both curves here are safe to check.
                assert!((t - t0).abs() < 1e-6, "t0={t0} t={t}");
            }
        }
    }

    #[test]
    fn multiple_local_minima_resolve_to_the_global_one() {
        let c = s_curve();
        // Sweeping across the top: the answer jumps from the right arm to the
        // left one, and anything that keeps a single seed lags the jump.
        for i in 0..=40 {
            let p = Point::new(-1.0 + f64::from(i) / 10.0, 4.0);
            let (t, d) = c.nearest(p);
            let (_, want) = dense(c, p, 200_000);
            assert!(d <= want + 1e-9, "p={p:?} d={d} dense={want}");
            assert!((d - c.eval(t).distance(p)).abs() < 1e-9, "p={p:?}");
        }
        // The exact axis of symmetry: two minima with identical distance, and
        // the documented tie rule picks the earlier parameter.
        let (t, _) = c.nearest(Point::new(1.0, 4.0));
        assert!(t < 0.5, "t={t}");
    }

    #[test]
    fn endpoints_are_reachable_and_exact() {
        let c = wiggly();
        // Far behind the start along the incoming tangent, and past the end
        // along the outgoing one.
        let before = c.p0 - c.deriv_at(0.0) * 10.0;
        let after = c.p3 + c.deriv_at(1.0) * 10.0;
        assert_eq!(c.nearest(before), (0.0, c.p0.distance(before)));
        assert_eq!(c.nearest(after), (1.0, c.p3.distance(after)));
        // Exactly at an endpoint.
        assert_eq!(c.nearest(c.p0), (0.0, 0.0));
        assert_eq!(c.nearest(c.p3), (1.0, 0.0));
    }

    #[test]
    fn degenerate_curves_do_not_produce_nan() {
        let p = Point::new(2.0, 3.0);
        let c = CubicBez::new(p, p, p, p);
        let q = Point::new(5.0, 7.0);
        let (t, d) = c.nearest(q);
        assert_eq!(t, 0.0);
        assert!((d - p.distance(q)).abs() < 1e-12, "d={d}");

        // A cusp, where the derivative vanishes and the quintic has a double
        // root at the same place.
        let cusp = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(-1.0, 1.0),
            Point::new(0.0, 0.0),
        );
        for i in 0..=20 {
            let p = Point::new(-1.5 + f64::from(i) / 10.0, 0.75);
            let (t, d) = cusp.nearest(p);
            assert!(d.is_finite() && (0.0..=1.0).contains(&t));
            assert!(d <= dense(cusp, p, 200_000).1 + 1e-9, "p={p:?} d={d}");
        }
    }

    #[test]
    fn matches_dense_sampling_to_1e_6() {
        let curves = [
            wiggly(),
            s_curve(),
            // A loop: three local minima for a point inside it.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(10.0, 8.0),
                Point::new(-6.0, 8.0),
                Point::new(4.0, 0.0),
            ),
        ];
        for c in curves {
            for i in -8..=8 {
                for j in -8..=8 {
                    let p = Point::new(f64::from(i) / 2.0, f64::from(j) / 2.0);
                    let (_, d) = c.nearest(p);
                    // 100k samples put the brute-force answer within 1e-9 of
                    // the true minimum for curves this size, so the two-sided
                    // bound really is a 1e-6 accuracy claim.
                    let want = dense(c, p, 100_000).1;
                    assert!((d - want).abs() < 1e-6, "p={p:?} d={d} dense={want}");
                    assert!(d <= want + 1e-12, "p={p:?} d={d} dense={want}");
                }
            }
        }
    }

    #[test]
    fn never_worse_than_dense_sampling_on_random_curves() {
        check(
            "cubic nearest vs dense",
            2_000,
            |r| (r.cubic(), r.point()),
            |&(c, p)| {
                let (t, d) = c.nearest(p);
                if !d.is_finite() {
                    // Only reachable when the input itself is non-finite,
                    // which the generators do not produce.
                    return false;
                }
                let want = dense(c, p, 2_000).1;
                // Relative to position, not just to size: generated
                // coordinates reach 2^57, where an absolute tolerance is
                // below one ulp and a curve of zero extent still evaluates to
                // a point whose distance carries error at that magnitude. A
                // genuinely missed minimum is wrong by a fraction of the
                // curve's own size, so this still catches it.
                let scale = magnitude(&[c.p0, c.p1, c.p2, c.p3, p]);
                (0.0..=1.0).contains(&t)
                    && (c.eval(t).distance(p) - d).abs() <= 1e-12 * scale
                    && d <= want + 1e-12 * scale
            },
        );
        check(
            "quad nearest vs dense",
            2_000,
            |r| (r.quad(), r.point()),
            |&(q, p)| {
                let (t, d) = q.nearest(p);
                let want = dense_quad(q, p, 2_000);
                let scale = magnitude(&[q.p0, q.p1, q.p2, p]);
                (0.0..=1.0).contains(&t)
                    && (q.eval(t).distance(p) - d).abs() <= 1e-12 * scale
                    && d <= want + 1e-12 * scale
            },
        );
    }

    #[test]
    fn a_quad_agrees_with_its_cubic() {
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(3.0, 6.0),
            Point::new(9.0, 0.0),
        );
        let c = q.to_cubic();
        for i in -4..=12 {
            for j in -4..=8 {
                let p = Point::new(f64::from(i), f64::from(j));
                let (tq, dq) = q.nearest(p);
                let (tc, dc) = c.nearest(p);
                assert!((tq - tc).abs() < 1e-9 && (dq - dc).abs() < 1e-9, "p={p:?}");
            }
        }
        assert_eq!(q.nearest(q.p0).0, 0.0);
        assert_eq!(q.nearest(q.p2).0, 1.0);
    }

    #[test]
    fn scale_does_not_change_the_answer() {
        // The search has no absolute tolerance in it, so a glyph-scale and an
        // artboard-scale copy of one shape must return the same parameter.
        let c = wiggly();
        let p = Point::new(2.0, 3.0);
        let (base, _) = c.nearest(p);
        for scale in [1e-6, 1e6] {
            let s = CubicBez::new(
                Point::new(c.p0.x * scale, c.p0.y * scale),
                Point::new(c.p1.x * scale, c.p1.y * scale),
                Point::new(c.p2.x * scale, c.p2.y * scale),
                Point::new(c.p3.x * scale, c.p3.y * scale),
            );
            let (t, d) = s.nearest(Point::new(p.x * scale, p.y * scale));
            assert!((t - base).abs() < 1e-9, "scale={scale} t={t}");
            assert!(
                (d / scale - c.nearest(p).1).abs() < 1e-9,
                "scale={scale} d={d}"
            );
        }
    }

    #[test]
    fn sign_changes_counts_across_zeros() {
        assert_eq!(sign_changes(&[0.0; 6]), 0);
        assert_eq!(sign_changes(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 0);
        assert_eq!(sign_changes(&[1.0, 0.0, 0.0, -1.0, 0.0, 1.0]), 2);
        // -0.0 is a zero, not a negative: it must not invent a sign change.
        assert_eq!(sign_changes(&[1.0, -0.0, 1.0, 1.0, 1.0, 1.0]), 0);
    }

    #[test]
    fn split_half_agrees_with_direct_evaluation() {
        // The isolation is only sound if the halves really are the same
        // polynomial reparameterised, so check against `half_grad`.
        let c = wiggly();
        let p = Point::new(1.0, 4.0);
        let (left, right) = split_half(stationary_coeffs(c, p));
        // The ends of a Bernstein polynomial are its outer coefficients.
        assert!((left[0] - half_grad(c, p, 0.0)).abs() < 1e-9);
        assert!((left[5] - half_grad(c, p, 0.5)).abs() < 1e-9);
        assert!((right[0] - half_grad(c, p, 0.5)).abs() < 1e-9);
        assert!((right[5] - half_grad(c, p, 1.0)).abs() < 1e-9);
    }
}
