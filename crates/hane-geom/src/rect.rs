//! Axis-aligned rectangles.

use crate::{Affine, Point, Vec2};

/// An axis-aligned rectangle, held as two corners.
///
/// `new` stores the coordinates exactly as given and does not normalise, so a
/// rectangle can be "backwards" (`x1 < x0`) and therefore empty. That is what
/// makes [`Rect::EMPTY`] usable as the identity for [`union`](Rect::union),
/// which is how bounding boxes get accumulated everywhere in the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Minimum x, for a normalised rectangle.
    pub x0: f64,
    /// Minimum y, for a normalised rectangle.
    pub y0: f64,
    /// Maximum x, for a normalised rectangle.
    pub x1: f64,
    /// Maximum y, for a normalised rectangle.
    pub y1: f64,
}

impl Rect {
    /// The degenerate rectangle at the origin.
    pub const ZERO: Self = Self {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    /// The identity for [`union`](Rect::union): inverted infinite bounds.
    ///
    /// Accumulate bounding boxes by folding [`union`](Rect::union) over this.
    pub const EMPTY: Self = Self {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };

    /// A rectangle from explicit bounds, stored as given.
    #[inline]
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// The smallest rectangle containing both points.
    #[inline]
    pub fn from_points(a: Point, b: Point) -> Self {
        Self {
            x0: a.x.min(b.x),
            y0: a.y.min(b.y),
            x1: a.x.max(b.x),
            y1: a.y.max(b.y),
        }
    }

    /// A rectangle from a corner and a size.
    #[inline]
    pub fn from_origin_size(origin: Point, size: Vec2) -> Self {
        Self::from_points(origin, origin + size)
    }

    /// Width. Negative for a non-normalised rectangle.
    #[inline]
    pub fn width(self) -> f64 {
        self.x1 - self.x0
    }

    /// Height. Negative for a non-normalised rectangle.
    #[inline]
    pub fn height(self) -> f64 {
        self.y1 - self.y0
    }

    /// Area. Zero for an empty rectangle.
    #[inline]
    pub fn area(self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.width() * self.height()
        }
    }

    /// True when the rectangle encloses nothing.
    #[inline]
    pub fn is_empty(self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    /// The centre point.
    #[inline]
    pub fn center(self) -> Point {
        Point::new(0.5 * (self.x0 + self.x1), 0.5 * (self.y0 + self.y1))
    }

    /// The minimum corner.
    #[inline]
    pub const fn origin(self) -> Point {
        Point::new(self.x0, self.y0)
    }

    /// Width and height as a displacement.
    #[inline]
    pub fn size(self) -> Vec2 {
        Vec2::new(self.width(), self.height())
    }

    /// The smallest rectangle containing both `self` and `other`.
    #[inline]
    pub fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// The smallest rectangle containing `self` and `p`.
    #[inline]
    pub fn union_point(self, p: Point) -> Self {
        Self {
            x0: self.x0.min(p.x),
            y0: self.y0.min(p.y),
            x1: self.x1.max(p.x),
            y1: self.y1.max(p.y),
        }
    }

    /// The overlap of `self` and `other`, which may be empty.
    #[inline]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// True when `self` and `other` share any area.
    ///
    /// The viewport-culling predicate, and so one of the hottest calls in the
    /// engine.
    #[inline]
    pub fn overlaps(self, other: Self) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    /// True when `p` lies inside, treating the minimum edges as inclusive and
    /// the maximum edges as exclusive.
    #[inline]
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x0 && p.x < self.x1 && p.y >= self.y0 && p.y < self.y1
    }

    /// True when `other` lies wholly inside `self`.
    #[inline]
    pub fn contains_rect(self, other: Self) -> bool {
        self.x0 <= other.x0 && self.y0 <= other.y0 && self.x1 >= other.x1 && self.y1 >= other.y1
    }

