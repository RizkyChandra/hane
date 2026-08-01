//! The transform box: scale, rotate and skew handles around the selection,
//! and the undoable command they drive.
//!
//! # Absolute, never incremental
//!
//! A gesture is a single [`Affine`] from where the drag started, applied to the
//! transforms the shapes had *when the drag began*. It is never composed onto
//! whatever they have now. Two reasons, one per issue this closes:
//!
//! * Multi-selection. One affine applied to every shape's starting placement
//!   is what "preserves relative positions" means; per-shape deltas drift apart
//!   within a few hundred frames of a rotation.
//! * Undo. [`Transform`] stores the six coefficients each shape had before the
//!   gesture and puts them back verbatim, which is exact. A command holding a
//!   delta cannot be: `(p * T) * T.inverse()` is not `p` in f64 once the
//!   magnitudes differ, and the undo fuzz property fails it at large
//!   coordinates. Storing the previous value also makes coalescing trivial --
//!   keep the group's `before`, take the newest `targets`.
//!
//! # Handles are screen-sized
//!
//! Grab radii go through [`document_length`], so a handle is the same number of
//! pixels across at every zoom. Stored in document units they become
//! unreachable zoomed out and swallow the whole shape zoomed in.

use crate::document::{Document, document_length};
use crate::select::Selection;
use crate::undo::Command;
use hane_geom::{Affine, Point, Rect, Vec2};
use hane_scene::{NodeId, View};

/// A grab point on the transform box.
///
/// The eight around the edge scale; [`Handle::Pivot`] moves the point rotation
/// turns about. Compass names because the box is axis-aligned in document
/// space and screen y grows downwards, so north is the top edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    /// Top-left corner.
    NorthWest,
    /// Middle of the top edge.
    North,
    /// Top-right corner.
    NorthEast,
    /// Middle of the right edge.
    East,
    /// Bottom-right corner.
    SouthEast,
    /// Middle of the bottom edge.
    South,
    /// Bottom-left corner.
    SouthWest,
    /// Middle of the left edge.
    West,
    /// The rotation pivot marker.
    Pivot,
}

/// The eight scale handles, clockwise from the top-left.
const SCALE_HANDLES: [Handle; 8] = [
    Handle::NorthWest,
    Handle::North,
    Handle::NorthEast,
    Handle::East,
    Handle::SouthEast,
    Handle::South,
    Handle::SouthWest,
    Handle::West,
];

impl Handle {
    /// Where this handle sits on `bounds`, in document space.
    fn position(self, bounds: Rect, pivot: Point) -> Point {
        let c = bounds.center();
        match self {
            Self::NorthWest => Point::new(bounds.x0, bounds.y0),
            Self::North => Point::new(c.x, bounds.y0),
            Self::NorthEast => Point::new(bounds.x1, bounds.y0),
            Self::East => Point::new(bounds.x1, c.y),
            Self::SouthEast => Point::new(bounds.x1, bounds.y1),
            Self::South => Point::new(c.x, bounds.y1),
            Self::SouthWest => Point::new(bounds.x0, bounds.y1),
            Self::West => Point::new(bounds.x0, c.y),
            Self::Pivot => pivot,
        }
    }

    /// The handle across the box from this one, which a scale holds still.
    fn opposite(self) -> Self {
        match self {
            Self::NorthWest => Self::SouthEast,
            Self::North => Self::South,
            Self::NorthEast => Self::SouthWest,
            Self::East => Self::West,
            Self::SouthEast => Self::NorthWest,
            Self::South => Self::North,
            Self::SouthWest => Self::NorthEast,
            Self::West => Self::East,
            Self::Pivot => Self::Pivot,
        }
    }
}

/// The box drawn around the selection, and the gestures its handles produce.
///
/// Document space throughout. The view is only consulted where a screen-space
/// quantity is involved, which is exactly [`TransformBox::handle_at`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformBox {
    /// The document-space box around the selection.
    pub bounds: Rect,
    /// The point rotation and skew turn about. Starts at the centre and can be
    /// dragged anywhere, inside the box or out.
    pub pivot: Point,
}

