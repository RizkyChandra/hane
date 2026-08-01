//! The document the editor edits, and the one screen-to-document conversion
//! the whole interaction layer shares.
//!
//! `hane-scene` supplies the storage ([`Arena`]) and the broad phase
//! ([`Quadtree`]) but no object type, because nothing below P5 needed one.
//! [`Shape`] is the smallest one selection, hit testing and transforms need:
//! an outline, where it sits, and which of its fill and stroke are hittable.
//!
//! [`Document`] owns the arena and the index together so they cannot drift
//! apart. The quadtree locates an item by the bounding box it was inserted
//! with, so every write that moves a shape has to hand the *old* box back --
//! a caller holding the two separately gets that wrong once and then loses
//! objects from every query for the rest of the session.

use hane_geom::{Affine, CubicBez, QuadBez, Rect};
use hane_path::{Path, Segment};
use hane_scene::{Arena, NodeId, Quadtree, View};

/// A distance of `pixels` screen pixels, in document units under `view`.
///
/// Every screen-space quantity in this crate goes through here: the hit
/// tolerance, the transform handles' grab radius and the snap distance. They
/// are all specified in pixels because that is what the hand controls -- a
/// four-pixel grab radius is four pixels whatever the zoom -- and a document
/// unit is not, so storing any of them in document units makes handles
/// ungrabbable zoomed out and the whole canvas sticky zoomed in.
///
/// One function rather than three because they must agree: the marquee and
/// the hit test disagreeing by a factor of the zoom is exactly the bug that
/// only shows up at 0.1x.
///
/// A [`View`] is a rotation and a uniform scale, so the conversion is the one
/// scalar and not [`Affine::max_scale`] of the matrix -- same number, without
/// the square roots, and exact rather than off by a rounding step. The zoom is
/// clamped away from zero by `View` itself, which is what makes this total.
#[must_use]
pub fn document_length(view: &View, pixels: f64) -> f64 {
    pixels / view.zoom()
}

/// One drawable object: an outline, where it sits, and what of it is hittable.
#[derive(Clone, Debug)]
pub struct Shape {
    /// The outline, in the shape's own coordinates.
    pub path: Path,
    /// Shape space to document space.
    ///
    /// Transform edits rewrite this and never the path. Baking a transform
    /// into control points cannot be undone exactly -- `(p * T) * T.inverse()`
    /// is not `p` in f64 -- whereas replacing six coefficients with the six
    /// that were there before is exact by construction.
    pub transform: Affine,
    /// Whether the interior is painted, and so whether a click inside hits.
    pub filled: bool,
    /// Stroke width in shape units, or `None` when the outline is not painted.
    pub stroke_width: Option<f64>,
}

impl Shape {
    /// A filled, unstroked, untransformed shape.
    #[must_use]
    pub fn new(path: Path) -> Self {
        Self {
            path,
            transform: Affine::IDENTITY,
            filled: true,
            stroke_width: None,
        }
    }

    /// The tightest document-space box containing this shape, stroke included.
    ///
    /// Not `path.bounding_box().transform(t)`: that bounds the *box's* four
    /// transformed corners, which for a rotated shape is loose enough that a
    /// diagonal line's box is the whole square it spans. Transforming the
    /// control points first and bounding those keeps the curve bounds tight,
    /// which is what the quadtree needs to sort 100k objects usefully.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        let t = self.transform;
        let b = self.path.segments().fold(Rect::EMPTY, |acc, seg| {
            acc.union(transformed_bounds(seg, t))
        });
        match self.stroke_width {
            // `max_scale` overstates a non-uniform transform's effect on the
            // thin axis. Overstating a bounding box costs a few extra broad
            // phase candidates; understating it loses the object.
            Some(w) if w > 0.0 => b.inflate(0.5 * w * t.max_scale()),
            _ => b,
        }
    }
}

/// The tight bounds of `seg` after `t`, without allocating a transformed path.
fn transformed_bounds(seg: Segment, t: Affine) -> Rect {
    match seg {
        Segment::Line(a, b) => Rect::from_points(t * a, t * b),
        Segment::Quad(q) => QuadBez::new(t * q.p0, t * q.p1, t * q.p2).bounding_box(),
        Segment::Cubic(c) => CubicBez::new(t * c.p0, t * c.p1, t * c.p2, t * c.p3).bounding_box(),
    }
}

/// A shape plus what the document has to remember about it.
struct Entry {
    shape: Shape,
    /// The box the shape is currently indexed under. Cached because removing
    /// from the quadtree is a descent keyed on it, so it must be the box the
    /// insert used and not one recomputed from a shape that has since moved.
    bounds: Rect,
    /// Paint order. Higher is nearer the viewer.
    z: u32,
}

