//! 2D affine transforms.

use crate::{Point, Vec2};
use core::ops::Mul;

/// A 2D affine transform, stored as the six coefficients `[a, b, c, d, e, f]`:
///
/// ```text
/// x' = a*x + c*y + e
/// y' = b*x + d*y + f
/// ```
///
/// This is exactly SVG's `matrix(a b c d e f)` argument order, which means
/// parsing and emitting SVG transforms is a copy rather than a conversion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine([f64; 6]);

impl Affine {
    /// The identity transform.
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// A transform from raw coefficients, in SVG `matrix()` order.
    #[inline]
    pub const fn new(coeffs: [f64; 6]) -> Self {
        Self(coeffs)
    }

    /// Translation by `v`.
    #[inline]
    pub const fn translate(v: Vec2) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, v.x, v.y])
    }

    /// Uniform scale about the origin.
    #[inline]
    pub const fn scale(s: f64) -> Self {
        Self([s, 0.0, 0.0, s, 0.0, 0.0])
    }

    /// Non-uniform scale about the origin.
    #[inline]
    pub const fn scale_non_uniform(sx: f64, sy: f64) -> Self {
        Self([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }

    /// Rotation about the origin by `angle` radians.
    ///
    /// Counter-clockwise in a y-up coordinate system. Screen space is y-down,
    /// where the same transform reads as clockwise.
    #[inline]
    pub fn rotate(angle: f64) -> Self {
        let (sin, cos) = angle.sin_cos();
        Self([cos, sin, -sin, cos, 0.0, 0.0])
    }

    /// Rotation by `angle` radians about `center`.
    #[inline]
    pub fn rotate_about(angle: f64, center: Point) -> Self {
        let v = center.to_vec2();
        Self::translate(v) * Self::rotate(angle) * Self::translate(-v)
    }

    /// Skew, in radians-free shear factors: `x' = x + kx*y`, `y' = ky*x + y`.
    #[inline]
    pub const fn skew(kx: f64, ky: f64) -> Self {
        Self([1.0, ky, kx, 1.0, 0.0, 0.0])
    }

    /// The raw coefficients, in SVG `matrix()` order.
    #[inline]
    pub const fn as_coeffs(self) -> [f64; 6] {
        self.0
    }

    /// The translation component.
    #[inline]
    pub const fn translation(self) -> Vec2 {
        Vec2::new(self.0[4], self.0[5])
    }

    /// Determinant of the linear part.
    ///
    /// Its absolute value is the area scale factor; a negative value means the
    /// transform flips orientation, which winding-sensitive code must account
    /// for.
    #[inline]
    pub fn determinant(self) -> f64 {
        self.0[0] * self.0[3] - self.0[1] * self.0[2]
    }

    /// The inverse transform, or `None` when this transform is singular.
    ///
    /// Returning `None` rather than producing infinities matters: a degenerate
    /// view transform (zoomed to zero, collapsed artboard) is reachable from
    /// the UI, and silently propagating NaN through hit testing is far harder
    /// to diagnose than an explicit absence.
    #[inline]
    pub fn inverse(self) -> Option<Self> {
        let [a, b, c, d, e, f] = self.0;
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = det.recip();
        Some(Self([
            d * inv,
            -b * inv,
            -c * inv,
            a * inv,
            (c * f - d * e) * inv,
            (b * e - a * f) * inv,
        ]))
    }

    /// The larger of the two singular values: the greatest factor by which this
    /// transform can stretch a length.
    ///
    /// Flattening tolerance is expressed in device pixels but curves are
    /// subdivided in document space, so this is what converts between them.
    pub fn max_scale(self) -> f64 {
        let [a, b, c, d, _, _] = self.0;
        // Singular values of [[a, c], [b, d]], via the closed form for 2x2.
        let sum = a * a + b * b + c * c + d * d;
        let det = self.determinant();
        let disc = (sum * sum - 4.0 * det * det).max(0.0).sqrt();
        (0.5 * (sum + disc)).max(0.0).sqrt()
    }

    /// True when every coefficient is finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.0.iter().all(|v| v.is_finite())
    }
}

impl Default for Affine {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mul for Affine {
    type Output = Self;

