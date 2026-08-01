//! Hit testing and marquee selection: quadtree broad phase, exact test after.
//!
//! Both start with a [`Quadtree`](hane_scene::Quadtree) query, never a scan
//! over the document. At 100k objects the difference is not a constant factor:
//! a scan touches every path in the file for every pointer move, and the exact
//! tests below cost microseconds each.
//!
//! # Where the tolerance lives
//!
//! The pointer tolerance arrives in screen pixels and is converted once, by
//! [`document_length`], before anything spatial happens. It is then in document
//! units for the query box, the stroke test and the flattening tolerance
//! alike, so all three shrink together as the view zooms in.
//!
//! # What "inside" means
//!
//! The fill test is the nonzero winding rule, matching
//! `hane_raster`'s fill (D-002): what the rasteriser paints is what the pointer
//! hits, or a shape can be visible and unclickable. It is evaluated on a
//! flattened outline, so the answer can differ from the true curve within the
//! flattening tolerance -- a quarter of the pointer tolerance, i.e. a fraction
//! of a pixel, well inside the slop the pointer already has.

use crate::document::{Document, Shape, document_length};
use hane_geom::{Affine, Point, Rect};
use hane_path::{Path, Segment};
use hane_scene::{NodeId, View};

/// What counts as inside the rubber band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarqueeMode {
    /// Only shapes lying wholly within the band.
    Contain,
    /// Every shape the band touches.
    Intersect,
}

/// The topmost shape under `screen`, or `None` for empty canvas.
///
/// `tolerance_px` is the pointer's slop in screen pixels: a click that far
/// from a stroke still counts, which is what makes a hairline grabbable.
///
/// Overlapping shapes resolve by paint order, nearest the viewer first, so the
/// one the user can see is the one they get.
#[must_use]
pub fn hit_test(doc: &Document, view: &View, screen: Point, tolerance_px: f64) -> Option<NodeId> {
    let point = view.to_document(screen);
    let tolerance = document_length(view, tolerance_px).max(0.0);
    let mut candidates = Vec::new();
    broad_phase(doc, point, tolerance, &mut candidates);
    // Descending paint order, then the first exact hit is the answer and the
    // shapes underneath are never tested at all.
    candidates.sort_unstable_by_key(|&id| std::cmp::Reverse(doc.z(id)));
    let mut scratch = Vec::new();
    candidates.into_iter().find(|&id| {
        doc.get(id)
            .is_some_and(|s| hits(s, point, tolerance, &mut scratch))
    })
}

/// Broad phase for a point: everything whose bounds come within `tolerance`.
fn broad_phase(doc: &Document, point: Point, tolerance: f64, out: &mut Vec<NodeId>) {
    // `Rect::overlaps` is strict on every edge, so a zero-area query box
    // matches nothing whatsoever -- pad by at least an ulp of the coordinates
    // so that a zero tolerance still finds the shape under the cursor.
    let pad = tolerance.max(f64::EPSILON * (1.0 + point.x.abs().max(point.y.abs())));
    let area = Rect::new(point.x, point.y, point.x, point.y).inflate(pad);
    let mut raw = Vec::new();
    doc.query(area, &mut raw);
    out.extend(raw.into_iter().map(NodeId::from_bits));
}

/// Whether `point` (document space) is on `shape`, within `tolerance`.
fn hits(shape: &Shape, point: Point, tolerance: f64, scratch: &mut Vec<Point>) -> bool {
    // A singular transform collapses the shape to a line or a point: nothing
    // is painted, so nothing is hit, and the inverse below would not exist.
    let Some(inverse) = shape.transform.inverse() else {
        return false;
    };
    let local = inverse * point;
    let scale = shape.transform.max_scale();

    if shape.filled {
        // Flattening tolerance in *shape* units, a quarter of the pointer's so
        // the approximation is not what decides a borderline click. The floor
        // keeps a zero pointer tolerance from asking for infinite subdivision;
        // it is relative to the shape's own size because an absolute one is
        // below an ulp for a shape drawn at 1e9.
        let size = shape.path.bounding_box().size().length();
        let flatten_tolerance = (0.25 * tolerance / scale).max(size * 1e-6);
        if winding(&shape.path, local, flatten_tolerance, scratch) != 0 {
            return true;
        }
    }

    match shape.stroke_width {
        // Half the stroke is how far its centre line is from its edge.
        // `max_scale` is exact for the conformal transforms a design tool
        // produces and generous on the thin axis of a squashed one; a stroke
        // that is a shade easier to grab is the right way to be wrong.
        Some(width) => {
            nearest_distance(&shape.path, shape.transform, local, point)
                <= tolerance + 0.5 * width * scale
        }
        None => false,
    }
}

