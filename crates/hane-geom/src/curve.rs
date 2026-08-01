//! Bezier curve segments.
//!
//! Quadratic and cubic segments in Bernstein form, with evaluation and
//! derivatives. Subdivision, bounding boxes, flattening and arc length build on
//! these and live in their own modules.

use crate::{Point, Vec2};

/// A quadratic Bezier segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuadBez {
    /// Start point.
    pub p0: Point,
    /// Control point.
    pub p1: Point,
    /// End point.
    pub p2: Point,
}

/// A cubic Bezier segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBez {
    /// Start point.
    pub p0: Point,
    /// First control point.
    pub p1: Point,
    /// Second control point.
    pub p2: Point,
    /// End point.
    pub p3: Point,
}

impl QuadBez {
    /// A new quadratic segment.
    #[inline]
    pub const fn new(p0: Point, p1: Point, p2: Point) -> Self {
        Self { p0, p1, p2 }
    }

    /// The point at parameter `t`.
    ///
    /// Written in explicit Bernstein form rather than Horner's, because the
    /// basis functions are exactly 1 and 0 at the ends: `eval(0)` returns `p0`
    /// and `eval(1)` returns `p2` bit-for-bit. Subdivision and flattening both
    /// rely on shared endpoints matching exactly, and Horner's form does not
    /// guarantee that.
    #[inline]
    pub fn eval(self, t: f64) -> Point {
        let mt = 1.0 - t;
        let a = mt * mt;
        let b = 2.0 * mt * t;
        let c = t * t;
        Point::new(
            a * self.p0.x + b * self.p1.x + c * self.p2.x,
            a * self.p0.y + b * self.p1.y + c * self.p2.y,
        )
    }

    /// Bernstein coefficients of the hodograph -- the derivative, which for a
    /// quadratic is a linear curve.
    #[inline]
    pub fn deriv_control(self) -> [Vec2; 2] {
        [(self.p1 - self.p0) * 2.0, (self.p2 - self.p1) * 2.0]
    }

    /// The first derivative at `t`, i.e. the tangent vector.
    #[inline]
    pub fn deriv_at(self, t: f64) -> Vec2 {
        let [d0, d1] = self.deriv_control();
        d0 * (1.0 - t) + d1 * t
    }

    /// The second derivative, which is constant for a quadratic.
    #[inline]
    pub fn deriv2(self) -> Vec2 {
        let [d0, d1] = self.deriv_control();
        d1 - d0
    }

    /// The start point.
    #[inline]
    pub const fn start(self) -> Point {
        self.p0
    }

    /// The end point.
    #[inline]
    pub const fn end(self) -> Point {
        self.p2
    }

    /// This segment as an exactly equivalent cubic.
    #[inline]
    pub fn to_cubic(self) -> CubicBez {
        const TWO_THIRDS: f64 = 2.0 / 3.0;
        CubicBez::new(
            self.p0,
            self.p0 + (self.p1 - self.p0) * TWO_THIRDS,
            self.p2 + (self.p1 - self.p2) * TWO_THIRDS,
            self.p2,
        )
    }
}

impl CubicBez {
    /// A new cubic segment.
    #[inline]
    pub const fn new(p0: Point, p1: Point, p2: Point, p3: Point) -> Self {
        Self { p0, p1, p2, p3 }
    }

    /// The point at parameter `t`.
    ///
    /// See [`QuadBez::eval`] for why this uses explicit Bernstein form: the
    /// endpoints come back bit-exact.
    #[inline]
    pub fn eval(self, t: f64) -> Point {
        let mt = 1.0 - t;
        let a = mt * mt * mt;
        let b = 3.0 * mt * mt * t;
        let c = 3.0 * mt * t * t;
        let d = t * t * t;
        Point::new(
            a * self.p0.x + b * self.p1.x + c * self.p2.x + d * self.p3.x,
            a * self.p0.y + b * self.p1.y + c * self.p2.y + d * self.p3.y,
        )
    }

    /// Bernstein coefficients of the hodograph -- the derivative, which for a
    /// cubic is a quadratic curve.
    ///
    /// Tight bounding boxes come from the roots of this; so does curvature, and
    /// so do offset curves.
    #[inline]
    pub fn deriv_control(self) -> [Vec2; 3] {
        [
            (self.p1 - self.p0) * 3.0,
            (self.p2 - self.p1) * 3.0,
            (self.p3 - self.p2) * 3.0,
        ]
    }

    /// The first derivative at `t`, i.e. the tangent vector.
    #[inline]
    pub fn deriv_at(self, t: f64) -> Vec2 {
        let [d0, d1, d2] = self.deriv_control();
        let mt = 1.0 - t;
        d0 * (mt * mt) + d1 * (2.0 * mt * t) + d2 * (t * t)
    }