    /// Grown by `d` on every side. A negative `d` shrinks, possibly to empty.
    #[inline]
    pub fn inflate(self, d: f64) -> Self {
        Self {
            x0: self.x0 - d,
            y0: self.y0 - d,
            x1: self.x1 + d,
            y1: self.y1 + d,
        }
    }

    /// Moved by `v`.
    #[inline]
    pub fn translate(self, v: Vec2) -> Self {
        Self {
            x0: self.x0 + v.x,
            y0: self.y0 + v.y,
            x1: self.x1 + v.x,
            y1: self.y1 + v.y,
        }
    }

    /// The bounding box of this rectangle's four transformed corners.
    ///
    /// Under rotation this is larger than the transformed rectangle itself --
    /// it is a bound, not an image.
    pub fn transform(self, t: Affine) -> Self {
        if self.is_empty() {
            return Self::EMPTY;
        }
        let corners = [
            t * Point::new(self.x0, self.y0),
            t * Point::new(self.x1, self.y0),
            t * Point::new(self.x0, self.y1),
            t * Point::new(self.x1, self.y1),
        ];
        corners
            .iter()
            .fold(Self::EMPTY, |acc, &p| acc.union_point(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_the_union_identity() {
        let r = Rect::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(Rect::EMPTY.union(r), r);
        assert_eq!(r.union(Rect::EMPTY), r);
        assert!(Rect::EMPTY.is_empty());
    }

    #[test]
    fn accumulating_points_from_empty_gives_a_tight_box() {
        let pts = [
            Point::new(3.0, -1.0),
            Point::new(-2.0, 5.0),
            Point::new(0.0, 0.0),
        ];
        let bbox = pts.iter().fold(Rect::EMPTY, |acc, &p| acc.union_point(p));
        assert_eq!(bbox, Rect::new(-2.0, -1.0, 3.0, 5.0));
    }

    #[test]
    fn degenerate_rects_are_empty() {
        assert!(Rect::ZERO.is_empty());
        assert!(Rect::new(5.0, 0.0, 1.0, 10.0).is_empty());
        assert!(!Rect::new(0.0, 0.0, 1.0, 1.0).is_empty());
    }

    #[test]
    fn overlaps_agrees_with_a_non_empty_intersection() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let cases = [
            Rect::new(5.0, 5.0, 15.0, 15.0),
            Rect::new(20.0, 20.0, 30.0, 30.0),
            Rect::new(-5.0, -5.0, 5.0, 5.0),
            Rect::new(10.0, 0.0, 20.0, 10.0), // edge-touching, not overlapping
        ];
        for b in cases {
            assert_eq!(a.overlaps(b), !a.intersect(b).is_empty(), "{b:?}");
        }
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(Point::new(0.0, 0.0)));
        assert!(r.contains(Point::new(9.999, 9.999)));
        assert!(!r.contains(Point::new(10.0, 5.0)));
        assert!(!r.contains(Point::new(5.0, 10.0)));
    }

    #[test]
    fn inflate_then_deflate_round_trips() {
        let r = Rect::new(1.0, 2.0, 9.0, 8.0);
        assert_eq!(r.inflate(3.0).inflate(-3.0), r);
    }

    #[test]
    fn transform_bounds_a_rotated_rect() {
        let r = Rect::new(-1.0, -1.0, 1.0, 1.0);
        let t = Affine::rotate(core::f64::consts::FRAC_PI_4);
        let b = r.transform(t);
        // The unit square rotated 45 degrees spans sqrt(2) each way.
        let half = 2.0_f64.sqrt();
        assert!((b.x0 + half).abs() < 1e-9);
        assert!((b.x1 - half).abs() < 1e-9);
        // A bound, not an image: it is strictly larger than the original.
        assert!(b.area() > r.area());
    }

    #[test]
    fn transforming_empty_stays_empty() {
        assert!(Rect::EMPTY.transform(Affine::scale(2.0)).is_empty());
    }
}
