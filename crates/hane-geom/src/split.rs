//! Subdivision: splitting a segment at a parameter, and extracting a
//! subsegment over a parameter range.
//!
//! Everything adaptive -- flattening, curve/curve intersection, nearest point
//! -- recurses by subdividing, so the two halves must join with no gap at all.
//! The shared endpoint here is bit-exact because both halves are handed the
//! *same* de Casteljau point, and the outer endpoints are copied straight from
//! the original rather than recomputed.

use crate::{CubicBez, QuadBez};

/// Reparameterise `t1` into the domain of the curve remaining after a split at
/// `t0`, whose parameter runs over `[t0, 1]`.
///
/// At `t0 == 1` that remainder is the degenerate point at the end of the
/// curve, and every parameter maps to it, so return 0 rather than divide 0/0.
#[inline]
fn remap(t0: f64, t1: f64) -> f64 {
    let span = 1.0 - t0;
    if span == 0.0 { 0.0 } else { (t1 - t0) / span }
}

impl QuadBez {
    /// Splits into the segments over `[0, t]` and `[t, 1]`.
    ///
    /// The two share an endpoint bit-for-bit, and their outer endpoints are
    /// exactly those of `self`.
    #[inline]
    pub fn split(self, t: f64) -> (Self, Self) {
        let a = self.p0.lerp(self.p1, t);
        let b = self.p1.lerp(self.p2, t);
        let m = a.lerp(b, t);
        (Self::new(self.p0, a, m), Self::new(m, b, self.p2))
    }

    /// The segment over `[t0, t1]`, reparameterised back onto `[0, 1]`.
    #[inline]
    pub fn subsegment(self, t0: f64, t1: f64) -> Self {
        let rest = self.split(t0).1;
        rest.split(remap(t0, t1)).0
    }
}

impl CubicBez {
    /// Splits into the segments over `[0, t]` and `[t, 1]`.
    ///
    /// The two share an endpoint bit-for-bit, and their outer endpoints are
    /// exactly those of `self`.
    #[inline]
    pub fn split(self, t: f64) -> (Self, Self) {
        let a = self.p0.lerp(self.p1, t);
        let b = self.p1.lerp(self.p2, t);
        let c = self.p2.lerp(self.p3, t);
        let d = a.lerp(b, t);
        let e = b.lerp(c, t);
        let m = d.lerp(e, t);
        (Self::new(self.p0, a, d, m), Self::new(m, e, c, self.p3))
    }