/// The nonzero winding number of `path` about `p`, both in shape space.
///
/// Every subpath is treated as closed, open or not, because that is what
/// filling one does.
fn winding(path: &Path, p: Point, tolerance: f64, scratch: &mut Vec<Point>) -> i32 {
    let mut total = 0;
    for subpath in path.subpaths() {
        let mut previous = None;
        let mut first = Point::ORIGIN;
        for segment in subpath.segments() {
            scratch.clear();
            match segment {
                Segment::Line(a, b) => {
                    scratch.push(a);
                    scratch.push(b);
                }
                Segment::Quad(q) => q.flatten(tolerance, scratch),
                Segment::Cubic(c) => c.flatten(tolerance, scratch),
            }
            for &point in scratch.iter() {
                match previous {
                    None => first = point,
                    Some(a) => total += crossing(a, point, p),
                }
                previous = Some(point);
            }
        }
        if let Some(last) = previous {
            total += crossing(last, first, p);
        }
    }
    total
}

/// The contribution of edge `a`-`b` to the winding number about `p`.
///
/// The half-open comparison (`<=` on both ends, then negated) is what stops a
/// vertex lying exactly on the ray being counted twice by the two edges that
/// meet there -- the classic off-by-one that makes a filled shape have a
/// one-pixel unclickable seam at every horizontal tangent.
#[inline]
fn crossing(a: Point, b: Point, p: Point) -> i32 {
    if (a.y <= p.y) == (b.y <= p.y) {
        return 0;
    }
    let t = (p.y - a.y) / (b.y - a.y);
    if a.x + t * (b.x - a.x) > p.x {
        if b.y > a.y { 1 } else { -1 }
    } else {
        0
    }
}

/// Distance from `point` (document space) to the outline of `path`, where
/// `local` is `point` mapped into shape space by the inverse of `transform`.
///
/// The nearest parameter is found in shape space, where the curve solvers
/// live, and the distance is then measured in *document* space on the mapped
/// point. Measuring in shape space instead would report a distance scaled by
/// whatever the transform does, so a shape scaled 100x would need a click a
/// hundred times closer than the tolerance says.
///
/// ponytail: under a non-uniform transform the parameter minimising distance
/// in shape space is not quite the one minimising it in document space, so the
/// result is a hair too large for a heavily squashed shape. Sample the mapped
/// curve directly if anisotropic scaling ever becomes common.
fn nearest_distance(path: &Path, transform: Affine, local: Point, point: Point) -> f64 {
    let mut best = f64::INFINITY;
    for segment in path.segments() {
        let nearest = match segment {
            Segment::Line(a, b) => nearest_on_line(a, b, local),
            Segment::Quad(q) => q.eval(q.nearest(local).0),
            Segment::Cubic(c) => c.eval(c.nearest(local).0),
        };
        best = best.min((transform * nearest).distance(point));
    }
    best
}

/// The point of segment `a`-`b` nearest `p`.
#[inline]
fn nearest_on_line(a: Point, b: Point, p: Point) -> Point {
    let ab = b - a;
    let length2 = ab.dot(ab);
    if length2 <= 0.0 {
        return a;
    }
    a + ab * ((p - a).dot(ab) / length2).clamp(0.0, 1.0)
}

