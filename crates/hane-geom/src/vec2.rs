//! Positions and displacements.
//!
//! [`Point`] and [`Vec2`] are deliberately distinct types. A point is a
//! location; a vector is a displacement. `Point - Point` yields a `Vec2`, and
//! `Point + Vec2` yields a `Point`. Conflating the two is a rich source of
//! geometry bugs that the type system otherwise catches for free.

use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign};

/// A displacement in 2D space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    /// Horizontal component.
    pub x: f64,
    /// Vertical component.
    pub y: f64,
}

/// A position in 2D space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

impl Vec2 {
    /// The zero vector.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// A new vector.
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// The unit vector `angle` radians from the positive x-axis.
    #[inline]
    pub fn from_angle(angle: f64) -> Self {
        let (sin, cos) = angle.sin_cos();
        Self { x: cos, y: sin }
    }

    /// Squared length.
    ///
    /// Prefer this to [`length`](Self::length) when only comparing magnitudes;
    /// it avoids a square root.
    #[inline]
    pub fn length_squared(self) -> f64 {
        self.x * self.x + self.y * self.y
    }

    /// Length.
    #[inline]
    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    /// Dot product.
    #[inline]
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// 2D cross product: the z component of the equivalent 3D cross product.
    ///
    /// Positive when `other` lies counter-clockwise of `self`, and equal to
    /// twice the signed area of the triangle the two span. The sign is what
    /// winding computations and convexity tests are built on.
    #[inline]
    pub fn cross(self, other: Self) -> f64 {
        self.x * other.y - self.y * other.x
    }

    /// This vector scaled to unit length.
    ///
    /// Returns [`Vec2::ZERO`] for the zero vector rather than NaN, so callers
    /// normalising possibly-degenerate segments need no special case.
    #[inline]
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len == 0.0 { Self::ZERO } else { self / len }
    }

    /// Rotated a quarter turn counter-clockwise.
    ///
    /// The building block for stroke normals and offset curves.
    #[inline]
    pub fn perp(self) -> Self {
        Self {
            x: -self.y,
            y: self.x,
        }
    }

    /// Angle from the positive x-axis, in radians, in `(-pi, pi]`.
    #[inline]
    pub fn angle(self) -> f64 {
        self.y.atan2(self.x)
    }

    /// Linear interpolation, in the symmetric form `(1 - t) * self + t * other`.
    ///
    /// Deliberately not the cheaper `self + (other - self) * t`. That form is
    /// exact at `t = 0` but not at `t = 1`: for widely separated magnitudes the
    /// difference cancels the endpoint away entirely, so `self = 1e300` with
    /// `other = 1.0` returns `0.0` rather than `1.0`. The symmetric form's
    /// weights are exactly 1 and 0 at *both* ends, which is what de Casteljau
    /// subdivision relies on to keep shared endpoints bit-exact.
    #[inline]
    pub fn lerp(self, other: Self, t: f64) -> Self {
        let mt = 1.0 - t;
        Self {
            x: mt * self.x + t * other.x,
            y: mt * self.y + t * other.y,
        }
    }

    /// This displacement read as a position offset from the origin.
    #[inline]
    pub const fn to_point(self) -> Point {
        Point {
            x: self.x,
            y: self.y,
        }
    }

    /// True when both components are finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Point {
    /// The origin.
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    /// A new point.
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// This position read as a displacement from the origin.
    #[inline]
    pub const fn to_vec2(self) -> Vec2 {
        Vec2 {
            x: self.x,
            y: self.y,
        }
    }

    /// Distance to `other`.
    #[inline]
    pub fn distance(self, other: Self) -> f64 {
        (other - self).length()
    }

    /// Squared distance to `other`.
    ///
    /// Prefer this for nearest-point searches; it avoids a square root per
    /// candidate and preserves ordering.
    #[inline]
    pub fn distance_squared(self, other: Self) -> f64 {
        (other - self).length_squared()
    }

    /// Linear interpolation, in the symmetric form `(1 - t) * self + t * other`.
    ///
    /// See [`Vec2::lerp`] for why the cheaper difference form is not used: it
    /// loses the `t = 1` endpoint when the two operands differ wildly in
    /// magnitude, and exact endpoints are what subdivision depends on.
    #[inline]
    pub fn lerp(self, other: Self, t: f64) -> Self {
        let mt = 1.0 - t;
        Self {
            x: mt * self.x + t * other.x,
            y: mt * self.y + t * other.y,
        }
    }

    /// The midpoint of `self` and `other`.
    #[inline]
    pub fn midpoint(self, other: Self) -> Self {
        Self {
            x: 0.5 * (self.x + other.x),
            y: 0.5 * (self.y + other.y),
        }
    }

    /// True when both coordinates are finite.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl From<(f64, f64)> for Point {
    #[inline]
    fn from((x, y): (f64, f64)) -> Self {
        Self { x, y }
    }
}