impl TransformBox {
    /// The box around `selection`, or `None` when nothing live is selected.
    #[must_use]
    pub fn new(doc: &Document, selection: &Selection) -> Option<Self> {
        let bounds = selection.bounds(doc);
        if bounds.is_empty() {
            return None;
        }
        Some(Self {
            bounds,
            pivot: bounds.center(),
        })
    }

    /// Refreshes the box from the document, keeping the pivot where the user
    /// put it.
    ///
    /// This is what makes the box track the selection under every transform:
    /// call it after each edit and the box is recomputed from the shapes'
    /// current bounds rather than transformed alongside them, so it can never
    /// drift out of agreement with what is drawn.
    pub fn track(&mut self, doc: &Document, selection: &Selection) {
        let bounds = selection.bounds(doc);
        if !bounds.is_empty() {
            self.bounds = bounds;
        }
    }

    /// Moves the rotation pivot.
    pub fn set_pivot(&mut self, pivot: Point) {
        self.pivot = pivot;
    }

    /// The handle under `screen`, or `None`.
    ///
    /// `size_px` is the handle's full width in screen pixels; a click within
    /// half of that grabs it. The pivot wins ties because it is drawn on top.
    #[must_use]
    pub fn handle_at(&self, view: &View, screen: Point, size_px: f64) -> Option<Handle> {
        let point = view.to_document(screen);
        let reach = document_length(view, size_px) * 0.5;
        let mut best: Option<(f64, Handle)> = None;
        for handle in [Handle::Pivot].into_iter().chain(SCALE_HANDLES) {
            let d = handle.position(self.bounds, self.pivot).distance(point);
            // Strictly nearer, so the pivot keeps the tie it starts with.
            if d <= reach && best.is_none_or(|(best_d, _)| d < best_d) {
                best = Some((d, handle));
            }
        }
        best.map(|(_, handle)| handle)
    }

    /// The gesture that drags `handle` to `cursor`, both in document space.
    ///
    /// `from_center` scales about the box centre instead of the opposite
    /// handle -- the alt/option modifier. `lock_aspect` forces one factor on
    /// both axes, the shift modifier, which for an edge handle means it scales
    /// the other axis too rather than doing nothing.
    ///
    /// An axis the handle does not drive (the x axis of a north handle) is left
    /// alone by the degenerate-denominator guard rather than by a case
    /// analysis: the handle and its anchor share that coordinate, so there is
    /// no ratio to take.
    #[must_use]
    pub fn scale(
        &self,
        handle: Handle,
        cursor: Point,
        from_center: bool,
        lock_aspect: bool,
    ) -> Affine {
        let position = handle.position(self.bounds, self.pivot);
        let anchor = if from_center {
            self.bounds.center()
        } else {
            handle.opposite().position(self.bounds, self.pivot)
        };
        let (sx, drives_x) = axis_factor(cursor.x, position.x, anchor.x);
        let (sy, drives_y) = axis_factor(cursor.y, position.y, anchor.y);
        let (sx, sy) = if lock_aspect {
            let k = match (drives_x, drives_y) {
                (true, true) => sx.abs().max(sy.abs()),
                (true, false) => sx.abs(),
                (false, true) => sy.abs(),
                (false, false) => 1.0,
            };
            (
                if drives_x { k * sign(sx) } else { k },
                if drives_y { k * sign(sy) } else { k },
            )
        } else {
            (sx, sy)
        };
        about(anchor, Affine::scale_non_uniform(sx, sy))
    }

    /// The gesture that turns the box about its pivot, dragging document point
    /// `from` to document point `to`.
    #[must_use]
    pub fn rotate(&self, from: Point, to: Point) -> Affine {
        let (a, b) = (from - self.pivot, to - self.pivot);
        // A drag that starts on the pivot has no angle to measure; a zero
        // vector's `angle` is 0, which would snap the selection to whatever
        // `to` happens to be.
        if a.length_squared() == 0.0 || b.length_squared() == 0.0 {
            return Affine::IDENTITY;
        }
        Affine::rotate_about(b.angle() - a.angle(), self.pivot)
    }