/// The shapes being edited, stored by id and indexed by bounding box.
pub struct Document {
    shapes: Arena<Entry>,
    index: Quadtree,
    /// The next paint order to hand out. Insertion order is z order: a newly
    /// drawn shape lands on top, which is what every drawing program does.
    next_z: u32,
}

impl Document {
    /// An empty document whose spatial index covers `bounds`.
    ///
    /// Shapes outside `bounds` still work; they are just not accelerated. See
    /// [`Quadtree::new`].
    #[must_use]
    pub fn new(bounds: Rect) -> Self {
        Self {
            shapes: Arena::new(),
            index: Quadtree::new(bounds),
            next_z: 0,
        }
    }

    /// Adds `shape` on top of everything already there, returning its id.
    pub fn insert(&mut self, shape: Shape) -> NodeId {
        let bounds = shape.bounds();
        let z = self.next_z;
        // Saturating rather than wrapping: wrapping would put the newest shape
        // at the bottom of the paint order, which reads as the shape silently
        // vanishing behind the others. Four billion inserts in one session is
        // not reachable, but a wrong answer there is worse than a tie.
        self.next_z = self.next_z.saturating_add(1);
        let id = self.shapes.insert(Entry { shape, bounds, z });
        self.index.insert(id.to_bits(), bounds);
        id
    }

    /// Removes the shape `id` names, returning it, or `None` if `id` is stale.
    pub fn remove(&mut self, id: NodeId) -> Option<Shape> {
        let entry = self.shapes.remove(id)?;
        self.index.remove(id.to_bits(), entry.bounds);
        Some(entry.shape)
    }

    /// The shape `id` names, or `None` if it has been removed.
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Shape> {
        self.shapes.get(id).map(|e| &e.shape)
    }

    /// The document-space bounds of the shape `id` names.
    #[must_use]
    pub fn bounds(&self, id: NodeId) -> Option<Rect> {
        self.shapes.get(id).map(|e| e.bounds)
    }

    /// Replaces the shape's placement, returning the transform it had.
    ///
    /// Returning the old value is what makes an undoable transform exact: the
    /// command stores these six coefficients and puts them back verbatim.
    pub fn set_transform(&mut self, id: NodeId, transform: Affine) -> Option<Affine> {
        let entry = self.shapes.get_mut(id)?;
        let previous = entry.shape.transform;
        entry.shape.transform = transform;
        let old = entry.bounds;
        entry.bounds = entry.shape.bounds();
        let new = entry.bounds;
        self.index.update(id.to_bits(), old, new);
        Some(previous)
    }

    /// Replaces the shape's outline, returning the path it had.
    ///
    /// The counterpart of [`set_transform`](Document::set_transform) for the
    /// node tool, and returning the old value for the same reason: the command
    /// that undoes an edit puts these elements back verbatim rather than
    /// recomputing them from the edit, which f64 cannot do exactly.
    pub fn set_path(&mut self, id: NodeId, path: Path) -> Option<Path> {
        let entry = self.shapes.get_mut(id)?;
        let previous = core::mem::replace(&mut entry.shape.path, path);
        let old = entry.bounds;
        entry.bounds = entry.shape.bounds();
        let new = entry.bounds;
        self.index.update(id.to_bits(), old, new);
        Some(previous)
    }

