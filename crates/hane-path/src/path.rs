//! `Path`: a sequence of [`PathEl`] plus the structure everything above it
//! needs -- subpaths, segments that carry their own start point, bounds and
//! transform.
//!
//! # Why segments and not elements
//!
//! [`PathEl`] stores only the points an element introduces: a `LineTo` knows
//! where it ends but not where it begins. Every consumer -- stroking, hit
//! testing, flattening, boolean ops -- needs the start point, so each would
//! otherwise carry the same current-point state machine, and each would get the
//! `ClosePath` case subtly wrong. [`Segment`] is that state machine's output:
//! a self-contained piece of geometry, already a [`QuadBez`] or [`CubicBez`]
//! ready to hand to `hane-geom`.

use hane_geom::{Affine, CubicBez, PathEl, Point, QuadBez, Rect};

/// A path: an ordered sequence of [`PathEl`], made of zero or more subpaths.
///
/// Elements are stored exactly as given. Nothing is normalised on the way in --
/// a path may start without a [`MoveTo`](PathEl::MoveTo), may close twice, may
/// contain lone `MoveTo`s. The iterators below define what those mean rather
/// than the constructor rejecting them, because paths arrive from SVG files and
/// font tables that contain all of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    els: Vec<PathEl>,
}

impl Path {
    /// An empty path.
    #[inline]
    pub const fn new() -> Self {
        Self { els: Vec::new() }
    }

    /// Appends one element.
    #[inline]
    pub fn push(&mut self, el: PathEl) {
        self.els.push(el);
    }

    /// The elements, in order.
    #[inline]
    pub fn elements(&self) -> &[PathEl] {
        &self.els
    }

    /// The subpaths, in order.
    ///
    /// A subpath starts at a [`MoveTo`](PathEl::MoveTo) and runs to just before
    /// the next one, or through a [`ClosePath`](PathEl::ClosePath) if one comes
    /// first. Two consequences worth knowing:
    ///
    /// - A lone `MoveTo` yields a subpath with no elements and no segments. It
    ///   draws nothing when filled, but it is where a zero-length stroke with
    ///   round caps puts its dot, so it is reported rather than dropped.
    /// - Elements following a `ClosePath` begin a new subpath at the *closed*
    ///   subpath's start point, which is what SVG specifies for a `closepath`
    ///   immediately followed by another command.
    pub fn subpaths(&self) -> impl Iterator<Item = Subpath<'_>> {
        Subpaths {
            els: &self.els,
            i: 0,
            // Elements before any MoveTo are drawn from the origin. SVG calls a
            // path not starting with a moveto an error, but files containing
            // one exist and Skia's own path builder resolves it exactly this
            // way, so matching it loses no geometry and invents no special
            // case.
            start: Point::ORIGIN,
        }
    }

    /// Every segment of every subpath, in order, each carrying its start point.
    #[inline]
    pub fn segments(&self) -> impl Iterator<Item = Segment> {
        self.subpaths().flat_map(Subpath::segments)
    }

    /// The bounding box of the geometry this path draws.
    ///
    /// Built from the per-segment [tight bounds](CubicBez::bounding_box), not
    /// from the control points: a control-polygon hull is a valid bound but can
    /// be far larger than the curve, and in P3 every unit of slack is a wasted
    /// quadtree hit.
    ///
    /// Points that no segment reaches -- a lone or trailing `MoveTo` -- are not
    /// included, because nothing is drawn there. An empty path bounds
    /// [`Rect::EMPTY`].
    pub fn bounding_box(&self) -> Rect {
        self.segments()
            .fold(Rect::EMPTY, |b, s| b.union(s.bounding_box()))
    }

    /// This path with `t` applied to every point, control points included.
    pub fn transform(&self, t: Affine) -> Self {
        Self {
            els: self.els.iter().map(|el| el.transform(t)).collect(),
        }
    }
}

impl From<Vec<PathEl>> for Path {
    #[inline]
    fn from(els: Vec<PathEl>) -> Self {
        Self { els }
    }
}

/// One subpath: a start point and the elements drawn from it.
///
/// Borrows its elements from the [`Path`]; it is a view, not a copy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Subpath<'a> {
    start: Point,
    els: &'a [PathEl],
}