    /// The gesture that skews the box by dragging an edge handle from `from` to
    /// `to`, holding the opposite edge still.
    ///
    /// Only the four edge handles skew; a corner has no edge to slide, so a
    /// corner (or the pivot) returns the identity rather than an invented
    /// combination of the two axes.
    #[must_use]
    pub fn skew(&self, handle: Handle, from: Point, to: Point) -> Affine {
        let horizontal = matches!(handle, Handle::North | Handle::South);
        let vertical = matches!(handle, Handle::East | Handle::West);
        if !horizontal && !vertical {
            return Affine::IDENTITY;
        }
        let position = handle.position(self.bounds, self.pivot);
        let anchor = handle.opposite().position(self.bounds, self.pivot);
        let delta = to - from;
        let (kx, ky) = if horizontal {
            let span = position.y - anchor.y;
            (if span == 0.0 { 0.0 } else { delta.x / span }, 0.0)
        } else {
            let span = position.x - anchor.x;
            (0.0, if span == 0.0 { 0.0 } else { delta.y / span })
        };
        about(anchor, Affine::skew(kx, ky))
    }
}

/// `transform` applied about `anchor` rather than about the origin.
fn about(anchor: Point, transform: Affine) -> Affine {
    Affine::translate(anchor.to_vec2())
        * transform
        * Affine::translate(Vec2::new(-anchor.x, -anchor.y))
}

/// The scale factor along one axis, and whether the handle drives that axis.
///
/// A handle whose coordinate equals its anchor's -- the x of a north handle --
/// cannot say anything about that axis, and the ratio would be 0/0.
fn axis_factor(cursor: f64, position: f64, anchor: f64) -> (f64, bool) {
    let span = position - anchor;
    if span == 0.0 {
        (1.0, false)
    } else {
        ((cursor - anchor) / span, true)
    }
}

/// The sign of `s` as `1.0` or `-1.0`, never zero and never NaN.
fn sign(s: f64) -> f64 {
    if s < 0.0 { -1.0 } else { 1.0 }
}

/// The placements the selected shapes have right now, to be captured at the
/// start of a gesture and handed to every [`Transform`] the gesture emits.
///
/// A drag must build each frame's command from these and not from the
/// document, or the gesture compounds: dragging a corner by 10% would grow the
/// selection by 10% per frame instead of by 10% in total.
#[must_use]
pub fn gesture_start(doc: &Document, selection: &Selection) -> Vec<(NodeId, Affine)> {
    selection
        .iter()
        .filter_map(|id| doc.get(id).map(|shape| (id, shape.transform)))
        .collect()
}

/// An undoable transform of a set of shapes.
pub struct Transform {
    /// The absolute shape-to-document transform to install on each target.
    targets: Vec<(NodeId, Affine)>,
    /// What each target had before, captured by the first `apply` and put back
    /// verbatim by `revert`. `None` for a target that was deleted meanwhile,
    /// which `revert` must then leave alone rather than resurrect.
    before: Option<Vec<Option<Affine>>>,
}

impl Transform {
    /// The command that applies `gesture` to the placements in `start`.
    ///
    /// `start` comes from [`gesture_start`]. One affine for every shape is what
    /// keeps a multi-selection rigid.
    #[must_use]
    pub fn new(start: &[(NodeId, Affine)], gesture: Affine) -> Self {
        Self {
            targets: start
                .iter()
                .map(|&(id, transform)| (id, gesture * transform))
                .collect(),
            before: None,
        }
    }

    /// The ids this command transforms.
    pub fn targets(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.targets.iter().map(|&(id, _)| id)
    }
}

impl Command for Transform {
    type Doc = Document;

    fn apply(&mut self, doc: &mut Document) {
        self.before = Some(
            self.targets
                .iter()
                .map(|&(id, transform)| doc.set_transform(id, transform))
                .collect(),
        );
    }

    fn revert(&mut self, doc: &mut Document) {
        let Some(before) = self.before.take() else {
            return;
        };
        for (&(id, _), previous) in self.targets.iter().zip(before) {
            if let Some(previous) = previous {
                doc.set_transform(id, previous);
            }
        }
    }