/// Every shape the rubber band from `a` to `b` selects, in paint order.
///
/// The corners are screen points and the test is made in screen space, because
/// that is the rectangle the user drew. Under a rotated view the band is not
/// axis-aligned in the document at all, and testing against its document-space
/// bounding box would select shapes outside the visible band -- so the
/// document-space box is used only as the broad phase, where being generous is
/// free.
///
/// ponytail: the exact test compares bounding boxes, so a marquee clipping the
/// empty corner of an L-shape's box selects it. That is what Figma and
/// Illustrator both do; swap in a path-versus-rectangle test if a user ever
/// complains, which needs P6's boolean machinery to be worth writing.
#[must_use]
pub fn marquee(doc: &Document, view: &View, a: Point, b: Point, mode: MarqueeMode) -> Vec<NodeId> {
    let band = Rect::from_points(a, b);
    let corners = [
        Point::new(band.x0, band.y0),
        Point::new(band.x1, band.y0),
        Point::new(band.x0, band.y1),
        Point::new(band.x1, band.y1),
    ];
    let area = corners
        .iter()
        .fold(Rect::EMPTY, |r, &c| r.union_point(view.to_document(c)));

    let mut raw = Vec::new();
    doc.query(area, &mut raw);
    let mut hits: Vec<NodeId> = raw
        .into_iter()
        .map(NodeId::from_bits)
        .filter(|&id| {
            doc.bounds(id).is_some_and(|bounds| {
                let screen = screen_bounds(bounds, view);
                match mode {
                    MarqueeMode::Contain => band.contains_rect(screen),
                    MarqueeMode::Intersect => band.overlaps(screen),
                }
            })
        })
        .collect();
    // Paint order, so the caller can rely on the last one being on top and two
    // runs of the same marquee agree.
    hits.sort_unstable_by_key(|&id| doc.z(id));
    hits
}