impl<'a> Subpath<'a> {
    /// The point this subpath is drawn from, and the point a
    /// [`ClosePath`](PathEl::ClosePath) returns to.
    #[inline]
    pub fn start(self) -> Point {
        self.start
    }

    /// True when the subpath ends with a [`ClosePath`](PathEl::ClosePath).
    #[inline]
    pub fn is_closed(self) -> bool {
        matches!(self.els.last(), Some(PathEl::ClosePath))
    }

    /// The elements after the leading [`MoveTo`](PathEl::MoveTo), including any
    /// trailing [`ClosePath`](PathEl::ClosePath).
    #[inline]
    pub fn elements(self) -> &'a [PathEl] {
        self.els
    }

    /// The segments, in order, each carrying its start point.
    ///
    /// A closed subpath ends with the line back to [`start`](Self::start),
    /// unless it is already there: a zero-length closing line draws nothing and
    /// has no tangent, which would leave the stroker dividing by zero.
    #[inline]
    pub fn segments(self) -> impl Iterator<Item = Segment> + 'a {
        Segments {
            els: self.els,
            i: 0,
            current: self.start,
            start: self.start,
        }
    }
}

/// One drawable piece of a path, with its start point built in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    /// Straight line from the first point to the second.
    Line(Point, Point),
    /// Quadratic Bezier.
    Quad(QuadBez),
    /// Cubic Bezier.
    Cubic(CubicBez),
}

impl Segment {
    /// The point the segment starts at.
    #[inline]
    pub fn start(self) -> Point {
        match self {
            Self::Line(p0, _) => p0,
            Self::Quad(q) => q.p0,
            Self::Cubic(c) => c.p0,
        }
    }

    /// The point the segment ends at.
    #[inline]
    pub fn end(self) -> Point {
        match self {
            Self::Line(_, p1) => p1,
            Self::Quad(q) => q.p2,
            Self::Cubic(c) => c.p3,
        }
    }

    /// The tightest axis-aligned box containing this segment.
    #[inline]
    pub fn bounding_box(self) -> Rect {
        match self {
            Self::Line(p0, p1) => Rect::from_points(p0, p1),
            Self::Quad(q) => q.bounding_box(),
            Self::Cubic(c) => c.bounding_box(),
        }
    }
}

/// Splits an element sequence into subpaths. See [`Path::subpaths`].
struct Subpaths<'a> {
    els: &'a [PathEl],
    i: usize,
    /// Where a subpath that has no leading `MoveTo` begins.
    start: Point,
}

impl<'a> Iterator for Subpaths<'a> {
    type Item = Subpath<'a>;

    fn next(&mut self) -> Option<Subpath<'a>> {
        if self.i >= self.els.len() {
            return None;
        }
        let start = match self.els[self.i] {
            PathEl::MoveTo(p) => {
                self.i += 1;
                p
            }
            // No MoveTo: continue from wherever the previous subpath left the
            // pen, or from the origin at the head of the path.
            _ => self.start,
        };
        let body = self.i;
        let mut end = body;
        while end < self.els.len() {
            match self.els[end] {
                PathEl::MoveTo(_) => break,
                // A ClosePath belongs to the subpath it closes, and ends it.
                PathEl::ClosePath => {
                    end += 1;
                    break;
                }
                _ => end += 1,
            }
        }
        let els = &self.els[body..end];
        self.i = end;
        self.start = match els.last() {
            // Closing returns the pen to the start point, and an empty subpath
            // never moved it.
            Some(PathEl::ClosePath) | None => start,
            Some(el) => el.end_point().unwrap_or(start),
        };
        Some(Subpath { start, els })
    }
}

