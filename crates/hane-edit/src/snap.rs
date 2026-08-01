//! Snapping a dragged selection to object edges, object centres, guides and
//! the grid.
//!
//! # Screen pixels, quadtree candidates
//!
//! The snap distance is what the hand can aim, so it is in screen pixels and
//! goes through [`document_length`] once per call. Stored in document units it
//! would snap from across the page zoomed out and be unusable zoomed in.
//!
//! Candidates come from the [`Quadtree`](hane_scene::Quadtree) query of the
//! moving box grown by that distance, so the cost is the number of shapes
//! *near* the drag and not the number in the file -- which is the only reason
//! this can run every frame over 100k objects.
//!
//! # What snaps to what
//!
//! Three source lines per axis (the moving box's two edges and its centre)
//! against every candidate's three, plus the guides and the grid. Nine
//! comparisons per nearby object per axis, which is nothing next to the query
//! that found them. The nearest candidate within the distance wins each axis
//! independently, so a box can snap its left edge to one object and its top to
//! another -- which is what a user aligning a layout expects.

use crate::document::{Document, document_length};
use crate::select::Selection;
use hane_geom::{Rect, Vec2};
use hane_scene::{NodeId, View};

/// A feature the drag snapped to, for drawing the indicator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapTarget {
    /// The document-space coordinate snapped to, on the axis it belongs to.
    /// The indicator is a line there.
    pub position: f64,
    /// The shape the feature came from, or `None` for a guide or the grid.
    /// The indicator can then be drawn spanning both boxes rather than the
    /// whole canvas.
    pub source: Option<NodeId>,
}

/// The result of a snap: how far to nudge the drag, and what it caught.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snap {
    /// Add this to the drag's translation.
    pub offset: Vec2,
    /// What the x axis caught, if anything.
    pub x: Option<SnapTarget>,
    /// What the y axis caught, if anything.
    pub y: Option<SnapTarget>,
}

impl Snap {
    /// No snap: a zero nudge and nothing to draw.
    pub const NONE: Self = Self {
        offset: Vec2::ZERO,
        x: None,
        y: None,
    };
}

/// The snapping configuration for a drag.
#[derive(Clone, Debug)]
pub struct Snapper {
    /// Whether to snap at all. Clear it while the suppress modifier is held --
    /// the drag then reports [`Snap::NONE`] rather than the caller having to
    /// remember to skip the call.
    pub enabled: bool,
    /// How near, in *screen pixels*, a feature has to be to catch.
    pub distance_px: f64,
    /// Grid pitch in document units, or `None` for no grid.
    pub grid: Option<f64>,
    /// Vertical guides, as document x coordinates.
    pub guides_x: Vec<f64>,
    /// Horizontal guides, as document y coordinates.
    pub guides_y: Vec<f64>,
}

impl Default for Snapper {
    fn default() -> Self {
        Self::new()
    }
}

impl Snapper {
    /// Snapping on, eight pixels, no grid and no guides.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            distance_px: 8.0,
            grid: None,
            guides_x: Vec::new(),
            guides_y: Vec::new(),
        }
    }

    /// The nudge that puts `moving` onto the nearest feature.
    ///
    /// `moving` is the dragged selection's document-space box at the cursor's
    /// current position. `exclude` is the selection itself: a shape must not
    /// snap to where it already is, or a drag can never leave its start.
    #[must_use]
    pub fn snap(&self, doc: &Document, view: &View, moving: Rect, exclude: &Selection) -> Snap {
        let reach = document_length(view, self.distance_px);
        // `is_finite` as well as positive: a NaN reach would make every
        // comparison below false and silently disable snapping instead.
        if !self.enabled || reach <= 0.0 || !reach.is_finite() || moving.is_empty() {
            return Snap::NONE;
        }

        let centre = moving.center();
        let sources_x = [moving.x0, centre.x, moving.x1];
        let sources_y = [moving.y0, centre.y, moving.y1];
        let mut x = Axis::new(reach);
        let mut y = Axis::new(reach);

        // ponytail: candidates are what the drag's neighbourhood overlaps, so a
        // box aligns to its neighbours and not to one on the far side of the
        // page -- that needs a guide. Query the viewport rectangle instead
        // (and pass it in) if long-range alignment guides are ever wanted; the
        // cost is then bounded by what is on screen rather than by the drag.
        let mut raw = Vec::new();
        doc.query(moving.inflate(reach), &mut raw);
        for id in raw.into_iter().map(NodeId::from_bits) {
            if exclude.contains(id) {
                continue;
            }
            let Some(bounds) = doc.bounds(id) else {
                continue;
            };
            let c = bounds.center();
            for target in [bounds.x0, c.x, bounds.x1] {
                x.consider(&sources_x, target, Some(id));
            }
            for target in [bounds.y0, c.y, bounds.y1] {
                y.consider(&sources_y, target, Some(id));
            }
        }

        for &guide in &self.guides_x {
            x.consider(&sources_x, guide, None);
        }
        for &guide in &self.guides_y {
            y.consider(&sources_y, guide, None);
        }

        if let Some(step) = self.grid.filter(|s| *s > 0.0) {
            // Each source against its own nearest grid line only: the whole
            // grid is a candidate everywhere, so pairing them all up would
            // just re-find lines further away.
            for &source in &sources_x {
                x.consider(&[source], (source / step).round() * step, None);
            }
            for &source in &sources_y {
                y.consider(&[source], (source / step).round() * step, None);
            }
        }

        Snap {
            offset: Vec2::new(x.offset, y.offset),
            x: x.target,
            y: y.target,
        }
    }
}