    fn merge(&mut self, next: Self) -> Option<Self> {
        // Same shapes in the same order is the drag case: the gesture is
        // recomputed from the same `gesture_start` every frame, so `next`
        // already describes the whole drag and `self.before` already holds
        // where it began. Anything else stays a separate undo step.
        if self.before.is_some() && self.targets.len() == next.targets.len() {
            let same = self
                .targets
                .iter()
                .zip(&next.targets)
                .all(|(a, b)| a.0 == b.0);
            if same {
                self.targets = next.targets;
                return None;
            }
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Shape;
    use crate::document::tests::{rect_path, rect_shape};
    use crate::select::{SelectMode, Selection};
    use crate::undo::UndoLog;
    use hane_geom::fuzz::{Rng, check};

    fn document() -> (Document, Vec<NodeId>, Selection) {
        let mut doc = Document::new(Rect::new(-1000.0, -1000.0, 1000.0, 1000.0));
        let ids: Vec<NodeId> = (0..3)
            .map(|i| {
                let x = f64::from(i) * 40.0;
                doc.insert(rect_shape(Rect::new(x, 0.0, x + 20.0, 20.0)))
            })
            .collect();
        let mut selection = Selection::new();
        selection.apply(SelectMode::Replace, &ids);
        (doc, ids, selection)
    }

    /// Applies `gesture` through the undo log, as a tool would.
    fn drag(
        doc: &mut Document,
        log: &mut UndoLog<Transform>,
        start: &[(NodeId, Affine)],
        g: Affine,
    ) {
        log.edit(doc, Transform::new(start, g));
    }

    fn close(a: Rect, b: Rect) -> bool {
        let scale = 1.0 + a.size().length() + b.size().length();
        (a.x0 - b.x0)
            .abs()
            .max((a.y0 - b.y0).abs())
            .max((a.x1 - b.x1).abs())
            .max((a.y1 - b.y1).abs())
            <= 1e-9 * scale
    }

    #[test]
    fn eight_handles_sit_where_the_box_does() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        assert_eq!(b.bounds, Rect::new(0.0, 0.0, 100.0, 20.0));
        assert_eq!(b.pivot, Point::new(50.0, 10.0));
        let positions: Vec<Point> = SCALE_HANDLES
            .iter()
            .map(|h| h.position(b.bounds, b.pivot))
            .collect();
        assert_eq!(positions[0], Point::new(0.0, 0.0));
        assert_eq!(positions[2], Point::new(100.0, 0.0));
        assert_eq!(positions[4], Point::new(100.0, 20.0));
        assert_eq!(positions[6], Point::new(0.0, 20.0));
        // Every handle is its opposite's opposite, and no handle is its own.
        for &h in &SCALE_HANDLES {
            assert_eq!(h.opposite().opposite(), h);
            assert_ne!(h.opposite(), h);
        }
    }

    #[test]
    fn nothing_selected_has_no_box() {
        let (doc, _, _) = document();
        assert!(TransformBox::new(&doc, &Selection::new()).is_none());
    }

    #[test]
    fn a_corner_handle_scales_about_the_opposite_corner() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        // Drag the south-east corner out to double the box.
        let g = b.scale(Handle::SouthEast, Point::new(200.0, 40.0), false, false);
        assert_eq!(
            g * Point::new(0.0, 0.0),
            Point::new(0.0, 0.0),
            "anchor held"
        );
        assert_eq!(g * Point::new(100.0, 20.0), Point::new(200.0, 40.0));
    }