/// Turns the elements of one subpath into segments. See [`Subpath::segments`].
struct Segments<'a> {
    els: &'a [PathEl],
    i: usize,
    current: Point,
    start: Point,
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        while self.i < self.els.len() {
            let el = self.els[self.i];
            self.i += 1;
            let seg = match el {
                // Unreachable through `Subpath::segments`, whose slice never
                // contains a MoveTo. Handled rather than asserted so the
                // iterator stays total.
                PathEl::MoveTo(p) => {
                    self.start = p;
                    self.current = p;
                    continue;
                }
                PathEl::LineTo(p) => Segment::Line(self.current, p),
                PathEl::QuadTo(c, p) => Segment::Quad(QuadBez::new(self.current, c, p)),
                PathEl::CurveTo(c0, c1, p) => {
                    Segment::Cubic(CubicBez::new(self.current, c0, c1, p))
                }
                PathEl::ClosePath => {
                    let from = self.current;
                    self.current = self.start;
                    if from == self.start {
                        continue;
                    }
                    Segment::Line(from, self.start)
                }
            };
            self.current = seg.end();
            return Some(seg);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::{
        Vec2,
        fuzz::{Rng, check},
    };

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    /// A path exercising all five element kinds in one subpath.
    fn sample() -> Path {
        Path::from(vec![
            PathEl::MoveTo(p(1.0, 1.0)),
            PathEl::LineTo(p(3.0, 1.0)),
            PathEl::QuadTo(p(4.0, 2.0), p(3.0, 3.0)),
            PathEl::CurveTo(p(2.5, 4.0), p(1.5, 4.0), p(1.0, 3.0)),
            PathEl::ClosePath,
        ])
    }

    #[test]
    fn segments_carry_their_start_points() {
        let segs: Vec<_> = sample().segments().collect();
        assert_eq!(
            segs,
            vec![
                Segment::Line(p(1.0, 1.0), p(3.0, 1.0)),
                Segment::Quad(QuadBez::new(p(3.0, 1.0), p(4.0, 2.0), p(3.0, 3.0))),
                Segment::Cubic(CubicBez::new(
                    p(3.0, 3.0),
                    p(2.5, 4.0),
                    p(1.5, 4.0),
                    p(1.0, 3.0)
                )),
                // The closing line, which no element stores the start of.
                Segment::Line(p(1.0, 3.0), p(1.0, 1.0)),
            ]
        );
    }

    #[test]
    fn a_closing_line_of_zero_length_is_not_emitted() {
        let path = Path::from(vec![
            PathEl::MoveTo(p(1.0, 1.0)),
            PathEl::LineTo(p(2.0, 2.0)),
            PathEl::LineTo(p(1.0, 1.0)),
            PathEl::ClosePath,
        ]);
        assert_eq!(path.segments().count(), 2);
        assert!(path.subpaths().next().unwrap().is_closed());
    }

    #[test]
    fn subpath_boundaries_are_movetos_and_closes() {
        let path = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 0.0)),
            PathEl::MoveTo(p(5.0, 5.0)),
            PathEl::LineTo(p(6.0, 5.0)),
            PathEl::ClosePath,
            // No MoveTo after the close: SVG restarts at the closed subpath's
            // own start point.
            PathEl::LineTo(p(9.0, 9.0)),
        ]);
        let subs: Vec<_> = path.subpaths().collect();
        assert_eq!(subs.len(), 3);

        assert_eq!(subs[0].start(), p(0.0, 0.0));
        assert!(!subs[0].is_closed());
        assert_eq!(subs[0].elements().len(), 1);

        assert_eq!(subs[1].start(), p(5.0, 5.0));
        assert!(subs[1].is_closed());
        assert_eq!(subs[1].segments().count(), 2); // the line, then the close

        assert_eq!(subs[2].start(), p(5.0, 5.0));
        assert!(!subs[2].is_closed());
        assert_eq!(
            subs[2].segments().collect::<Vec<_>>(),
            vec![Segment::Line(p(5.0, 5.0), p(9.0, 9.0))]
        );
    }

    #[test]
    fn a_path_not_starting_with_moveto_is_drawn_from_the_origin() {
        let path = Path::from(vec![
            PathEl::LineTo(p(2.0, 0.0)),
            PathEl::LineTo(p(2.0, 2.0)),
            PathEl::ClosePath,
        ]);
        let sub = path.subpaths().next().unwrap();
        assert_eq!(sub.start(), Point::ORIGIN);
        assert!(sub.is_closed());
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec![
                Segment::Line(Point::ORIGIN, p(2.0, 0.0)),
                Segment::Line(p(2.0, 0.0), p(2.0, 2.0)),
                Segment::Line(p(2.0, 2.0), Point::ORIGIN),
            ]
        );
        assert_eq!(path.bounding_box(), Rect::new(0.0, 0.0, 2.0, 2.0));
    }

    #[test]
    fn a_lone_moveto_is_a_subpath_with_no_segments() {
        let path = Path::from(vec![
            PathEl::MoveTo(p(7.0, 7.0)),
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 0.0)),
        ]);
        let subs: Vec<_> = path.subpaths().collect();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].start(), p(7.0, 7.0));
        assert_eq!(subs[0].segments().count(), 0);
        // Nothing is drawn at (7, 7), so it is not in the bounds either.
        assert_eq!(path.bounding_box(), Rect::new(0.0, 0.0, 1.0, 0.0));
    }

    #[test]
    fn an_empty_path_has_no_subpaths_and_empty_bounds() {
        let path = Path::new();
        assert_eq!(path.subpaths().count(), 0);
        assert_eq!(path.segments().count(), 0);
        assert!(path.bounding_box().is_empty());
    }

    #[test]
    fn bounds_come_from_the_curve_not_the_control_points() {
        // The classic arch: y peaks at 0.75, well under the control points at 1.
        let path = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::CurveTo(p(0.0, 1.0), p(1.0, 1.0), p(1.0, 0.0)),
        ]);
        let b = path.bounding_box();
        assert!((b.y1 - 0.75).abs() < 1e-15, "{b:?}");
        assert_eq!((b.x0, b.y0, b.x1), (0.0, 0.0, 1.0));
    }

    #[test]
    fn bounds_span_every_subpath() {
        let path = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 1.0)),
            PathEl::MoveTo(p(-4.0, 2.0)),
            PathEl::LineTo(p(-3.0, 8.0)),
        ]);
        assert_eq!(path.bounding_box(), Rect::new(-4.0, 0.0, 1.0, 8.0));
    }

    #[test]
    fn transform_moves_control_points_too() {
        let t = Affine::translate(Vec2::new(10.0, 0.0)) * Affine::scale(2.0);
        let moved = sample().transform(t);
        for (before, after) in sample().elements().iter().zip(moved.elements()) {
            assert_eq!(&before.transform(t), after);
        }
        // Including the ones only a segment exposes: the quad's control point.
        let Some(Segment::Quad(q)) = moved.segments().nth(1) else {
            panic!("expected a quad");
        };
        assert_eq!(q.p1, t * p(4.0, 2.0));
    }

    /// Random element sequences, weighted towards the structural cases:
    /// closes, lone moves, and elements with no preceding move.
    fn path(r: &mut Rng) -> Path {
        let mut path = Path::new();
        for _ in 0..r.below(9) {
            path.push(match r.below(8) {
                0 | 1 => PathEl::MoveTo(r.point()),
                2 | 3 => PathEl::LineTo(r.point()),
                4 => PathEl::QuadTo(r.point(), r.point()),
                5 => PathEl::CurveTo(r.point(), r.point(), r.point()),
                _ => PathEl::ClosePath,
            });
        }
        path
    }

    #[test]
    fn segments_chain_start_to_end_within_a_subpath() {
        // Bit-exact: the iterator copies points, it does not compute them.
        check("segment chain", 2000, path, |path| {
            path.subpaths().all(|sub| {
                let mut cur = sub.start();
                for seg in sub.segments() {
                    if seg.start() != cur {
                        return false;
                    }
                    cur = seg.end();
                }
                // A closed subpath must leave the pen back at its start.
                !sub.is_closed() || cur == sub.start()
            })
        });
    }

    #[test]
    fn bounds_contain_every_segment_endpoint() {
        // Endpoints are stored coordinates and the box is built from min/max of
        // them, so this holds exactly -- no tolerance to get wrong.
        check("bounds contain endpoints", 2000, path, |path| {
            let b = path.bounding_box();
            path.segments()
                .flat_map(|s| [s.start(), s.end()])
                .all(|q| q.x >= b.x0 && q.x <= b.x1 && q.y >= b.y0 && q.y <= b.y1)
        });
    }

    #[test]
    fn every_drawing_element_yields_at_most_one_segment() {
        check("segment count", 2000, path, |path| {
            let drawing = path.elements().iter().filter(|el| !el.is_move()).count();
            let n = path.segments().count();
            // Fewer only when a close was already at its start point.
            n <= drawing
                && n >= drawing
                    - path
                        .elements()
                        .iter()
                        .filter(|el| **el == PathEl::ClosePath)
                        .count()
        });
    }
}