/// The best snap found so far on one axis.
struct Axis {
    /// Distance to beat. Starts at the reach, so a candidate further than that
    /// simply never wins and there is no second threshold test.
    best: f64,
    offset: f64,
    target: Option<SnapTarget>,
}

impl Axis {
    fn new(reach: f64) -> Self {
        Self {
            best: reach,
            offset: 0.0,
            target: None,
        }
    }

    /// Tries `target` against every one of the moving box's source lines.
    fn consider(&mut self, sources: &[f64], target: f64, source: Option<NodeId>) {
        for &from in sources {
            let delta = target - from;
            let distance = delta.abs();
            // Strictly nearer, so the first source line to reach a given
            // distance keeps it: edges before centres, which is what a user
            // aligning two boxes means by "snap".
            if distance < self.best {
                self.best = distance;
                self.offset = delta;
                self.target = Some(SnapTarget {
                    position: target,
                    source,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::{grid_document, rect_shape};
    use crate::select::Selection;
    use hane_geom::Point;
    use hane_scene::NodeId;
    use std::time::Instant;

    /// Debug builds run an order of magnitude slower than the release build
    /// the issue's budget is about, and CI runs the debug one.
    const SLOWDOWN: f64 = if cfg!(debug_assertions) { 25.0 } else { 1.0 };

    /// A fixed shape at x in 100..120, and an empty selection to exclude.
    fn document() -> (Document, NodeId) {
        let mut doc = Document::new(Rect::new(-1000.0, -1000.0, 1000.0, 1000.0));
        let id = doc.insert(rect_shape(Rect::new(100.0, 100.0, 120.0, 120.0)));
        (doc, id)
    }

    #[test]
    fn a_near_edge_catches_and_a_far_one_does_not() {
        let (doc, id) = document();
        let snapper = Snapper::new();
        let view = View::new();
        let nothing = Selection::new();

        // The same size as the fixed shape, three units up and to the left, so
        // all three feature pairings on each axis agree on the same nudge and
        // the reported target is the first of them.
        let moving = Rect::new(97.0, 97.0, 117.0, 117.0);
        let snap = snapper.snap(&doc, &view, moving, &nothing);
        assert_eq!(snap.offset, Vec2::new(3.0, 3.0));
        assert_eq!(
            snap.x,
            Some(SnapTarget {
                position: 100.0,
                source: Some(id)
            })
        );

        // Twenty units away is past the eight-pixel reach, and far enough that
        // the broad phase does not even offer it.
        let far = Rect::new(140.0, 140.0, 160.0, 160.0);
        assert_eq!(snapper.snap(&doc, &view, far, &nothing), Snap::NONE);
    }

    #[test]
    fn both_axes_snap_independently() {
        let (doc, _) = document();
        let snapper = Snapper::new();
        let view = View::new();
        // Right edge near the fixed left edge, centre near the fixed centre.
        let moving = Rect::new(78.0, 98.0, 98.0, 118.0);
        let snap = snapper.snap(&doc, &view, moving, &Selection::new());
        assert_eq!(snap.offset, Vec2::new(2.0, 2.0));
        assert!(snap.x.is_some() && snap.y.is_some());
    }

    #[test]
    fn a_centre_snaps_to_a_centre() {
        let (doc, _) = document();
        let snapper = Snapper::new();
        let view = View::new();
        // Nothing near an edge, but the centres are three apart.
        let moving = Rect::new(57.0, 60.0, 157.0, 120.0);
        let snap = snapper.snap(&doc, &view, moving, &Selection::new());
        assert_eq!(snap.offset, Vec2::new(3.0, 0.0));
        assert_eq!(snap.x.unwrap().position, 110.0);
    }

    #[test]
    fn the_selection_does_not_snap_to_itself() {
        let (doc, id) = document();
        let snapper = Snapper::new();
        let view = View::new();
        let mut selection = Selection::new();
        selection.select(id);
        // A drag of one unit: without the exclusion the shape's own edge is
        // one away and the drag would never start.
        let moving = Rect::new(101.0, 101.0, 121.0, 121.0);
        assert_eq!(snapper.snap(&doc, &view, moving, &selection), Snap::NONE);
        assert_ne!(
            snapper.snap(&doc, &view, moving, &Selection::new()),
            Snap::NONE
        );
    }

    #[test]
    fn the_reach_is_screen_pixels_at_every_zoom() {
        let (doc, _) = document();
        let snapper = Snapper::new();
        let nothing = Selection::new();
        for factor in [1.0, 20.0, 0.05] {
            let mut view = View::new();
            view.zoom_about(Point::ORIGIN, factor);
            let reach = 8.0 / factor;
            // A hair-thin moving box, so its three x features coincide and the
            // only distance in play is the one being measured -- a box as wide
            // as the fixed one has features 10 apart and snaps to the *next*
            // feature just as the intended one goes out of reach.
            let near = Rect::new(100.0 - reach * 0.9, 105.0, 100.001 - reach * 0.9, 115.0);
            let far = Rect::new(100.0 - reach * 1.1, 105.0, 100.001 - reach * 1.1, 115.0);
            assert!(
                snapper.snap(&doc, &view, near, &nothing).x.is_some(),
                "{factor}"
            );
            assert!(
                snapper.snap(&doc, &view, far, &nothing).x.is_none(),
                "{factor}"
            );
        }
    }

    #[test]
    fn guides_and_the_grid_snap_and_report_no_source() {
        let mut doc = Document::new(Rect::new(-1000.0, -1000.0, 1000.0, 1000.0));
        doc.insert(rect_shape(Rect::new(900.0, 900.0, 910.0, 910.0)));
        let mut snapper = Snapper::new();
        snapper.guides_x = vec![50.0];
        let view = View::new();
        let moving = Rect::new(47.0, 0.0, 67.0, 20.0);
        let snap = snapper.snap(&doc, &view, moving, &Selection::new());
        assert_eq!(snap.offset, Vec2::new(3.0, 0.0));
        assert_eq!(
            snap.x,
            Some(SnapTarget {
                position: 50.0,
                source: None
            })
        );

        snapper.guides_x.clear();
        snapper.grid = Some(10.0);
        let moving = Rect::new(47.0, 3.0, 67.0, 23.0);
        let snap = snapper.snap(&doc, &view, moving, &Selection::new());
        assert_eq!(snap.offset, Vec2::new(3.0, -3.0));
        assert_eq!(snap.y.unwrap().position, 0.0);
    }

    #[test]
    fn a_nearer_object_beats_a_further_guide() {
        let (doc, id) = document();
        let mut snapper = Snapper::new();
        snapper.guides_x = vec![95.0];
        let view = View::new();
        // Left edge at 99: one from the object's edge, four from the guide.
        let moving = Rect::new(99.0, 105.0, 119.0, 125.0);
        let snap = snapper.snap(&doc, &view, moving, &Selection::new());
        assert_eq!(snap.x.unwrap().source, Some(id));
        assert_eq!(snap.offset.x, 1.0);
    }

    #[test]
    fn the_modifier_suppresses_everything() {
        let (doc, _) = document();
        let mut snapper = Snapper::new();
        snapper.grid = Some(10.0);
        snapper.guides_x = vec![98.0];
        snapper.enabled = false;
        let view = View::new();
        let moving = Rect::new(99.0, 99.0, 119.0, 119.0);
        assert_eq!(
            snapper.snap(&doc, &view, moving, &Selection::new()),
            Snap::NONE
        );
    }

    #[test]
    fn snapping_a_hundred_thousand_shapes_stays_interactive() {
        let doc = grid_document(100_000);
        let snapper = Snapper::new();
        let view = View::new();
        let nothing = Selection::new();

        // A drag crossing the grid, so each frame queries a different region
        // and no single quadtree node stays warm.
        let frames: Vec<Rect> = (0..1000)
            .map(|i| {
                let k = f64::from(i) * 3.1;
                Rect::new(k, k * 0.7, k + 20.0, k * 0.7 + 20.0)
            })
            .collect();

        let start = Instant::now();
        let mut caught = 0;
        for &moving in &frames {
            let snap = snapper.snap(&doc, &view, moving, &nothing);
            if snap.x.is_some() || snap.y.is_some() {
                caught += 1;
            }
        }
        let per_frame = start.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64;
        assert!(caught > 0, "nothing snapped, so nothing was measured");
        assert!(
            per_frame < SLOWDOWN,
            "{per_frame:.4} ms per snap over 100k shapes"
        );
    }
}