/// The screen-space bounding box of a document-space one.
fn screen_bounds(bounds: Rect, view: &View) -> Rect {
    // `Rect::transform` bounds the four transformed corners, which is exactly
    // right here: a rotated view turns an axis-aligned document box into a
    // diamond, and the band test wants the extent that diamond covers.
    bounds.transform(view.matrix())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::{grid_document, rect_path, rect_shape};
    use hane_geom::{PathEl, Vec2};
    use std::time::Instant;

    /// Debug builds run these an order of magnitude slower than the release
    /// build the budget in the issue is about, and CI runs the debug one.
    const SLOWDOWN: f64 = if cfg!(debug_assertions) { 25.0 } else { 1.0 };

    fn document() -> Document {
        Document::new(Rect::new(-1000.0, -1000.0, 1000.0, 1000.0))
    }

    #[test]
    fn a_click_inside_a_fill_selects_it_and_outside_does_not() {
        let mut doc = document();
        let id = doc.insert(rect_shape(Rect::new(0.0, 0.0, 10.0, 10.0)));
        let view = View::new();
        assert_eq!(hit_test(&doc, &view, Point::new(5.0, 5.0), 4.0), Some(id));
        assert_eq!(hit_test(&doc, &view, Point::new(0.5, 9.5), 4.0), Some(id));
        // Far outside, past the tolerance.
        assert_eq!(hit_test(&doc, &view, Point::new(-20.0, 5.0), 4.0), None);
        assert_eq!(hit_test(&doc, &view, Point::new(5.0, 40.0), 4.0), None);
    }

    #[test]
    fn a_hole_is_not_inside() {
        // A square with a counter-wound square inside it: the middle winds to
        // zero and must not be hittable.
        let mut path = rect_path(Rect::new(0.0, 0.0, 30.0, 30.0));
        for el in [
            PathEl::MoveTo(Point::new(10.0, 10.0)),
            PathEl::LineTo(Point::new(10.0, 20.0)),
            PathEl::LineTo(Point::new(20.0, 20.0)),
            PathEl::LineTo(Point::new(20.0, 10.0)),
            PathEl::ClosePath,
        ] {
            path.push(el);
        }
        let mut doc = document();
        let id = doc.insert(Shape::new(path));
        let view = View::new();
        assert_eq!(hit_test(&doc, &view, Point::new(5.0, 15.0), 0.0), Some(id));
        assert_eq!(hit_test(&doc, &view, Point::new(15.0, 15.0), 0.0), None);
    }

    #[test]
    fn a_curve_is_hit_where_it_bulges() {
        // A single cubic bulging right, closed back along the y axis. The
        // bulge peaks at x = 7.5, so x = 6 is inside and x = 9 is not.
        let path = Path::from(vec![
            PathEl::MoveTo(Point::new(0.0, 0.0)),
            PathEl::CurveTo(
                Point::new(10.0, 0.0),
                Point::new(10.0, 10.0),
                Point::new(0.0, 10.0),
            ),
            PathEl::ClosePath,
        ]);
        let mut doc = document();
        let id = doc.insert(Shape::new(path));
        let view = View::new();
        assert_eq!(hit_test(&doc, &view, Point::new(6.0, 5.0), 0.0), Some(id));
        assert_eq!(hit_test(&doc, &view, Point::new(9.0, 5.0), 0.0), None);
    }

    #[test]
    fn a_click_on_a_stroke_selects_an_unfilled_shape() {
        let mut doc = document();
        let mut shape = rect_shape(Rect::new(0.0, 0.0, 40.0, 40.0));
        shape.filled = false;
        shape.stroke_width = Some(2.0);
        let id = doc.insert(shape);
        let view = View::new();
        // The middle is empty; the edge is not.
        assert_eq!(hit_test(&doc, &view, Point::new(20.0, 20.0), 1.0), None);
        assert_eq!(hit_test(&doc, &view, Point::new(20.0, 0.5), 0.0), Some(id));
        // Just outside the stroke, but inside the pointer tolerance.
        assert_eq!(hit_test(&doc, &view, Point::new(20.0, -3.0), 3.0), Some(id));
        assert_eq!(hit_test(&doc, &view, Point::new(20.0, -3.0), 0.5), None);
    }

    #[test]
    fn the_tolerance_is_screen_pixels_at_every_zoom() {
        let mut doc = document();
        let mut shape = rect_shape(Rect::new(0.0, 0.0, 40.0, 40.0));
        shape.filled = false;
        shape.stroke_width = Some(0.01);
        let id = doc.insert(shape);

        // Four pixels of slop, at three zooms, always four pixels away from
        // the stroke on screen. A tolerance stored in document units would
        // pass at 1x and fail at both of the others.
        for factor in [1.0, 40.0, 0.02] {
            let mut view = View::new();
            view.zoom_about(Point::ORIGIN, factor);
            let on_edge = view.to_screen(Point::new(20.0, 0.0));
            let inside = on_edge + Vec2::new(0.0, 3.0);
            let outside = on_edge + Vec2::new(0.0, 6.0);
            assert_eq!(hit_test(&doc, &view, inside, 4.0), Some(id), "{factor}");
            assert_eq!(hit_test(&doc, &view, outside, 4.0), None, "{factor}");
        }
    }

    #[test]
    fn the_topmost_shape_wins() {
        let mut doc = document();
        let bottom = doc.insert(rect_shape(Rect::new(0.0, 0.0, 20.0, 20.0)));
        let top = doc.insert(rect_shape(Rect::new(10.0, 10.0, 30.0, 30.0)));
        let view = View::new();
        assert_eq!(
            hit_test(&doc, &view, Point::new(15.0, 15.0), 0.0),
            Some(top)
        );
        assert_eq!(
            hit_test(&doc, &view, Point::new(5.0, 5.0), 0.0),
            Some(bottom)
        );
        assert_eq!(
            hit_test(&doc, &view, Point::new(25.0, 25.0), 0.0),
            Some(top)
        );
    }

    #[test]
    fn a_transformed_shape_is_hit_where_it_is_drawn() {
        let mut doc = document();
        let mut shape = rect_shape(Rect::new(-5.0, -5.0, 5.0, 5.0));
        // Rotated 45 degrees and moved: the corner of the unrotated square is
        // now empty space and the middle of its edge is not.
        shape.transform = Affine::translate(Vec2::new(100.0, 100.0))
            * Affine::rotate(core::f64::consts::FRAC_PI_4);
        let id = doc.insert(shape);
        let view = View::new();
        assert_eq!(
            hit_test(&doc, &view, Point::new(100.0, 106.0), 0.0),
            Some(id)
        );
        assert_eq!(hit_test(&doc, &view, Point::new(104.5, 104.5), 0.0), None);
    }

    #[test]
    fn marquee_contain_takes_only_what_it_encloses() {
        let mut doc = document();
        let inside = doc.insert(rect_shape(Rect::new(10.0, 10.0, 20.0, 20.0)));
        let straddling = doc.insert(rect_shape(Rect::new(25.0, 10.0, 60.0, 20.0)));
        let away = doc.insert(rect_shape(Rect::new(200.0, 200.0, 210.0, 210.0)));
        let view = View::new();
        let band = (Point::new(0.0, 0.0), Point::new(30.0, 30.0));

        let contained = marquee(&doc, &view, band.0, band.1, MarqueeMode::Contain);
        assert_eq!(contained, vec![inside]);
        let touched = marquee(&doc, &view, band.0, band.1, MarqueeMode::Intersect);
        assert_eq!(touched, vec![inside, straddling]);
        assert!(!touched.contains(&away));
    }

    #[test]
    fn a_marquee_drawn_backwards_is_the_same_marquee() {
        let mut doc = document();
        let id = doc.insert(rect_shape(Rect::new(10.0, 10.0, 20.0, 20.0)));
        let view = View::new();
        let forwards = marquee(
            &doc,
            &view,
            Point::new(0.0, 0.0),
            Point::new(30.0, 30.0),
            MarqueeMode::Contain,
        );
        let backwards = marquee(
            &doc,
            &view,
            Point::new(30.0, 30.0),
            Point::new(0.0, 0.0),
            MarqueeMode::Contain,
        );
        assert_eq!(forwards, vec![id]);
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn a_rotated_view_bands_what_is_on_screen_not_what_is_axis_aligned() {
        let mut doc = document();
        // Two shapes far apart along the document diagonal. With the view
        // turned 45 degrees the band is a diamond in the document, and testing
        // against its document-space bounding box would take both.
        let near = doc.insert(rect_shape(Rect::new(-2.0, -2.0, 2.0, 2.0)));
        doc.insert(rect_shape(Rect::new(58.0, 58.0, 62.0, 62.0)));
        let mut view = View::new();
        view.rotate_about(Point::ORIGIN, core::f64::consts::FRAC_PI_4);

        let corner = view.to_screen(Point::new(0.0, 0.0));
        let band = marquee(
            &doc,
            &view,
            corner + Vec2::new(-10.0, -10.0),
            corner + Vec2::new(10.0, 10.0),
            MarqueeMode::Intersect,
        );
        assert_eq!(band, vec![near]);
    }

    #[test]
    fn hit_testing_a_hundred_thousand_shapes_stays_under_a_millisecond() {
        let doc = grid_document(100_000);
        assert_eq!(doc.len(), 100_000);
        let view = View::new();

        // Spread over the whole grid so the measurement is not one warm
        // quadtree node answering every time.
        let clicks: Vec<Point> = (0..1000)
            .map(|i| {
                let side = 317.0;
                let k = f64::from(i) * 97.0;
                Point::new((k % side) * 10.0 + 2.0, (k / side).floor() * 10.0 + 2.0)
            })
            .collect();

        let start = Instant::now();
        let mut found = 0;
        for &click in &clicks {
            if hit_test(&doc, &view, click, 4.0).is_some() {
                found += 1;
            }
        }
        let per_click = start.elapsed().as_secs_f64() * 1000.0 / clicks.len() as f64;
        assert!(found > 0, "the clicks all missed, so nothing was measured");
        assert!(
            per_click < SLOWDOWN,
            "{per_click:.4} ms per hit test over 100k shapes"
        );
    }

    #[test]
    fn a_marquee_over_a_hundred_thousand_shapes_stays_interactive() {
        let doc = grid_document(100_000);
        let view = View::new();
        let corner = Point::new(1000.0, 1000.0);

        // A band covering everything is the worst case: the broad phase cannot
        // reject anything and every shape reaches the exact test.
        let start = Instant::now();
        let all = marquee(
            &doc,
            &view,
            Point::new(-100.0, -100.0),
            Point::new(1e6, 1e6),
            MarqueeMode::Intersect,
        );
        let whole = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(all.len(), 100_000);

        // The common case is a small band, which the quadtree makes cheap.
        let start = Instant::now();
        let some = marquee(
            &doc,
            &view,
            corner,
            corner + Vec2::new(100.0, 100.0),
            MarqueeMode::Contain,
        );
        let small = start.elapsed().as_secs_f64() * 1000.0;
        assert!(!some.is_empty());
        assert!(
            small < SLOWDOWN,
            "{small:.4} ms for a small marquee over 100k shapes"
        );
        assert!(
            whole < 400.0 * SLOWDOWN,
            "{whole:.4} ms for a marquee over all 100k shapes"
        );
    }
}
