//! Path element vocabulary.
//!
//! This is the shared type every other crate speaks: `hane-svg` parses into it,
//! `hane-path` strokes and combines it, `hane-raster` and `hane-gpu` fill it,
//! `hane-text` emits glyph outlines as it.
//!
//! Higher-level path structure -- subpaths, winding, stroking, boolean ops --
//! belongs to `hane-path`. This module is only the alphabet.

use crate::{Affine, Point};

/// A single path element.
///
/// A well-formed path begins with [`MoveTo`](PathEl::MoveTo); every other
/// element continues from the current point, which is why only the new points
/// are stored. Arcs are deliberately absent: SVG elliptical arcs are converted
/// to cubics at parse time so that everything downstream handles exactly three
/// segment kinds instead of four.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    /// Begin a new subpath at this point.
    MoveTo(Point),
    /// Straight line from the current point.
    LineTo(Point),
    /// Quadratic Bezier: one control point, then the end point.
    QuadTo(Point, Point),
    /// Cubic Bezier: two control points, then the end point.
    CurveTo(Point, Point, Point),
    /// Close the current subpath with a straight line back to its start.
    ClosePath,
}

impl PathEl {
    /// The point this element ends at, or `None` for [`ClosePath`], whose end
    /// point is the subpath start rather than anything stored here.
    #[inline]
    pub fn end_point(self) -> Option<Point> {
        match self {
            Self::MoveTo(p) | Self::LineTo(p) | Self::QuadTo(_, p) | Self::CurveTo(_, _, p) => {
                Some(p)
            }
            Self::ClosePath => None,
        }
    }

    /// This element with `t` applied to each of its points.
    #[inline]
    pub fn transform(self, t: Affine) -> Self {
        match self {
            Self::MoveTo(p) => Self::MoveTo(t * p),
            Self::LineTo(p) => Self::LineTo(t * p),
            Self::QuadTo(c, p) => Self::QuadTo(t * c, t * p),
            Self::CurveTo(c0, c1, p) => Self::CurveTo(t * c0, t * c1, t * p),
            Self::ClosePath => Self::ClosePath,
        }
    }

    /// True when this element starts a new subpath.
    #[inline]
    pub fn is_move(self) -> bool {
        matches!(self, Self::MoveTo(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vec2;

    #[test]
    fn close_path_has_no_stored_end_point() {
        assert_eq!(PathEl::ClosePath.end_point(), None);
    }

    #[test]
    fn every_other_element_reports_its_end_point() {
        let p = Point::new(4.0, 5.0);
        assert_eq!(PathEl::MoveTo(p).end_point(), Some(p));
        assert_eq!(PathEl::LineTo(p).end_point(), Some(p));
        assert_eq!(PathEl::QuadTo(Point::ORIGIN, p).end_point(), Some(p));
        assert_eq!(
            PathEl::CurveTo(Point::ORIGIN, Point::ORIGIN, p).end_point(),
            Some(p)
        );
    }

    #[test]
    fn transform_moves_control_points_too() {
        let t = Affine::translate(Vec2::new(1.0, 2.0));
        let el = PathEl::CurveTo(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
        );
        assert_eq!(
            el.transform(t),
            PathEl::CurveTo(
                Point::new(1.0, 2.0),
                Point::new(2.0, 3.0),
                Point::new(3.0, 4.0),
            )
        );
    }

    #[test]
    fn transforming_close_path_is_a_no_op() {
        let t = Affine::rotate(1.0) * Affine::translate(Vec2::new(9.0, 9.0));
        assert_eq!(PathEl::ClosePath.transform(t), PathEl::ClosePath);
    }
}
