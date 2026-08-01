//! Stroke expansion: the outline a path fills when it is drawn with a pen.
//!
//! # One traversal
//!
//! Offsets, joins, caps, dashes and alignment are one algorithm, not four.
//! They share the walk over [`Subpath::segments`], and more importantly they
//! share its degenerate cases: a segment with no tangent, a subpath with no
//! segments, a corner that is not a corner. Split apart, each would grow its
//! own answer to those and the four answers would not agree. So there is one
//! entry point, [`Path::stroke`], and every stage below is a step in it:
//!
//! 1. [`Path::dash`] cuts the path into the pattern's "on" spans, if any.
//! 2. Segments with no direction at all are dropped, once. That is what stops
//!    a zero-length line from emitting a join for a corner that is not there.
//! 3. A subpath with nothing left is a *dot* -- a round or square cap with no
//!    length under it -- rather than nothing. A lone `MoveTo` and a zero-length
//!    dash arrive here by the same route and get the same answer.
//! 4. Each side is offset ([`Segment::offset`]), consecutive offsets are
//!    joined, and the two sides are closed with caps, or left as two contours
//!    when the subpath is closed and there is nothing to cap.
//!
//! # Winding
//!
//! The result is filled **nonzero**, and that is load-bearing. The inner side
//! of a tight corner, the loop an offset makes where the curvature radius
//! drops below the offset distance, and the overlap where a stroke doubles
//! back on itself are all left in the outline rather than trimmed out. Under
//! nonzero they are already the same filled region; trimming them needs the
//! boolean machinery of P6, and would change no pixels.

mod dash;
mod offset;

use hane_geom::{CubicBez, PathEl, Point, QuadBez, Vec2};

use crate::{Path, Segment, Subpath};

/// Used when a style asks for a tolerance that cannot be met -- zero, negative
/// or NaN -- in place of refusing to draw.
const DEFAULT_TOLERANCE: f64 = 0.1;

/// How the outline turns a corner between two segments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Join {
    /// Extend both edges until they meet, falling back to
    /// [`Bevel`](Self::Bevel) past [`StrokeStyle::miter_limit`]. The SVG
    /// default.
    #[default]
    Miter,
    /// A circular arc of the stroke's radius about the corner.
    Round,
    /// A straight edge across the gap.
    Bevel,
}

/// How the outline ends an open subpath.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cap {
    /// Square off at the endpoint, adding nothing. The SVG default.
    #[default]
    Butt,
    /// A half circle of the stroke's radius about the endpoint.
    Round,
    /// Square off half a width past the endpoint.
    Square,
}

/// Where the stroke sits relative to the path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Half the width either side. The only alignment SVG has.
    #[default]
    Center,
    /// The full width inside the fill.
    Inside,
    /// The full width outside the fill.
    Outside,
}

// Inside and outside are not SVG features, and they are defined against the
// *fill*, not against the path: the stroke is the ring between the fill's
// boundary and that boundary offset by the whole width. Two consequences worth
// knowing before reading `stroke_subpath`:
//
// - An open subpath is closed implicitly, because that is what its fill is.
// - Neither alignment ever caps. A ring has no ends.

/// Everything that decides what a stroke looks like.
///
/// The defaults are SVG's: width 1, miter joins with a limit of 4, butt caps,
/// no dashes, centred.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    /// Total width of the stroke. Zero or negative draws nothing at all --
    /// not a hairline.
    pub width: f64,
    /// How corners are turned.
    pub join: Join,
    /// How the ends of open subpaths are finished.
    pub cap: Cap,
    /// The largest ratio of miter length to stroke width a miter join may
    /// reach before it becomes a bevel. SVG's default is 4, which bevels
    /// corners sharper than about 29 degrees.
    pub miter_limit: f64,
    /// Where the stroke sits relative to the path.
    pub align: Align,
    /// Alternating on and off lengths, in arc length. Empty for a solid
    /// stroke. An odd-length pattern repeats doubled, per SVG.
    pub dash: Vec<f64>,
    /// How far into the dash pattern each subpath starts.
    pub dash_offset: f64,
    /// The largest distance the outline may stray from the true one, in
    /// document units. Divide the device-pixel budget by
    /// [`Affine::max_scale`](hane_geom::Affine::max_scale) of the view
    /// transform to get it.
    pub tolerance: f64,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            join: Join::default(),
            cap: Cap::default(),
            miter_limit: 4.0,
            align: Align::default(),
            dash: Vec::new(),
            dash_offset: 0.0,
            tolerance: DEFAULT_TOLERANCE,
        }
    }
}

