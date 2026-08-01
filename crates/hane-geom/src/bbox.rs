//! Tight bounding boxes for Bezier segments.
//!
//! The bound comes from the curve's actual extrema, not from the control
//! polygon. A control-polygon hull is a valid bound and trivial to compute, but
//! it can be far larger than the curve it encloses -- and in P3 every unit of
//! slack is a wasted spatial-index hit and a wasted tile. So: solve the
//! hodograph for zeros per axis, keep the ones strictly inside the segment, and
//! union those points with the endpoints.

use crate::{CubicBez, QuadBez, Rect};

impl QuadBez {
    /// The tightest axis-aligned box containing this segment.
    ///
    /// The hodograph of a quadratic is linear, so each axis contributes at most
    /// one interior extremum.
    pub fn bounding_box(self) -> Rect {
        let [d0, d1] = self.deriv_control();
        let mut bbox = Rect::from_points(self.p0, self.p2);
        // Bernstein d0(1-t) + d1*t in power basis is (d1-d0)t + d0, which
        // `quadratic_roots` solves through its degenerate linear branch.
        for (c0, c1) in [(d0.x, d1.x), (d0.y, d1.y)] {
            for t in roots_inside(0.0, c1 - c0, c0) {
                bbox = bbox.union_point(self.eval(t));
            }
        }
        bbox
    }
}

impl CubicBez {
    /// The tightest axis-aligned box containing this segment.
    ///
    /// The hodograph of a cubic is quadratic, so each axis contributes at most
    /// two interior extrema.
    pub fn bounding_box(self) -> Rect {
        let [d0, d1, d2] = self.deriv_control();
        let mut bbox = Rect::from_points(self.p0, self.p3);
        for (c0, c1, c2) in [(d0.x, d1.x, d2.x), (d0.y, d1.y, d2.y)] {
            // Bernstein d0(1-t)^2 + 2*d1*t(1-t) + d2*t^2 in power basis.
            for t in roots_inside(c0 - 2.0 * c1 + c2, 2.0 * (c1 - c0), c0) {
                bbox = bbox.union_point(self.eval(t));
            }
        }
        bbox
    }
}

/// Real roots of `a t^2 + b t + c` that lie strictly inside `(0, 1)`.
///
/// Returns a fixed-size buffer so this allocates nothing; the iterator hides
/// the unused slots. Roots at exactly 0 or 1 are dropped because the endpoints
/// are already in the box, and `eval` is bit-exact only there -- re-evaluating
/// them could only widen the bound by an ulp.
fn roots_inside(a: f64, b: f64, c: f64) -> impl Iterator<Item = f64> {
    let (roots, n) = quadratic_roots(a, b, c);
    let mut kept = [0.0; 2];
    let mut k = 0;
    for &t in &roots[..n] {
        // NaN fails both comparisons, so a NaN control point yields no interior
        // candidates rather than poisoning the box with NaN bounds.
        if t > 0.0 && t < 1.0 {
            kept[k] = t;
            k += 1;
        }
    }
    (0..k).map(move |i| kept[i])
}