    /// Every shape in paint order, furthest from the viewer first.
    ///
    /// This is the order a renderer paints in and a serialiser writes out, and
    /// it is total: no two shapes share a `z`, so it is the same order twice
    /// running and the same after a save and a load.
    #[must_use]
    pub fn z_order(&self) -> Vec<NodeId> {
        let mut ids: Vec<(u32, NodeId)> = self.shapes.iter().map(|(id, e)| (e.z, id)).collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Sets the paint order of one shape, returning the one it had.
    pub(crate) fn set_z(&mut self, id: NodeId, z: u32) -> Option<u32> {
        let entry = self.shapes.get_mut(id)?;
        let previous = core::mem::replace(&mut entry.z, z);
        // Keeps the next insertion on top of everything, including a shape
        // just sent to the front.
        self.next_z = self.next_z.max(z.saturating_add(1));
        Some(previous)
    }

    /// The number of shapes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    /// Whether the document holds no shapes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// Appends the raw ids of every shape whose bounds overlap `area`.
    ///
    /// Raw `u32`s and an out-parameter because that is what [`Quadtree::query`]
    /// speaks, and the callers here run per pointer move: reusing one scratch
    /// buffer keeps the hit test and the snapper allocation-free.
    pub(crate) fn query(&self, area: Rect, out: &mut Vec<u32>) {
        self.index.query(area, out);
    }

    /// Paint order of the shape `id` names; higher is nearer the viewer.
    pub(crate) fn z(&self, id: NodeId) -> u32 {
        self.shapes.get(id).map_or(0, |e| e.z)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hane_geom::{PathEl, Point};

    /// An axis-aligned rectangle as a closed path, the test shape everywhere
    /// in this crate.
    pub(crate) fn rect_path(r: Rect) -> Path {
        Path::from(vec![
            PathEl::MoveTo(Point::new(r.x0, r.y0)),
            PathEl::LineTo(Point::new(r.x1, r.y0)),
            PathEl::LineTo(Point::new(r.x1, r.y1)),
            PathEl::LineTo(Point::new(r.x0, r.y1)),
            PathEl::ClosePath,
        ])
    }

    pub(crate) fn rect_shape(r: Rect) -> Shape {
        Shape::new(rect_path(r))
    }

    /// A document with `n` unit squares on a grid, for the 100k gates.
    pub(crate) fn grid_document(n: u32) -> Document {
        let side = (f64::from(n).sqrt().ceil()) as u32;
        let extent = f64::from(side) * 10.0;
        let mut doc = Document::new(Rect::new(-10.0, -10.0, extent, extent));
        for i in 0..n {
            let (x, y) = (f64::from(i % side) * 10.0, f64::from(i / side) * 10.0);
            doc.insert(rect_shape(Rect::new(x, y, x + 4.0, y + 4.0)));
        }
        doc
    }

    #[test]
    fn a_pixel_is_a_pixel_at_every_zoom() {
        let mut view = View::new();
        assert_eq!(document_length(&view, 8.0), 8.0);
        view.zoom_about(Point::ORIGIN, 4.0);
        assert_eq!(document_length(&view, 8.0), 2.0);
        view.zoom_about(Point::ORIGIN, 1.0 / 400.0);
        assert_eq!(document_length(&view, 8.0), 800.0);
        // Rotation is not a scale, so it must not change the answer.
        let before = document_length(&view, 8.0);
        view.rotate_about(Point::new(13.0, 7.0), 0.9);
        assert_eq!(document_length(&view, 8.0), before);
    }

    #[test]
    fn a_rotated_shape_keeps_tight_bounds() {
        // A 45-degree diagonal: the loose bound (transform the box, not the
        // points) would be the whole 2x2 square rather than the 1.414-wide
        // band the line actually occupies.
        let mut s = Shape::new(Path::from(vec![
            PathEl::MoveTo(Point::new(-1.0, 0.0)),
            PathEl::LineTo(Point::new(1.0, 0.0)),
        ]));
        s.transform = Affine::rotate(core::f64::consts::FRAC_PI_4);
        let b = s.bounds();
        let half = core::f64::consts::SQRT_2 / 2.0;
        assert!(
            (b.x1 - half).abs() < 1e-12 && (b.y1 - half).abs() < 1e-12,
            "{b:?}"
        );
    }

    #[test]
    fn a_stroke_widens_the_bounds_and_the_index_agrees() {
        let mut doc = Document::new(Rect::new(-100.0, -100.0, 100.0, 100.0));
        let mut s = rect_shape(Rect::new(0.0, 0.0, 10.0, 10.0));
        s.stroke_width = Some(4.0);
        let id = doc.insert(s);
        assert_eq!(doc.bounds(id), Some(Rect::new(-2.0, -2.0, 12.0, 12.0)));
        let mut out = Vec::new();
        // A box that touches only the stroke's outer edge must still find it.
        doc.query(Rect::new(-3.0, -3.0, -1.0, -1.0), &mut out);
        assert_eq!(out, vec![id.to_bits()]);
    }

    #[test]
    fn moving_a_shape_reindexes_it() {
        let mut doc = Document::new(Rect::new(-1000.0, -1000.0, 1000.0, 1000.0));
        let id = doc.insert(rect_shape(Rect::new(0.0, 0.0, 4.0, 4.0)));
        let previous = doc.set_transform(id, Affine::translate(hane_geom::Vec2::new(500.0, 500.0)));
        assert_eq!(previous, Some(Affine::IDENTITY));

        let mut out = Vec::new();
        doc.query(Rect::new(-1.0, -1.0, 5.0, 5.0), &mut out);
        assert!(out.is_empty(), "the old box is still in the index");
        doc.query(Rect::new(499.0, 499.0, 505.0, 505.0), &mut out);
        assert_eq!(out, vec![id.to_bits()]);

        // And removal has to use the box it currently occupies, not the one it
        // was inserted with.
        assert!(doc.remove(id).is_some());
        out.clear();
        doc.query(Rect::new(499.0, 499.0, 505.0, 505.0), &mut out);
        assert!(out.is_empty());
        assert!(doc.is_empty());
    }

    #[test]
    fn insertion_order_is_paint_order() {
        let mut doc = Document::new(Rect::new(0.0, 0.0, 100.0, 100.0));
        let bottom = doc.insert(rect_shape(Rect::new(0.0, 0.0, 10.0, 10.0)));
        let top = doc.insert(rect_shape(Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert!(doc.z(top) > doc.z(bottom));
    }
}