impl From<(f64, f64)> for Vec2 {
    #[inline]
    fn from((x, y): (f64, f64)) -> Self {
        Self { x, y }
    }
}

impl Add for Vec2 {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl Sub for Vec2 {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl Neg for Vec2 {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self {
            x: -self.x,
            y: -self.y,
        }
    }
}

impl Mul<f64> for Vec2 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f64) -> Self {
        Self {
            x: self.x * rhs,
            y: self.y * rhs,
        }
    }
}

impl Mul<Vec2> for f64 {
    type Output = Vec2;
    #[inline]
    fn mul(self, rhs: Vec2) -> Vec2 {
        rhs * self
    }
}

impl Div<f64> for Vec2 {
    type Output = Self;
    #[inline]
    fn div(self, rhs: f64) -> Self {
        Self {
            x: self.x / rhs,
            y: self.y / rhs,
        }
    }
}

impl AddAssign for Vec2 {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl SubAssign for Vec2 {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl MulAssign<f64> for Vec2 {
    #[inline]
    fn mul_assign(&mut self, rhs: f64) {
        *self = *self * rhs;
    }
}

impl DivAssign<f64> for Vec2 {
    #[inline]
    fn div_assign(&mut self, rhs: f64) {
        *self = *self / rhs;
    }
}

impl Add<Vec2> for Point {
    type Output = Point;
    #[inline]
    fn add(self, rhs: Vec2) -> Point {
        Point {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl Sub<Vec2> for Point {
    type Output = Point;
    #[inline]
    fn sub(self, rhs: Vec2) -> Point {
        Point {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl Sub for Point {
    type Output = Vec2;
    #[inline]
    fn sub(self, rhs: Self) -> Vec2 {
        Vec2 {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl AddAssign<Vec2> for Point {
    #[inline]
    fn add_assign(&mut self, rhs: Vec2) {
        *self = *self + rhs;
    }
}

impl SubAssign<Vec2> for Point {
    #[inline]
    fn sub_assign(&mut self, rhs: Vec2) {
        *self = *self - rhs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    #[test]
    fn point_difference_is_a_displacement() {
        let a = Point::new(3.0, 7.0);
        let b = Point::new(1.0, 2.0);
        assert_eq!(a - b, Vec2::new(2.0, 5.0));
        assert_eq!(b + (a - b), a);
    }

    #[test]
    fn cross_sign_encodes_orientation() {
        let right = Vec2::new(1.0, 0.0);
        let up = Vec2::new(0.0, 1.0);
        assert!(right.cross(up) > 0.0);
        assert!(up.cross(right) < 0.0);
        assert_eq!(right.cross(right), 0.0);
    }

    #[test]
    fn perp_is_a_quarter_turn() {
        let v = Vec2::new(3.0, -4.0);
        let p = v.perp();
        assert!(v.dot(p).abs() < EPS);
        assert!((v.length() - p.length()).abs() < EPS);
        // Four quarter turns return to the start.
        assert_eq!(v.perp().perp().perp().perp(), v);
    }

    #[test]
    fn normalize_of_zero_is_zero_not_nan() {
        let n = Vec2::ZERO.normalize();
        assert_eq!(n, Vec2::ZERO);
        assert!(n.is_finite());
    }

    #[test]
    fn normalize_gives_unit_length() {
        let v = Vec2::new(-3.0, 4.0);
        assert!((v.normalize().length() - 1.0).abs() < EPS);
    }

    #[test]
    fn from_angle_round_trips_through_angle() {
        for i in -8..=8 {
            let theta = f64::from(i) * 0.3;
            assert!((Vec2::from_angle(theta).angle() - theta).abs() < EPS);
        }
    }

    #[test]
    fn lerp_hits_both_endpoints_exactly() {
        let a = Point::new(-2.0, 5.0);
        let b = Point::new(11.0, -1.0);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.5), a.midpoint(b));
    }

    #[test]
    fn lerp_endpoints_survive_wildly_separated_magnitudes() {
        // The regression that motivates the symmetric form. With the cheaper
        // `a + (b - a) * t`, the difference cancels `b` away completely and
        // t = 1 returns 0.0 instead of 1.0.
        let a = Point::new(1e300, -1e300);
        let b = Point::new(1.0, 1.0);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(b.lerp(a, 1.0), a);

        let u = Vec2::new(1e300, -1e300);
        let v = Vec2::new(1.0, 1.0);
        assert_eq!(u.lerp(v, 1.0), v);
        assert_eq!(u.lerp(v, 0.0), u);
    }

    #[test]
    fn distance_squared_orders_like_distance() {
        let o = Point::ORIGIN;
        let near = Point::new(1.0, 1.0);
        let far = Point::new(3.0, 0.0);
        assert!(o.distance_squared(near) < o.distance_squared(far));
        assert!(o.distance(near) < o.distance(far));
    }
}