    #[test]
    fn an_edge_handle_scales_one_axis_only() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        let g = b.scale(Handle::North, Point::new(999.0, -20.0), false, false);
        // The x of the cursor is ignored: a north handle drives y alone.
        assert_eq!(g * Point::new(0.0, 20.0), Point::new(0.0, 20.0));
        assert_eq!(g * Point::new(0.0, 0.0), Point::new(0.0, -20.0));
        assert_eq!(g * Point::new(100.0, 0.0), Point::new(100.0, -20.0));
    }

    #[test]
    fn from_center_holds_the_centre_and_moves_both_sides() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        // East sits 50 from the centre and the cursor is 100 from it, so this
        // is a doubling about the centre -- and the *west* edge moves too,
        // which is the difference from the default anchor.
        let g = b.scale(Handle::East, Point::new(150.0, 10.0), true, false);
        assert_eq!(g * b.bounds.center(), b.bounds.center());
        assert_eq!(g * Point::new(100.0, 10.0), Point::new(150.0, 10.0));
        assert_eq!(g * Point::new(0.0, 10.0), Point::new(-50.0, 10.0));
        let scaled = b.bounds.transform(g);
        assert!(
            close(scaled, Rect::new(-50.0, 0.0, 150.0, 20.0)),
            "{scaled:?}"
        );
    }

    #[test]
    fn locking_the_aspect_gives_one_factor_to_both_axes() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        // A corner dragged to 2x in x and 4x in y: the larger wins.
        let g = b.scale(Handle::SouthEast, Point::new(200.0, 80.0), false, true);
        assert!(close(
            b.bounds.transform(g),
            Rect::new(0.0, 0.0, 400.0, 80.0)
        ));
        // An edge handle under the lock scales the other axis too, which is
        // the whole point of the modifier there. Its anchor is the west
        // handle, at mid-height, so the box grows above and below.
        let g = b.scale(Handle::East, Point::new(200.0, 10.0), false, true);
        assert!(close(
            b.bounds.transform(g),
            Rect::new(0.0, -10.0, 200.0, 30.0)
        ));
    }

    #[test]
    fn a_scale_dragged_past_the_anchor_flips_rather_than_collapsing() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        let g = b.scale(Handle::East, Point::new(-100.0, 10.0), false, false);
        assert!(g.determinant() < 0.0, "dragging through the anchor mirrors");
        assert!(close(
            b.bounds.transform(g),
            Rect::new(-100.0, 0.0, 0.0, 20.0)
        ));
    }

    #[test]
    fn rotation_turns_about_the_pivot_and_the_pivot_moves() {
        let (doc, _, selection) = document();
        let mut b = TransformBox::new(&doc, &selection).unwrap();
        let quarter = b.rotate(Point::new(100.0, 10.0), Point::new(50.0, 60.0));
        assert_eq!(quarter * b.pivot, b.pivot, "the pivot is the fixed point");
        let turned = quarter * Point::new(100.0, 10.0);
        assert!(turned.distance(Point::new(50.0, 60.0)) < 1e-9, "{turned:?}");

        b.set_pivot(Point::new(0.0, 0.0));
        let about_corner = b.rotate(Point::new(10.0, 0.0), Point::new(0.0, 10.0));
        assert_eq!(about_corner * Point::ORIGIN, Point::ORIGIN);
        assert!((about_corner * Point::new(20.0, 0.0)).distance(Point::new(0.0, 20.0)) < 1e-9);

        // A drag starting on the pivot has no angle and must not invent one.
        assert_eq!(b.rotate(b.pivot, Point::new(5.0, 5.0)), Affine::IDENTITY);
    }

    #[test]
    fn an_edge_handle_skews_and_a_corner_does_not() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        // Drag the top edge right by 20 over a box 20 tall: a 45 degree slant.
        let g = b.skew(Handle::North, Point::new(50.0, 0.0), Point::new(70.0, 0.0));
        assert_eq!(
            g * Point::new(0.0, 20.0),
            Point::new(0.0, 20.0),
            "bottom held"
        );
        assert_eq!(g * Point::new(0.0, 0.0), Point::new(20.0, 0.0));
        assert_eq!(
            b.skew(Handle::NorthEast, Point::ORIGIN, Point::new(9.0, 9.0)),
            Affine::IDENTITY
        );
    }

    #[test]
    fn handles_are_grabbable_at_every_zoom() {
        let (doc, _, selection) = document();
        let b = TransformBox::new(&doc, &selection).unwrap();
        for factor in [1.0, 200.0, 0.005] {
            let mut view = View::new();
            view.zoom_about(Point::ORIGIN, factor);
            let corner = view.to_screen(Point::new(100.0, 20.0));
            // Three screen pixels off a ten-pixel handle: inside it whatever
            // the zoom. A grab radius stored in document units fails one of
            // these two at every factor but 1.
            assert_eq!(
                b.handle_at(&view, corner + Vec2::new(3.0, 0.0), 10.0),
                Some(Handle::SouthEast),
                "{factor}"
            );
            assert_eq!(
                b.handle_at(&view, corner + Vec2::new(30.0, 30.0), 10.0),
                None,
                "{factor}"
            );
        }
    }

    #[test]
    fn the_pivot_wins_where_it_overlaps_a_scale_handle() {
        let (doc, _, selection) = document();
        let mut b = TransformBox::new(&doc, &selection).unwrap();
        let view = View::new();
        assert_eq!(
            b.handle_at(&view, Point::new(100.0, 20.0), 10.0),
            Some(Handle::SouthEast)
        );
        b.set_pivot(Point::new(100.0, 20.0));
        assert_eq!(
            b.handle_at(&view, Point::new(100.0, 20.0), 10.0),
            Some(Handle::Pivot)
        );
    }

    #[test]
    fn the_box_tracks_the_selection_through_a_transform() {
        let (mut doc, _, selection) = document();
        let mut b = TransformBox::new(&doc, &selection).unwrap();
        b.set_pivot(Point::new(0.0, 0.0));
        let start = gesture_start(&doc, &selection);
        let mut log = UndoLog::new(8);
        drag(
            &mut doc,
            &mut log,
            &start,
            Affine::translate(Vec2::new(7.0, -3.0)),
        );
        b.track(&doc, &selection);
        assert!(close(b.bounds, Rect::new(7.0, -3.0, 107.0, 17.0)));
        assert_eq!(b.pivot, Point::new(0.0, 0.0), "a moved pivot stays put");

        // And a rotation: the box is recomputed from the shapes, so it stays
        // axis-aligned around what is actually drawn.
        drag(
            &mut doc,
            &mut log,
            &start,
            Affine::rotate_about(core::f64::consts::FRAC_PI_2, Point::ORIGIN),
        );
        b.track(&doc, &selection);
        assert!(
            close(b.bounds, Rect::new(-20.0, 0.0, 0.0, 100.0)),
            "{:?}",
            b.bounds
        );
    }

    #[test]
    fn a_multi_selection_keeps_its_relative_positions() {
        let (mut doc, ids, selection) = document();
        let start = gesture_start(&doc, &selection);
        let before: Vec<Rect> = ids.iter().map(|&id| doc.bounds(id).unwrap()).collect();
        let mut log = UndoLog::new(8);

        let g = Affine::rotate_about(0.7, Point::new(50.0, 10.0)) * Affine::scale(2.0);
        drag(&mut doc, &mut log, &start, g);

        // Every shape moved by the same affine, so every distance between two
        // of them scales by that affine's factor and nothing shears apart.
        // Compared on centres rather than on the boxes, whose axis-aligned
        // extents legitimately change under a rotation.
        let after: Vec<Rect> = ids.iter().map(|&id| doc.bounds(id).unwrap()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let was = before[i].center().distance(before[j].center());
                let now = after[i].center().distance(after[j].center());
                assert!((now / was - 2.0).abs() < 1e-9, "{i}-{j}: {now} / {was}");
            }
        }
        // And each shape is still the same shape, just twice the size: its
        // transform is exactly the gesture composed onto what it had.
        for (&id, &(_, was)) in ids.iter().zip(&start) {
            assert_eq!(
                doc.get(id).unwrap().transform.as_coeffs(),
                (g * was).as_coeffs()
            );
        }
    }

    #[test]
    fn a_drag_is_one_undo_step_that_restores_the_start_exactly() {
        let (mut doc, ids, selection) = document();
        let bits: Vec<[f64; 6]> = ids
            .iter()
            .map(|&id| doc.get(id).unwrap().transform.as_coeffs())
            .collect();
        let start = gesture_start(&doc, &selection);
        let mut log = UndoLog::new(64);
        for step in 1..=300 {
            let g = Affine::rotate_about(f64::from(step) * 0.01, Point::new(1e9, 1e9));
            drag(&mut doc, &mut log, &start, g);
        }
        assert_eq!(log.len(), 1, "a drag is one step");
        assert!(log.undo(&mut doc));
        for (&id, expected) in ids.iter().zip(&bits) {
            // Bit-exact, not close: the command put the old coefficients back
            // rather than inverting the gesture.
            assert_eq!(&doc.get(id).unwrap().transform.as_coeffs(), expected);
        }
        assert!(log.redo(&mut doc));
        assert!(log.undo(&mut doc));
        for (&id, expected) in ids.iter().zip(&bits) {
            assert_eq!(&doc.get(id).unwrap().transform.as_coeffs(), expected);
        }
    }

    #[test]
    fn a_different_selection_is_a_different_step() {
        let (mut doc, ids, selection) = document();
        let mut log = UndoLog::new(64);
        let all = gesture_start(&doc, &selection);
        drag(
            &mut doc,
            &mut log,
            &all,
            Affine::translate(Vec2::new(1.0, 0.0)),
        );
        let mut one = Selection::new();
        one.select(ids[0]);
        let single = gesture_start(&doc, &one);
        drag(
            &mut doc,
            &mut log,
            &single,
            Affine::translate(Vec2::new(1.0, 0.0)),
        );
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn a_target_deleted_mid_gesture_is_not_resurrected() {
        let (mut doc, ids, selection) = document();
        let start = gesture_start(&doc, &selection);
        doc.remove(ids[1]);
        let mut log = UndoLog::new(8);
        drag(
            &mut doc,
            &mut log,
            &start,
            Affine::translate(Vec2::new(5.0, 5.0)),
        );
        assert!(log.undo(&mut doc));
        assert!(doc.get(ids[1]).is_none());
        assert_eq!(doc.len(), 2);
    }

    /// Random gestures over random selections, fully undone, must restore the
    /// exact coefficients -- the property a delta-based command fails.
    #[test]
    fn fuzz_undo_restores_the_exact_coefficients() {
        check(
            "transform undo restores coefficients",
            600,
            |r: &mut Rng| {
                let steps: Vec<(u64, f64, f64, f64)> = (0..r.below(12))
                    .map(|_| (r.below(3), r.coord(), r.coord(), r.unit() * 6.0 - 3.0))
                    .collect();
                (r.below(3), steps)
            },
            |(which, steps)| {
                let mut doc = Document::new(Rect::new(-1e6, -1e6, 1e6, 1e6));
                let ids: Vec<NodeId> = (0..3)
                    .map(|i| {
                        let mut s = Shape::new(rect_path(Rect::new(0.0, 0.0, 10.0, 10.0)));
                        s.transform = Affine::translate(Vec2::new(f64::from(i) * 30.0, 0.0));
                        doc.insert(s)
                    })
                    .collect();
                let mut selection = Selection::new();
                selection.apply(SelectMode::Replace, &ids[..=*which as usize]);
                let coeffs: Vec<[f64; 6]> = ids
                    .iter()
                    .map(|&id| doc.get(id).unwrap().transform.as_coeffs())
                    .collect();

                let start = gesture_start(&doc, &selection);
                let mut log = UndoLog::new(64);
                for &(kind, x, y, k) in steps {
                    // Built from raw coefficients rather than from a rotation:
                    // a seeded case has to reproduce bit for bit, and `sin`
                    // and `cos` are not bit-identical across platforms.
                    let g = match kind {
                        0 => Affine::translate(Vec2::new(x, y)),
                        1 => Affine::scale_non_uniform(k, 1.0 + k.abs()),
                        _ => Affine::skew(k, 0.0),
                    };
                    log.edit(&mut doc, Transform::new(&start, g));
                    if kind == 1 {
                        log.seal();
                    }
                }
                while log.undo(&mut doc) {}
                ids.iter()
                    .zip(&coeffs)
                    .all(|(&id, c)| doc.get(id).unwrap().transform.as_coeffs() == *c)
            },
        );
    }
}