impl Path {
    /// The outline this path fills when stroked with `style`, wound nonzero.
    ///
    /// The result is a fill: pass it to the rasteriser as an ordinary path.
    /// It is closed subpath by subpath, and it may overlap itself -- see the
    /// module docs on winding.
    ///
    /// A width that is not positive gives an empty path. A hairline is a
    /// rendering concept, not a geometric one; there is no outline of zero
    /// width, and returning one pixel of something would be a different
    /// drawing from the one that was asked for.
    ///
    /// ```
    /// use hane_geom::{PathEl, Point};
    /// use hane_path::{Path, StrokeStyle};
    ///
    /// let line = Path::from(vec![
    ///     PathEl::MoveTo(Point::new(0.0, 0.0)),
    ///     PathEl::LineTo(Point::new(10.0, 0.0)),
    /// ]);
    /// let style = StrokeStyle {
    ///     width: 2.0,
    ///     ..StrokeStyle::default()
    /// };
    /// // A butt-capped straight line strokes to its own rectangle.
    /// assert_eq!(
    ///     line.stroke(&style).bounding_box(),
    ///     hane_geom::Rect::new(0.0, -1.0, 10.0, 1.0)
    /// );
    /// ```
    pub fn stroke(&self, style: &StrokeStyle) -> Path {
        // Negated so a NaN or infinite width draws nothing too, rather than
        // propagating into every coordinate below.
        if !(style.width > 0.0 && style.width.is_finite()) {
            return Path::new();
        }
        let tol = if style.tolerance > 0.0 {
            style.tolerance
        } else {
            DEFAULT_TOLERANCE
        };
        let dashed;
        let src = if style.dash.is_empty() {
            self
        } else {
            dashed = self.dash(&style.dash, style.dash_offset);
            &dashed
        };
        let mut els = Vec::new();
        for sub in src.subpaths() {
            stroke_subpath(sub, style, tol, &mut els);
        }
        Path::from(els)
    }
}

/// The outline of one subpath, appended to `els`.
fn stroke_subpath(sub: Subpath<'_>, style: &StrokeStyle, tol: f64, els: &mut Vec<PathEl>) {
    // Dropped once, here. A zero-length segment has no tangent, so every
    // formula below would divide by zero on it -- and it is also a corner that
    // does not exist, which is why removing it is what stops the spurious
    // join rather than a special case inside `join`.
    let mut segs: Vec<Segment> = sub.segments().filter(|s| !is_point(*s)).collect();
    let Some(&first) = segs.first() else {
        // Nothing to walk: a lone `MoveTo`, a subpath of zero-length lines, or
        // a zero-length dash. All three draw the same dot.
        if style.align == Align::Center {
            dot(sub.start(), style, els);
        }
        return;
    };
    let last = segs[segs.len() - 1];
    let closed = sub.is_closed();

    if style.align == Align::Center {
        let d = style.width * 0.5;
        let rev = reversed(&segs);
        if closed {
            // Two contours and no caps: the seam is a corner like any other,
            // so it takes a join.
            offset_side(&segs, true, d, style, tol, els, true);
            offset_side(&rev, true, d, style, tol, els, true);
        } else {
            offset_side(&segs, false, d, style, tol, els, true);
            cap(els, last.end(), end_tangent(last), d, style.cap);
            offset_side(&rev, false, d, style, tol, els, false);
            cap(
                els,
                first.start(),
                end_tangent(rev[rev.len() - 1]),
                d,
                style.cap,
            );
            els.push(PathEl::ClosePath);
        }
        return;
    }

    // Inside and outside are defined against the *fill*, and the fill of an
    // open subpath is its implicit closure, so the ring below is built on the
    // closed contour either way. That also means these alignments never cap:
    // there is no end to cap, only a ring between two boundaries.
    if !closed && last.end() != first.start() {
        segs.push(Segment::Line(last.end(), first.start()));
    }
    let rev = reversed(&segs);
    // Which way is out depends on which way the contour is wound. Reversing
    // the traversal flips both the side an offset lands on and the side we
    // want, so the same signed distance serves both alignments.
    let d = if signed_area(&segs) < 0.0 {
        style.width
    } else {
        -style.width
    };
    // Nonzero winding does the clipping: the offset boundary and the fill
    // boundary wound oppositely leave the ring between them at winding 1 and
    // the fill's interior at 0. No boolean operation, and none needed.
    match style.align {
        Align::Outside => {
            offset_side(&segs, true, d, style, tol, els, true);
            contour(&rev, els);
        }
        _ => {
            contour(&segs, els);
            offset_side(&rev, true, d, style, tol, els, true);
        }
    }
}

/// Appends the offset of `segs` at signed distance `d`, joined at every
/// interior corner and at the seam too when `closed`.
///
/// `begin` starts a new contour with a `MoveTo`; the second side of an open
/// stroke continues from the cap that precedes it instead.
fn offset_side(
    segs: &[Segment],
    closed: bool,
    d: f64,
    style: &StrokeStyle,
    tol: f64,
    els: &mut Vec<PathEl>,
    begin: bool,
) {
    let mut buf = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            join(els, seg.start(), segs[i - 1], *seg, d, style, tol);
        }
        buf.clear();
        seg.offset(d, tol, &mut buf);
        if i == 0 && begin {
            els.push(PathEl::MoveTo(
                buf.first().map_or_else(|| seg.start(), |s| s.start()),
            ));
        }
        for s in &buf {
            push_seg(els, *s);
        }
    }
    if closed {
        let last = segs[segs.len() - 1];
        join(els, segs[0].start(), last, segs[0], d, style, tol);
        els.push(PathEl::ClosePath);
    }
}