    /// Composition: `(a * b)` applies `b` first, then `a`.
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        let [a0, b0, c0, d0, e0, f0] = self.0;
        let [a1, b1, c1, d1, e1, f1] = rhs.0;
        Self([
            a0 * a1 + c0 * b1,
            b0 * a1 + d0 * b1,
            a0 * c1 + c0 * d1,
            b0 * c1 + d0 * d1,
            a0 * e1 + c0 * f1 + e0,
            b0 * e1 + d0 * f1 + f0,
        ])
    }
}

impl Mul<Point> for Affine {
    type Output = Point;
    #[inline]
    fn mul(self, rhs: Point) -> Point {
        let [a, b, c, d, e, f] = self.0;
        Point::new(a * rhs.x + c * rhs.y + e, b * rhs.x + d * rhs.y + f)
    }
}

impl Mul<Vec2> for Affine {
    type Output = Vec2;

    /// Transforms a displacement: the linear part only, ignoring translation.
    #[inline]
    fn mul(self, rhs: Vec2) -> Vec2 {
        let [a, b, c, d, _, _] = self.0;
        Vec2::new(a * rhs.x + c * rhs.y, b * rhs.x + d * rhs.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn close(a: Point, b: Point) -> bool {
        (a - b).length() < EPS
    }

    #[test]
    fn identity_changes_nothing() {
        let p = Point::new(3.0, -7.0);
        assert_eq!(Affine::IDENTITY * p, p);
    }

    #[test]
    fn composition_applies_right_hand_side_first() {
        let scale = Affine::scale(2.0);
        let shift = Affine::translate(Vec2::new(10.0, 0.0));
        let p = Point::new(1.0, 0.0);
        // Shift first, then scale: (1 + 10) * 2 = 22.
        assert!(close(scale * shift * p, Point::new(22.0, 0.0)));
        // Scale first, then shift: 1 * 2 + 10 = 12.
        assert!(close(shift * scale * p, Point::new(12.0, 0.0)));
    }

    #[test]
    fn inverse_round_trips() {
        let t = Affine::translate(Vec2::new(4.0, -2.0))
            * Affine::rotate(0.7)
            * Affine::scale_non_uniform(3.0, 0.5);
        let inv = t.inverse().expect("non-singular");
        let p = Point::new(-5.0, 11.0);
        assert!(close(inv * (t * p), p));
        assert!(close(t * (inv * p), p));
    }

    #[test]
    fn singular_transform_has_no_inverse() {
        assert!(Affine::scale(0.0).inverse().is_none());
        assert!(Affine::scale_non_uniform(1.0, 0.0).inverse().is_none());
        assert!(
            Affine::new([1.0, 2.0, 2.0, 4.0, 0.0, 0.0])
                .inverse()
                .is_none()
        );
    }

    #[test]
    fn vectors_ignore_translation_but_points_do_not() {
        let t = Affine::translate(Vec2::new(100.0, 100.0));
        assert_eq!(t * Vec2::new(1.0, 0.0), Vec2::new(1.0, 0.0));
        assert_eq!(t * Point::new(1.0, 0.0), Point::new(101.0, 100.0));
    }

    #[test]
    fn rotate_about_fixes_its_center() {
        let c = Point::new(7.0, -3.0);
        let t = Affine::rotate_about(1.234, c);
        assert!(close(t * c, c));
    }

    #[test]
    fn quarter_turn_maps_x_to_y() {
        let t = Affine::rotate(core::f64::consts::FRAC_PI_2);
        assert!(close(t * Point::new(1.0, 0.0), Point::new(0.0, 1.0)));
    }

    #[test]
    fn determinant_is_the_area_scale_factor() {
        assert!((Affine::scale(3.0).determinant() - 9.0).abs() < EPS);
        assert!((Affine::rotate(0.9).determinant() - 1.0).abs() < EPS);
        // Mirroring flips orientation.
        assert!(Affine::scale_non_uniform(-1.0, 1.0).determinant() < 0.0);
    }

    #[test]
    fn max_scale_reports_the_largest_stretch() {
        assert!((Affine::scale(4.0).max_scale() - 4.0).abs() < EPS);
        assert!((Affine::scale_non_uniform(5.0, 2.0).max_scale() - 5.0).abs() < EPS);
        // Rotation preserves lengths.
        assert!((Affine::rotate(0.4).max_scale() - 1.0).abs() < EPS);
        // Rotation still preserves them after scaling.
        let t = Affine::rotate(0.4) * Affine::scale(3.0);
        assert!((t.max_scale() - 3.0).abs() < EPS);
    }
}