/// Real roots of `a t^2 + b t + c`, as a buffer and a count.
fn quadratic_roots(a: f64, b: f64, c: f64) -> ([f64; 2], usize) {
    if a == 0.0 {
        // Not actually a quadratic. This is the common case, not an exotic one:
        // any curve symmetric about its midpoint on some axis lands here, and
        // so does every quadratic segment (whose hodograph is linear by
        // construction).
        if b == 0.0 {
            // A derivative that is constant and, when c is also zero,
            // identically zero. Either way no isolated extremum exists.
            return ([0.0; 2], 0);
        }
        return ([-c / b, 0.0], 1);
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return ([0.0; 2], 0);
    }
    // The textbook formula loses most of its precision on one of the two roots
    // whenever 4ac is small next to b^2: that root is a difference of two
    // nearly equal numbers. Computing the well-conditioned root first and
    // getting the other from the product of roots (c/a = q/a * c/q) avoids the
    // cancellation entirely.
    let q = -0.5 * (b + b.signum() * disc.sqrt());
    if q == 0.0 {
        // b == 0 and disc == 0, so both roots are zero -- outside (0, 1)
        // anyway, but returning it keeps the function honest.
        return ([0.0; 2], 1);
    }
    ([q / a, c / q], 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Point;

    /// The easy wrong answer, kept only as the thing tight bounds must beat.
    fn control_hull(c: CubicBez) -> Rect {
        [c.p0, c.p1, c.p2, c.p3]
            .iter()
            .fold(Rect::EMPTY, |acc, &p| acc.union_point(p))
    }

    fn sampled_bbox(c: CubicBez, n: u32) -> Rect {
        (0..=n).fold(Rect::EMPTY, |acc, i| {
            acc.union_point(c.eval(f64::from(i) / f64::from(n)))
        })
    }

    /// xorshift64*, so the random-curve test needs no dependency and no
    /// entropy source. Fixed seed: a failure here must be reproducible.
    struct Rng(u64);

    impl Rng {
        /// A coordinate in `[-10, 10)`.
        fn coord(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let unit = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            unit * 20.0 - 10.0
        }

        fn point(&mut self) -> Point {
            Point::new(self.coord(), self.coord())
        }

        fn cubic(&mut self) -> CubicBez {
            CubicBez::new(self.point(), self.point(), self.point(), self.point())
        }
    }

    #[test]
    fn symmetric_cubic_has_a_known_box() {
        // The classic arch: x sweeps 0 to 1 monotonically, y peaks at t = 0.5.
        // Its y hodograph has a zero leading coefficient, so this also exercises
        // the quadratic-collapses-to-linear path.
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(0.0, 1.0),
            Point::new(1.0, 1.0),
            Point::new(1.0, 0.0),
        );
        let b = c.bounding_box();
        assert!((b.x0 - 0.0).abs() < 1e-15);
        assert!((b.x1 - 1.0).abs() < 1e-15);
        assert!((b.y0 - 0.0).abs() < 1e-15);
        assert!((b.y1 - 0.75).abs() < 1e-15);
        // Strictly tighter than the hull, which reaches y = 1.
        assert!(control_hull(c).contains_rect(b));
        assert!(b.area() < control_hull(c).area());
    }

    #[test]
    fn quad_solves_its_linear_derivative() {
        // Apex at t = 0.5, y = 3, well below the control point at y = 6.
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(2.0, 6.0),
            Point::new(4.0, 0.0),
        );
        let b = q.bounding_box();
        assert!((b.y1 - 3.0).abs() < 1e-15);
        assert!((b.y0 - 0.0).abs() < 1e-15);
        assert!((b.x0 - 0.0).abs() < 1e-15);
        assert!((b.x1 - 4.0).abs() < 1e-15);
        // The equivalent cubic must agree, since it is the same curve.
        let cb = q.to_cubic().bounding_box();
        assert!((cb.x0 - b.x0).abs() < 1e-12);
        assert!((cb.y0 - b.y0).abs() < 1e-12);
        assert!((cb.x1 - b.x1).abs() < 1e-12);
        assert!((cb.y1 - b.y1).abs() < 1e-12);
    }

    #[test]
    fn monotone_curve_is_bounded_by_its_endpoints() {
        // No interior extremum on either axis: the box is exactly the endpoints.
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(3.0, 3.0),
        );
        assert_eq!(c.bounding_box(), Rect::new(0.0, 0.0, 3.0, 3.0));
    }

    #[test]
    fn degenerate_curves_do_not_produce_nan() {
        let p = Point::new(2.0, 3.0);
        let cases = [
            // Every point coincident: hodograph identically zero.
            CubicBez::new(p, p, p, p),
            // Cusp: the derivative vanishes at t = 0.5 without a sign change on
            // one axis, and the discriminant is exactly zero.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(1.0, 1.0),
                Point::new(0.0, 1.0),
                Point::new(1.0, 0.0),
            ),
            // Start and end coincident, so the box comes entirely from roots.
            CubicBez::new(
                Point::new(0.0, 0.0),
                Point::new(4.0, 0.0),
                Point::new(4.0, 4.0),
                Point::new(0.0, 0.0),
            ),
        ];
        for c in cases {
            let b = c.bounding_box();
            assert!(
                b.x0.is_finite() && b.y0.is_finite() && b.x1.is_finite() && b.y1.is_finite(),
                "{c:?} -> {b:?}"
            );
            assert!(control_hull(c).contains_rect(b), "{c:?}");
        }

        // A degenerate quadratic too.
        let q = QuadBez::new(p, p, p);
        assert_eq!(q.bounding_box(), Rect::new(p.x, p.y, p.x, p.y));
    }

    #[test]
    fn edges_are_touched_by_the_curve() {
        // A million samples put the worst grid error near 1e-11 for these
        // coordinates, comfortably under the 1e-9 the bound must meet.
        const N: u32 = 1_000_000;
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for _ in 0..8 {
            let c = rng.cubic();
            let b = c.bounding_box();
            let s = sampled_bbox(c, N);
            assert!((b.x0 - s.x0).abs() < 1e-9, "{c:?}: {b:?} vs {s:?}");
            assert!((b.y0 - s.y0).abs() < 1e-9, "{c:?}: {b:?} vs {s:?}");
            assert!((b.x1 - s.x1).abs() < 1e-9, "{c:?}: {b:?} vs {s:?}");
            assert!((b.y1 - s.y1).abs() < 1e-9, "{c:?}: {b:?} vs {s:?}");
        }
    }

    #[test]
    fn random_curves_match_dense_sampling() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..500 {
            let c = rng.cubic();
            let b = c.bounding_box();
            let s = sampled_bbox(c, 10_000);

            // Never larger than the easy wrong answer.
            assert!(control_hull(c).contains_rect(b), "{c:?}: {b:?}");

            // Every sampled point is inside. The slack absorbs rounding only:
            // `eval` at a root is not bit-exactly the extremum, so a sample
            // landing on a flat maximum can exceed it by an ulp.
            assert!(b.inflate(1e-12).contains_rect(s), "{c:?}: {b:?} vs {s:?}");

            // And no larger than sampling can account for.
            assert!((b.x0 - s.x0).abs() < 1e-6, "{c:?}: {b:?} vs {s:?}");
            assert!((b.y0 - s.y0).abs() < 1e-6, "{c:?}: {b:?} vs {s:?}");
            assert!((b.x1 - s.x1).abs() < 1e-6, "{c:?}: {b:?} vs {s:?}");
            assert!((b.y1 - s.y1).abs() < 1e-6, "{c:?}: {b:?} vs {s:?}");
        }
    }

    #[test]
    fn random_quads_match_dense_sampling() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
        for _ in 0..500 {
            let q = QuadBez::new(rng.point(), rng.point(), rng.point());
            let b = q.bounding_box();
            let hull = [q.p0, q.p1, q.p2]
                .iter()
                .fold(Rect::EMPTY, |acc, &p| acc.union_point(p));
            assert!(hull.contains_rect(b), "{q:?}: {b:?}");

            let s = (0..=10_000u32).fold(Rect::EMPTY, |acc, i| {
                acc.union_point(q.eval(f64::from(i) / 10_000.0))
            });
            assert!(b.inflate(1e-12).contains_rect(s), "{q:?}: {b:?} vs {s:?}");
            assert!((b.x0 - s.x0).abs() < 1e-6, "{q:?}: {b:?} vs {s:?}");
            assert!((b.y0 - s.y0).abs() < 1e-6, "{q:?}: {b:?} vs {s:?}");
            assert!((b.x1 - s.x1).abs() < 1e-6, "{q:?}: {b:?} vs {s:?}");
            assert!((b.y1 - s.y1).abs() < 1e-6, "{q:?}: {b:?} vs {s:?}");
        }
    }

    #[test]
    fn ill_conditioned_quadratic_keeps_both_roots() {
        // b^2 hugely dominates 4ac: the naive formula returns 0 for the small
        // root, which would silently drop an extremum.
        let (roots, n) = quadratic_roots(1.0, -1e8, 1.0);
        assert_eq!(n, 2);
        let small = roots[0].min(roots[1]);
        let large = roots[0].max(roots[1]);
        assert!((small - 1e-8).abs() < 1e-20, "{small}");
        assert!((large - 1e8).abs() < 1e-6, "{large}");
    }

    #[test]
    fn no_roots_when_the_derivative_never_vanishes() {
        assert_eq!(quadratic_roots(0.0, 0.0, 5.0).1, 0); // constant, non-zero
        assert_eq!(quadratic_roots(0.0, 0.0, 0.0).1, 0); // identically zero
        assert_eq!(quadratic_roots(1.0, 0.0, 1.0).1, 0); // negative discriminant
        assert_eq!(roots_inside(1.0, -3.0, 2.0).count(), 0); // roots at 1 and 2
    }
}