/// The corner at `p` between the offsets of `prev` and `next`.
///
/// Ends at `next`'s offset start point, so the chain continues from it.
fn join(
    els: &mut Vec<PathEl>,
    p: Point,
    prev: Segment,
    next: Segment,
    d: f64,
    style: &StrokeStyle,
    tol: f64,
) {
    let t0 = end_tangent(prev);
    let t1 = start_tangent(next);
    let a = p + t0.perp() * d;
    let b = p + t1.perp() * d;
    // Not a corner. Two segments meeting smoothly still have tangents that
    // differ in the last bit, and the gap that leaves is below any tolerance
    // worth drawing -- but the *direction* of that gap is rounding noise, and
    // a round join that reads it backwards sweeps the long way round and
    // draws a whole circle. The current point carries the outline across the
    // gap instead.
    if t0 == t1 || a.distance(b) <= tol {
        return;
    }
    let dot = t0.dot(t1);
    // The miter ratio, `1 / sin(theta/2)` for an included angle `theta`,
    // written through the half-angle identity so it needs no trigonometry and
    // no line intersection: `sin(theta/2) = cos(phi/2)` for a turn of `phi`.
    let half = (0.5 * (1.0 + dot)).max(0.0).sqrt();
    // The intersection of the two offset lines, on the bisector. Zero for a
    // reversal, where `normalize` gives zero rather than NaN and the miter is
    // out of reach anyway.
    let miter = p + (t0.perp() + t1.perp()).normalize() * (d / half);

    // The offset is on the outside of the turn -- leaving a gap to fill --
    // when the turn and the offset have opposite signs.
    if t0.cross(t1) * d < 0.0 {
        match style.join {
            // SVG measures the miter length against the stroke *width*, which
            // is twice the offset distance, so the ratio compared here is the
            // same number on both sides of the scale. Strictly greater bevels:
            // a corner exactly at the limit still miters.
            Join::Miter if 1.0 / half <= style.miter_limit => els.push(PathEl::LineTo(miter)),
            Join::Round => {
                let mut buf = Vec::new();
                // The offset points rotate with the tangents, so the sweep
                // between them is the turn itself: under a half turn on the
                // outside, and the way the path turns.
                arc(p, a, b, t0.cross(t1) > 0.0, &mut buf);
                for s in &buf {
                    push_seg(els, *s);
                }
            }
            _ => {}
        }
    } else {
        // The inside of the turn, where the two offsets overlap instead of
        // gapping. The trim is their intersection -- the same miter point --
        // but only while it is close enough to be covered by both segments;
        // past that it is a spike sticking out of the stroke, and the corner
        // itself is the nearest point that is certainly inside it.
        let reach = prev
            .start()
            .distance(prev.end())
            .min(next.start().distance(next.end()));
        els.push(PathEl::LineTo(
            if miter.is_finite() && (miter - p).length() <= reach {
                miter
            } else {
                p
            },
        ));
    }
    els.push(PathEl::LineTo(b));
}

/// The end of an open stroke at `p`, from `p + perp(t)*d` round to
/// `p - perp(t)*d`, where `t` is the direction of travel.
fn cap(els: &mut Vec<PathEl>, p: Point, t: Vec2, d: f64, style: Cap) {
    let n = t.perp() * d;
    match style {
        Cap::Butt => els.push(PathEl::LineTo(p - n)),
        Cap::Square => {
            let e = t * d.abs();
            els.push(PathEl::LineTo(p + n + e));
            els.push(PathEl::LineTo(p - n + e));
            els.push(PathEl::LineTo(p - n));
        }
        Cap::Round => {
            let mut buf = Vec::new();
            half_circle(p, t, d, &mut buf);
            for s in &buf {
                push_seg(els, *s);
            }
        }
    }
}