    /// The second derivative at `t`.
    #[inline]
    pub fn deriv2_at(self, t: f64) -> Vec2 {
        let [d0, d1, d2] = self.deriv_control();
        (d1 - d0) * (2.0 * (1.0 - t)) + (d2 - d1) * (2.0 * t)
    }

    /// The start point.
    #[inline]
    pub const fn start(self) -> Point {
        self.p0
    }

    /// The end point.
    #[inline]
    pub const fn end(self) -> Point {
        self.p3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    fn cubic() -> CubicBez {
        CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        )
    }

    /// Reference implementation: de Casteljau, used to check the Bernstein form.
    fn de_casteljau(c: CubicBez, t: f64) -> Point {
        let a = c.p0.lerp(c.p1, t);
        let b = c.p1.lerp(c.p2, t);
        let d = c.p2.lerp(c.p3, t);
        let e = a.lerp(b, t);
        let f = b.lerp(d, t);
        e.lerp(f, t)
    }

    #[test]
    fn endpoints_are_exact() {
        let c = cubic();
        assert_eq!(c.eval(0.0), c.p0);
        assert_eq!(c.eval(1.0), c.p3);

        let q = QuadBez::new(
            Point::new(1.0, 1.0),
            Point::new(5.0, 9.0),
            Point::new(2.0, 3.0),
        );
        assert_eq!(q.eval(0.0), q.p0);
        assert_eq!(q.eval(1.0), q.p2);
    }

    #[test]
    fn eval_matches_de_casteljau() {
        let c = cubic();
        for i in 0..=100 {
            let t = f64::from(i) / 100.0;
            assert!((c.eval(t) - de_casteljau(c, t)).length() < EPS, "t={t}");
        }
    }

    #[test]
    fn deriv_at_matches_finite_difference() {
        let c = cubic();
        let h = 1e-6;
        for i in 1..100 {
            let t = f64::from(i) / 100.0;
            let fd = (c.eval(t + h) - c.eval(t - h)) / (2.0 * h);
            assert!((c.deriv_at(t) - fd).length() < 1e-6, "t={t}");
        }
    }

    #[test]
    fn deriv2_at_matches_finite_difference() {
        let c = cubic();
        let h = 1e-4;
        for i in 1..100 {
            let t = f64::from(i) / 100.0;
            let fd = (c.deriv_at(t + h) - c.deriv_at(t - h)) / (2.0 * h);
            assert!((c.deriv2_at(t) - fd).length() < 1e-5, "t={t}");
        }
    }

    #[test]
    fn quad_deriv_matches_finite_difference() {
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(2.0, 5.0),
            Point::new(7.0, 1.0),
        );
        let h = 1e-6;
        for i in 1..100 {
            let t = f64::from(i) / 100.0;
            let fd = (q.eval(t + h) - q.eval(t - h)) / (2.0 * h);
            assert!((q.deriv_at(t) - fd).length() < 1e-6, "t={t}");
        }
    }

    #[test]
    fn straight_line_cubic_has_constant_direction() {
        // Control points collinear and evenly spaced: a degree-3 straight line.
        let c = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(3.0, 3.0),
        );
        for i in 0..=10 {
            let t = f64::from(i) / 10.0;
            let p = c.eval(t);
            assert!((p.x - p.y).abs() < EPS);
            assert!(
                c.deriv_at(t)
                    .normalize()
                    .cross(Vec2::new(1.0, 1.0).normalize())
                    .abs()
                    < EPS
            );
        }
    }

    #[test]
    fn degenerate_curve_does_not_produce_nan() {
        let p = Point::new(2.0, 3.0);
        let c = CubicBez::new(p, p, p, p);
        for i in 0..=10 {
            let t = f64::from(i) / 10.0;
            // Not bit-exact: the Bernstein basis functions do not sum to
            // exactly 1.0 in floating point, so even a constant curve drifts by
            // an ulp or two away from the endpoints. Exactness is guaranteed
            // only at t = 0 and t = 1, where the basis is exactly 1 and 0 --
            // see `endpoints_are_exact`.
            assert!((c.eval(t) - p).length() < EPS, "t={t}");
            assert!(c.eval(t).is_finite());
            // The derivative *is* exactly zero: it is built from differences of
            // identical points, not from a weighted sum.
            assert_eq!(c.deriv_at(t), Vec2::ZERO);
        }
    }

    #[test]
    fn quad_to_cubic_is_exact() {
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(3.0, 6.0),
            Point::new(9.0, 0.0),
        );
        let c = q.to_cubic();
        for i in 0..=100 {
            let t = f64::from(i) / 100.0;
            assert!((q.eval(t) - c.eval(t)).length() < 1e-14, "t={t}");
        }
    }
}