    /// The segment over `[t0, t1]`, reparameterised back onto `[0, 1]`.
    #[inline]
    pub fn subsegment(self, t0: f64, t1: f64) -> Self {
        let rest = self.split(t0).1;
        rest.split(remap(t0, t1)).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Point;

    const EPS: f64 = 1e-12;

    fn cubics() -> [CubicBez; 4] {
        [
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(1.0, 2.0),
                Point::new(3.0, -1.0),
                Point::new(4.0, 1.0),
            ),
            // A loop: control points cross over, so t is far from arc length.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(10.0, 8.0),
                Point::new(-6.0, 8.0),
                Point::new(4.0, 0.0),
            ),
            // Degenerate: every control point identical.
            CubicBez::new(
                Point::new(2.0, 3.0),
                Point::new(2.0, 3.0),
                Point::new(2.0, 3.0),
                Point::new(2.0, 3.0),
            ),
            // Wildly different magnitudes, where the naive lerp form drifts.
            CubicBez::new(
                Point::new(-1.0e9, 4.0),
                Point::new(1.0e-9, -7.0),
                Point::new(2.5, 1.0e8),
                Point::new(3.0, -2.0),
            ),
        ]
    }

    fn quads() -> [QuadBez; 3] {
        [
            QuadBez::new(
                Point::new(0.0, 0.0),
                Point::new(2.0, 5.0),
                Point::new(7.0, 1.0),
            ),
            QuadBez::new(
                Point::new(1.0, 1.0),
                Point::new(1.0, 1.0),
                Point::new(1.0, 1.0),
            ),
            QuadBez::new(
                Point::new(-1.0e9, 0.0),
                Point::new(3.0, 1.0e7),
                Point::new(5.0, -2.0),
            ),
        ]
    }

    /// Relative tolerance: the loop and large-magnitude curves carry
    /// coordinates near 1e9, where 1e-12 is below one ulp.
    fn close(a: Point, b: Point) -> bool {
        let scale = 1.0 + a.x.abs().max(a.y.abs()).max(b.x.abs()).max(b.y.abs());
        a.distance(b) <= EPS * scale
    }

    #[test]
    fn split_halves_cover_the_original() {
        for c in cubics() {
            for i in 0..=20 {
                let t = f64::from(i) / 20.0;
                let (l, r) = c.split(t);
                for j in 0..=20 {
                    let s = f64::from(j) / 20.0;
                    assert!(close(l.eval(s), c.eval(t * s)), "t={t} s={s}");
                    assert!(close(r.eval(s), c.eval(t + (1.0 - t) * s)), "t={t} s={s}");
                }
            }
        }
        for q in quads() {
            for i in 0..=20 {
                let t = f64::from(i) / 20.0;
                let (l, r) = q.split(t);
                for j in 0..=20 {
                    let s = f64::from(j) / 20.0;
                    assert!(close(l.eval(s), q.eval(t * s)), "t={t} s={s}");
                    assert!(close(r.eval(s), q.eval(t + (1.0 - t) * s)), "t={t} s={s}");
                }
            }
        }
    }

    #[test]
    fn shared_endpoint_is_bit_exact() {
        for c in cubics() {
            for i in 0..=20 {
                let t = f64::from(i) / 20.0;
                let (l, r) = c.split(t);
                assert_eq!(l.end(), r.start(), "t={t}");
                // And the outer ends are the original's, untouched.
                assert_eq!(l.start(), c.start(), "t={t}");
                assert_eq!(r.end(), c.end(), "t={t}");
            }
        }
        for q in quads() {
            for i in 0..=20 {
                let t = f64::from(i) / 20.0;
                let (l, r) = q.split(t);
                assert_eq!(l.end(), r.start(), "t={t}");
                assert_eq!(l.start(), q.start(), "t={t}");
                assert_eq!(r.end(), q.end(), "t={t}");
            }
        }
    }

    #[test]
    fn split_at_the_ends_is_degenerate_plus_the_original() {
        for c in cubics() {
            let (l, r) = c.split(0.0);
            assert_eq!(l, CubicBez::new(c.p0, c.p0, c.p0, c.p0));
            assert_eq!(r, c);

            let (l, r) = c.split(1.0);
            assert_eq!(l, c);
            assert_eq!(r, CubicBez::new(c.p3, c.p3, c.p3, c.p3));
        }
        for q in quads() {
            let (l, r) = q.split(0.0);
            assert_eq!(l, QuadBez::new(q.p0, q.p0, q.p0));
            assert_eq!(r, q);

            let (l, r) = q.split(1.0);
            assert_eq!(l, q);
            assert_eq!(r, QuadBez::new(q.p2, q.p2, q.p2));
        }
    }

    #[test]
    fn full_subsegment_is_the_original_unchanged() {
        for c in cubics() {
            assert_eq!(c.subsegment(0.0, 1.0), c);
        }
        for q in quads() {
            assert_eq!(q.subsegment(0.0, 1.0), q);
        }
    }

    #[test]
    fn subsegment_matches_the_original_over_its_range() {
        for c in cubics() {
            for i in 0..=10 {
                for j in i..=10 {
                    let (t0, t1) = (f64::from(i) / 10.0, f64::from(j) / 10.0);
                    let sub = c.subsegment(t0, t1);
                    for k in 0..=10 {
                        let s = f64::from(k) / 10.0;
                        let want = c.eval(t0 + (t1 - t0) * s);
                        assert!(close(sub.eval(s), want), "t0={t0} t1={t1} s={s}");
                    }
                }
            }
        }
        for q in quads() {
            for i in 0..=10 {
                for j in i..=10 {
                    let (t0, t1) = (f64::from(i) / 10.0, f64::from(j) / 10.0);
                    let sub = q.subsegment(t0, t1);
                    for k in 0..=10 {
                        let s = f64::from(k) / 10.0;
                        let want = q.eval(t0 + (t1 - t0) * s);
                        assert!(close(sub.eval(s), want), "t0={t0} t1={t1} s={s}");
                    }
                }
            }
        }
    }

    #[test]
    fn empty_subsegment_is_a_point() {
        let c = cubics()[0];
        for i in 0..=10 {
            let t = f64::from(i) / 10.0;
            let sub = c.subsegment(t, t);
            let p = sub.start();
            assert_eq!(sub, CubicBez::new(p, p, p, p), "t={t}");
            assert!(close(p, c.eval(t)), "t={t}");
        }
        // t0 == 1 is the case where the reparameterisation would divide by
        // zero; it must land on the endpoint, not NaN.
        assert_eq!(c.subsegment(1.0, 1.0).start(), c.p3);
    }

    #[test]
    fn splitting_twice_matches_subsegment() {
        let c = cubics()[1];
        let (t0, t1) = (0.25, 0.75);
        let via_splits = c.split(t1).0.split(t0 / t1).1;
        let sub = c.subsegment(t0, t1);
        for i in 0..=20 {
            let s = f64::from(i) / 20.0;
            assert!(close(via_splits.eval(s), sub.eval(s)), "s={s}");
        }
    }

    #[test]
    fn split_preserves_the_tangent_direction_at_the_seam() {
        // The halves meet C1 up to the parameter rescaling: the left half's
        // outgoing tangent and the right half's incoming one are parallel.
        let c = cubics()[1];
        for i in 1..20 {
            let t = f64::from(i) / 20.0;
            let (l, r) = c.split(t);
            let out = l.deriv_at(1.0).normalize();
            let inc = r.deriv_at(0.0).normalize();
            assert!(out.cross(inc).abs() < 1e-9, "t={t}");
            assert!(out.dot(inc) > 0.0, "t={t}");
        }
    }

    #[test]
    fn quad_split_agrees_with_the_equivalent_cubic() {
        let q = quads()[0];
        let t = 0.375;
        let (ql, qr) = q.split(t);
        let (cl, cr) = q.to_cubic().split(t);
        for i in 0..=20 {
            let s = f64::from(i) / 20.0;
            assert!(close(ql.to_cubic().eval(s), cl.eval(s)), "s={s}");
            assert!(close(qr.to_cubic().eval(s), cr.eval(s)), "s={s}");
        }
    }
}