/// A subpath with no length: a dot under a round cap, a square under a square
/// one, and nothing at all under a butt cap, which has no extent to draw.
fn dot(p: Point, style: &StrokeStyle, els: &mut Vec<PathEl>) {
    let d = style.width * 0.5;
    // A zero-length subpath has no direction. SVG leaves it undefined; every
    // renderer picks the x axis, and so does this.
    let t = Vec2::new(1.0, 0.0);
    match style.cap {
        Cap::Butt => {}
        Cap::Round => {
            let mut buf = Vec::new();
            half_circle(p, t, d, &mut buf);
            half_circle(p, -t, d, &mut buf);
            // Empty when the two ends of the first half round to the same
            // point -- a width small enough to vanish beside the coordinate it
            // sits at. Nothing is the right drawing there, and it is also the
            // difference between that and an index out of range.
            if let Some(start) = buf.first() {
                els.push(PathEl::MoveTo(start.start()));
                for s in &buf {
                    push_seg(els, *s);
                }
                els.push(PathEl::ClosePath);
            }
        }
        Cap::Square => {
            els.push(PathEl::MoveTo(Point::new(p.x - d, p.y - d)));
            els.push(PathEl::LineTo(Point::new(p.x + d, p.y - d)));
            els.push(PathEl::LineTo(Point::new(p.x + d, p.y + d)));
            els.push(PathEl::LineTo(Point::new(p.x - d, p.y + d)));
            els.push(PathEl::ClosePath);
        }
    }
}

/// The half circle of radius `|d|` about `p`, from `p + perp(t)*d` to
/// `p - perp(t)*d`, bulging along `t`.
///
/// The round cap -- and, in [`offset`](Segment::offset), what a cusp needs,
/// which is the same shape for the same reason.
fn half_circle(p: Point, t: Vec2, d: f64, out: &mut Vec<Segment>) {
    let n = t.perp() * d;
    // Going from `+n` to `-n` the long way round through `p + t*|d|` means
    // decreasing angle for a positive `d`.
    arc(p, p + n, p - n, d < 0.0, out);
}

/// A circular arc about `center` from `from` to `to`, turning `ccw`.
pub(super) fn arc(center: Point, from: Point, to: Point, ccw: bool, out: &mut Vec<Segment>) {
    let r = (from - center).length();
    let mut sweep = (to - center).angle() - (from - center).angle();
    if ccw && sweep < 0.0 {
        sweep += core::f64::consts::TAU;
    } else if !ccw && sweep > 0.0 {
        sweep -= core::f64::consts::TAU;
    }
    let mut cur = from;
    for el in PathEl::arc(
        from,
        Vec2::new(r, r),
        0.0,
        sweep.abs() > core::f64::consts::PI,
        ccw,
        to,
    ) {
        cur = match el {
            PathEl::CurveTo(c0, c1, e) => {
                out.push(Segment::Cubic(CubicBez::new(cur, c0, c1, e)));
                e
            }
            // `arc` draws a chord when the radius is zero or the arithmetic
            // overflows. Both are finite and both are the least wrong thing.
            other => {
                let e = other.end_point().unwrap_or(cur);
                out.push(Segment::Line(cur, e));
                e
            }
        };
    }
}

/// Appends `segs` as one closed contour of its own.
fn contour(segs: &[Segment], els: &mut Vec<PathEl>) {
    els.push(PathEl::MoveTo(segs[0].start()));
    for s in segs {
        push_seg(els, *s);
    }
    els.push(PathEl::ClosePath);
}

/// The same segments walked backwards, each reversed.
fn reversed(segs: &[Segment]) -> Vec<Segment> {
    segs.iter().rev().map(|s| reverse(*s)).collect()
}

/// One segment, traversed the other way. Control points reverse with it, so
/// the geometry is identical and the endpoints are exact.
fn reverse(s: Segment) -> Segment {
    match s {
        Segment::Line(p0, p1) => Segment::Line(p1, p0),
        Segment::Quad(q) => Segment::Quad(QuadBez::new(q.p2, q.p1, q.p0)),
        Segment::Cubic(c) => Segment::Cubic(CubicBez::new(c.p3, c.p2, c.p1, c.p0)),
    }
}

/// True when every control point sits on the start point, so the segment has
/// no direction anywhere.
fn is_point(s: Segment) -> bool {
    match s {
        Segment::Line(p0, p1) => p0 == p1,
        Segment::Quad(q) => q.p0 == q.p1 && q.p1 == q.p2,
        Segment::Cubic(c) => c.p0 == c.p1 && c.p1 == c.p2 && c.p2 == c.p3,
    }
}

/// This segment as a cubic. Exact for all three kinds: elevating a line or a
/// quadratic preserves the curve and its parameterisation.
fn as_cubic(s: Segment) -> CubicBez {
    match s {
        Segment::Line(p0, p1) => {
            CubicBez::new(p0, p0.lerp(p1, 1.0 / 3.0), p0.lerp(p1, 2.0 / 3.0), p1)
        }
        Segment::Quad(q) => q.to_cubic(),
        Segment::Cubic(c) => c,
    }
}

/// The unit direction leaving the start of `s`.
fn start_tangent(s: Segment) -> Vec2 {
    match s {
        Segment::Line(p0, p1) => (p1 - p0).normalize(),
        _ => offset::tangent(as_cubic(s), 0.0, true),
    }
}

/// The unit direction arriving at the end of `s`.
fn end_tangent(s: Segment) -> Vec2 {
    match s {
        Segment::Line(p0, p1) => (p1 - p0).normalize(),
        _ => offset::tangent(as_cubic(s), 1.0, false),
    }
}

/// Appends `s` as the element that draws it from the current point.
fn push_seg(els: &mut Vec<PathEl>, s: Segment) {
    els.push(match s {
        Segment::Line(_, p) => PathEl::LineTo(p),
        Segment::Quad(q) => PathEl::QuadTo(q.p1, q.p2),
        Segment::Cubic(c) => PathEl::CurveTo(c.p1, c.p2, c.p3),
    });
}

/// The signed area enclosed by the closed contour `segs`; positive when it
/// winds counter-clockwise.
///
/// Green's theorem, integrated exactly rather than off a flattened polygon:
/// the integrand is degree five for a cubic and three-point Gauss-Legendre is
/// exact through degree five. Only the *sign* is used, to decide which side is
/// out, and a flattening error big enough to flip it on a thin shape would put
/// the whole stroke on the wrong side.
fn signed_area(segs: &[Segment]) -> f64 {
    /// Half of `sqrt(3/5)`: the outer Gauss nodes, mapped onto `[0, 1]`.
    const H: f64 = 0.387_298_334_620_741_7;
    const NODES: [f64; 3] = [0.5 - H, 0.5, 0.5 + H];
    const WEIGHTS: [f64; 3] = [5.0 / 18.0, 8.0 / 18.0, 5.0 / 18.0];

    let mut area = 0.0;
    for &s in segs {
        let c = as_cubic(s);
        for (t, w) in NODES.into_iter().zip(WEIGHTS) {
            let p = c.eval(t);
            let v = c.deriv_at(t);
            area += w * (p.x * v.y - p.y * v.x);
        }
    }
    area * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::{
        Rect,
        fuzz::{Rng, check},
    };

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn line(a: Point, b: Point) -> Path {
        Path::from(vec![PathEl::MoveTo(a), PathEl::LineTo(b)])
    }

    /// The corner path every join test uses: right along x, then up.
    fn corner(turn: f64) -> Path {
        Path::from(vec![
            PathEl::MoveTo(p(-10.0, 0.0)),
            PathEl::LineTo(p(0.0, 0.0)),
            PathEl::LineTo(p(10.0 * turn.cos(), 10.0 * turn.sin())),
        ])
    }

    fn style(width: f64) -> StrokeStyle {
        StrokeStyle {
            width,
            tolerance: 1e-6,
            ..StrokeStyle::default()
        }
    }

    /// Every point the outline passes through, densely.
    fn samples(path: &Path) -> Vec<Point> {
        let mut out = Vec::new();
        for s in path.segments() {
            for i in 0..=32 {
                let t = f64::from(i) / 32.0;
                out.push(match s {
                    Segment::Line(a, b) => a.lerp(b, t),
                    Segment::Quad(q) => q.eval(t),
                    Segment::Cubic(c) => c.eval(t),
                });
            }
        }
        out
    }

    /// The nonzero winding number of `path` about `q`, off a flattened
    /// polygon. Test-only: the engine's own fill lives in `hane-raster`.
    fn winding(path: &Path, q: Point) -> i32 {
        let mut w = 0;
        for sub in path.subpaths() {
            let mut poly = vec![sub.start()];
            for s in sub.segments() {
                match s {
                    Segment::Line(_, b) => poly.push(b),
                    Segment::Quad(c) => c.flatten(1e-4, &mut poly),
                    Segment::Cubic(c) => c.flatten(1e-4, &mut poly),
                }
            }
            poly.push(poly[0]);
            for e in poly.windows(2) {
                let (a, b) = (e[0], e[1]);
                if (a.y <= q.y) != (b.y <= q.y) {
                    let side = (b - a).cross(q - a);
                    if (side > 0.0) == (b.y > a.y) {
                        w += if b.y > a.y { 1 } else { -1 };
                    }
                }
            }
        }
        w
    }

    fn max_distance_to(path: &Path, q: Point) -> f64 {
        samples(path)
            .into_iter()
            .fold(0.0f64, |m, s| m.max(s.distance(q)))
    }

    /// How far the outline reaches past the corner of [`corner`] along the
    /// outward bisector -- the one number that tells the three join styles
    /// apart. For an offset `d` and an included angle `theta` it is
    /// `d / sin(theta/2)` for a miter, `d` for a round join, and
    /// `d * sin(theta/2)` for a bevel.
    fn outer_reach(path: &Path, turn: f64) -> f64 {
        let t0 = Vec2::new(1.0, 0.0);
        let t1 = Vec2::new(turn.cos(), turn.sin());
        let u = -(t0.perp() + t1.perp()).normalize();
        samples(path)
            .into_iter()
            .fold(0.0f64, |m, s| m.max((s - Point::ORIGIN).dot(u)))
    }

    // -- offsets and the basic shape (#33, #34) ---------------------------

    #[test]
    fn a_straight_line_strokes_to_its_rectangle() {
        let out = line(p(0.0, 0.0), p(10.0, 0.0)).stroke(&style(4.0));
        assert_eq!(out.bounding_box(), Rect::new(0.0, -2.0, 10.0, 2.0));
        // Butt caps add nothing: four corners, four edges, no more.
        assert_eq!(out.segments().count(), 4);
        assert_eq!(winding(&out, p(5.0, 0.0)).abs(), 1);
        assert_eq!(winding(&out, p(5.0, 3.0)), 0);
        assert_eq!(winding(&out, p(-1.0, 0.0)), 0);
    }

    #[test]
    fn zero_width_renders_nothing() {
        for w in [0.0, -1.0, f64::NAN] {
            let out = line(p(0.0, 0.0), p(10.0, 0.0)).stroke(&style(w));
            assert_eq!(out.elements().len(), 0, "width {w}");
        }
    }

    // -- joins (#34) ------------------------------------------------------

    #[test]
    fn a_miter_join_reaches_its_reference_tip() {
        // A right-angle turn, width 2: the outer tip is on the bisector at
        // 1 / sin(45 deg) = sqrt(2) from the corner, which for this corner is
        // the point (1, -1).
        let out = corner(core::f64::consts::FRAC_PI_2).stroke(&style(2.0));
        let expected = p(1.0, -1.0);
        assert!(
            samples(&out).iter().any(|s| s.distance(expected) < 1e-12),
            "miter tip {expected:?} missing from {out:?}"
        );
        assert!((outer_reach(&out, core::f64::consts::FRAC_PI_2) - 2.0f64.sqrt()).abs() < 1e-12);
        // And nothing on the outline is farther from the corner than the tip.
        assert!(max_distance_to(&out, Point::ORIGIN) <= 10.0 + 1.0 + 1e-12);
    }

    #[test]
    fn a_bevel_join_is_the_chord_and_a_round_join_is_the_arc() {
        let turn = core::f64::consts::FRAC_PI_2;
        let bevel = corner(turn).stroke(&StrokeStyle {
            join: Join::Bevel,
            ..style(2.0)
        });
        let round = corner(turn).stroke(&StrokeStyle {
            join: Join::Round,
            ..style(2.0)
        });
        // The bevel is the chord between the two offset points, which on the
        // bisector reaches only d*sin(45 deg); the round join is the arc, which
        // reaches exactly d.
        assert!((outer_reach(&bevel, turn) - 0.5f64.sqrt()).abs() < 1e-12);
        // The circle approximation is a cubic, good to 2.7e-4 of the radius.
        assert!((outer_reach(&round, turn) - 1.0).abs() < 3e-4);
        let outer: Vec<Point> = samples(&round)
            .into_iter()
            .filter(|s| s.x > 0.0 && s.y < 0.0)
            .collect();
        assert!(!outer.is_empty());
        for s in outer {
            assert!(
                (s.distance(Point::ORIGIN) - 1.0).abs() < 3e-4,
                "{s:?} is not on the join arc"
            );
        }
    }

    #[test]
    fn the_miter_limit_falls_back_to_bevel_at_the_threshold() {
        // The miter ratio is 1/sin(theta/2) for an included angle theta, so a
        // limit of exactly that ratio still miters and a hair less bevels.
        for included in [0.4, 1.0, core::f64::consts::FRAC_PI_2, 2.5] {
            let half = (included * 0.5).sin();
            let ratio = 1.0 / half;
            let turn = core::f64::consts::PI - included;
            let at = corner(turn).stroke(&StrokeStyle {
                miter_limit: ratio * (1.0 + 1e-9),
                ..style(2.0)
            });
            let under = corner(turn).stroke(&StrokeStyle {
                miter_limit: ratio * (1.0 - 1e-9),
                ..style(2.0)
            });
            assert!(
                (outer_reach(&at, turn) - ratio).abs() < 1e-9,
                "included {included}: miter should reach {ratio}"
            );
            assert!(
                (outer_reach(&under, turn) - half).abs() < 1e-9,
                "included {included}: past the limit it should bevel to {half}"
            );
        }
    }

    #[test]
    fn a_closed_path_joins_at_the_seam_rather_than_capping() {
        let square = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(10.0, 0.0)),
            PathEl::LineTo(p(10.0, 10.0)),
            PathEl::LineTo(p(0.0, 10.0)),
            PathEl::ClosePath,
        ]);
        let out = square.stroke(&style(2.0));
        // Miter joins all round, including at (0, 0): the outer contour is the
        // 12x12 square, not a 10x12 one with a square end stuck on.
        assert_eq!(out.bounding_box(), Rect::new(-1.0, -1.0, 11.0, 11.0));
        assert_eq!(out.subpaths().count(), 2);
        assert_eq!(winding(&out, p(0.0, 5.0)).abs(), 1);
        assert_eq!(winding(&out, p(5.0, 5.0)), 0);
        assert_eq!(winding(&out, p(-0.5, -0.5)).abs(), 1);
    }

    #[test]
    fn degenerate_segments_do_not_emit_spurious_joins() {
        // Two collinear segments, and the same two with a zero-length line and
        // a degenerate curve wedged between them. If either emitted a join the
        // outlines would differ; they must be the same path, element for
        // element.
        let clean = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(5.0, 0.0)),
            PathEl::LineTo(p(10.0, 0.0)),
        ]);
        let padded = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(5.0, 0.0)),
            PathEl::LineTo(p(5.0, 0.0)),
            PathEl::CurveTo(p(5.0, 0.0), p(5.0, 0.0), p(5.0, 0.0)),
            PathEl::LineTo(p(10.0, 0.0)),
        ]);
        for join in [Join::Miter, Join::Round, Join::Bevel] {
            let style = StrokeStyle { join, ..style(2.0) };
            assert_eq!(clean.stroke(&style), padded.stroke(&style), "{join:?}");
        }
        // And a real corner between the same degenerate pieces still joins:
        // dropping them must not drop the corner they surround.
        let corner = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(5.0, 0.0)),
            PathEl::LineTo(p(5.0, 0.0)),
            PathEl::LineTo(p(5.0, 5.0)),
        ]);
        assert_eq!(
            corner.stroke(&style(2.0)).bounding_box(),
            Rect::new(0.0, -1.0, 6.0, 5.0)
        );
    }

    // -- caps (#34) -------------------------------------------------------

    #[test]
    fn each_cap_matches_its_reference() {
        let l = line(p(0.0, 0.0), p(10.0, 0.0));
        let butt = l.stroke(&StrokeStyle {
            cap: Cap::Butt,
            ..style(2.0)
        });
        let square = l.stroke(&StrokeStyle {
            cap: Cap::Square,
            ..style(2.0)
        });
        let round = l.stroke(&StrokeStyle {
            cap: Cap::Round,
            ..style(2.0)
        });
        assert_eq!(butt.bounding_box(), Rect::new(0.0, -1.0, 10.0, 1.0));
        // Square adds half a width at each end; round adds the same box but
        // only touches it at one point.
        assert_eq!(square.bounding_box(), Rect::new(-1.0, -1.0, 11.0, 1.0));
        let rb = round.bounding_box();
        assert!(
            (rb.x0 + 1.0).abs() < 1e-9 && (rb.x1 - 11.0).abs() < 1e-9,
            "{rb:?}"
        );
        // The round cap is the half circle about the endpoint, to the 2.7e-4
        // a cubic approximates a circle to.
        for s in samples(&round).into_iter().filter(|s| s.x < 0.0) {
            assert!(
                (s.distance(Point::ORIGIN) - 1.0).abs() < 3e-4,
                "{s:?} is not on the cap"
            );
        }
        assert_eq!(winding(&square, p(-0.5, 0.9)).abs(), 1);
        assert_eq!(winding(&round, p(-0.5, 0.9)), 0);
        assert_eq!(winding(&round, p(-0.5, 0.0)).abs(), 1);
    }

    #[test]
    fn a_lone_moveto_is_a_dot_under_a_round_cap() {
        let path = Path::from(vec![PathEl::MoveTo(p(3.0, 4.0))]);
        let round = path.stroke(&StrokeStyle {
            cap: Cap::Round,
            ..style(2.0)
        });
        assert_eq!(round.bounding_box(), Rect::new(2.0, 3.0, 4.0, 5.0));
        assert_eq!(winding(&round, p(3.0, 4.0)).abs(), 1);
        assert_eq!(winding(&round, p(3.9, 4.9)), 0);
        for s in samples(&round) {
            assert!((s.distance(p(3.0, 4.0)) - 1.0).abs() < 3e-4, "{s:?}");
        }

        let square = path.stroke(&StrokeStyle {
            cap: Cap::Square,
            ..style(2.0)
        });
        assert_eq!(square.bounding_box(), Rect::new(2.0, 3.0, 4.0, 5.0));
        assert_eq!(winding(&square, p(3.9, 4.9)).abs(), 1);

        // A butt cap has no extent, so a zero-length subpath draws nothing.
        assert_eq!(path.stroke(&style(2.0)).elements().len(), 0);
    }

    // -- alignment (#36) --------------------------------------------------

    #[test]
    fn center_alignment_is_the_svg_default() {
        assert_eq!(StrokeStyle::default().align, Align::Center);
        let l = line(p(0.0, 0.0), p(10.0, 0.0));
        assert_eq!(
            l.stroke(&style(4.0)),
            l.stroke(&StrokeStyle {
                align: Align::Center,
                ..style(4.0)
            })
        );
    }

    #[test]
    fn inside_and_outside_clip_against_the_fill() {
        let square = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(10.0, 0.0)),
            PathEl::LineTo(p(10.0, 10.0)),
            PathEl::LineTo(p(0.0, 10.0)),
            PathEl::ClosePath,
        ]);
        for winding_order in [false, true] {
            let src = if winding_order {
                square.clone()
            } else {
                // The same square wound the other way: which side is out must
                // come from the geometry, not from the order of the points.
                Path::from(vec![
                    PathEl::MoveTo(p(0.0, 0.0)),
                    PathEl::LineTo(p(0.0, 10.0)),
                    PathEl::LineTo(p(10.0, 10.0)),
                    PathEl::LineTo(p(10.0, 0.0)),
                    PathEl::ClosePath,
                ])
            };
            let inside = src.stroke(&StrokeStyle {
                align: Align::Inside,
                ..style(2.0)
            });
            let outside = src.stroke(&StrokeStyle {
                align: Align::Outside,
                ..style(2.0)
            });
            // Inside stays within the fill and covers the band just in.
            assert_eq!(inside.bounding_box(), Rect::new(0.0, 0.0, 10.0, 10.0));
            assert_eq!(winding(&inside, p(1.0, 5.0)).abs(), 1);
            assert_eq!(winding(&inside, p(5.0, 5.0)), 0);
            assert_eq!(winding(&inside, p(-0.5, 5.0)), 0);
            // Outside stays out of it.
            assert_eq!(outside.bounding_box(), Rect::new(-2.0, -2.0, 12.0, 12.0));
            assert_eq!(winding(&outside, p(-1.0, 5.0)).abs(), 1);
            assert_eq!(winding(&outside, p(5.0, 5.0)), 0);
            assert_eq!(winding(&outside, p(1.0, 5.0)), 0);
        }
    }

    // -- everything at once -----------------------------------------------

    #[test]
    fn every_stroke_is_finite_and_closed() {
        let make = |r: &mut Rng| {
            let mut path = Path::new();
            for _ in 0..r.below(7) {
                path.push(match r.below(8) {
                    0 | 1 => PathEl::MoveTo(r.point()),
                    2 | 3 => PathEl::LineTo(r.point()),
                    4 => PathEl::QuadTo(r.point(), r.point()),
                    5 => PathEl::CurveTo(r.point(), r.point(), r.point()),
                    _ => PathEl::ClosePath,
                });
            }
            let s = StrokeStyle {
                width: r.unit() * 1e6,
                join: [Join::Miter, Join::Round, Join::Bevel][r.below(3) as usize],
                cap: [Cap::Butt, Cap::Round, Cap::Square][r.below(3) as usize],
                miter_limit: r.unit() * 8.0,
                align: [Align::Center, Align::Inside, Align::Outside][r.below(3) as usize],
                dash: Vec::new(),
                dash_offset: 0.0,
                tolerance: 1e-3,
            };
            (path, s)
        };
        check("stroke finite", 150, make, |(path, s)| {
            let out = path.stroke(s);
            out.elements().iter().all(|el| match *el {
                PathEl::MoveTo(q) | PathEl::LineTo(q) => q.is_finite(),
                PathEl::QuadTo(a, b) => a.is_finite() && b.is_finite(),
                PathEl::CurveTo(a, b, c) => a.is_finite() && b.is_finite() && c.is_finite(),
                PathEl::ClosePath => true,
            }) && out
                .subpaths()
                .all(|sub| sub.is_closed() || sub.elements().is_empty())
        });
    }

    #[test]
    fn a_stroked_circle_is_an_annulus() {
        // Four cubic quarters: the offset of each must stay concentric, and
        // the two contours must come out at the two radii.
        let k = 4.0 / 3.0 * core::f64::consts::FRAC_PI_8.tan();
        let mut els = vec![PathEl::MoveTo(p(1.0, 0.0))];
        let mut a = p(1.0, 0.0);
        for i in 0..4 {
            let (s0, c0) = (f64::from(i) * core::f64::consts::FRAC_PI_2).sin_cos();
            let (s1, c1) = (f64::from(i + 1) * core::f64::consts::FRAC_PI_2).sin_cos();
            let b = p(c1, s1);
            els.push(PathEl::CurveTo(
                a + Vec2::new(-s0, c0) * k,
                b - Vec2::new(-s1, c1) * k,
                b,
            ));
            a = b;
        }
        els.push(PathEl::ClosePath);
        let out = Path::from(els).stroke(&StrokeStyle {
            width: 0.5,
            join: Join::Round,
            ..style(0.5)
        });
        for s in samples(&out) {
            let r = s.to_vec2().length();
            assert!(
                (r - 1.25).abs() < 1e-3 || (r - 0.75).abs() < 1e-3,
                "radius {r} is on neither side of the annulus"
            );
        }
        assert_eq!(winding(&out, p(1.0, 0.0)).abs(), 1);
        assert_eq!(winding(&out, Point::ORIGIN), 0);
        assert_eq!(winding(&out, p(2.0, 0.0)), 0);
    }
}
