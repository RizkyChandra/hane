//! Viewport culling: the document-space rectangle to query each frame (#28).
//!
//! The whole of culling is one rectangle. [`Quadtree::query`](crate::Quadtree)
//! already answers "which items overlap this box" in time proportional to the
//! answer rather than to the document, so the only thing missing was turning a
//! viewport -- which lives in screen pixels and may be rotated -- into a box
//! the index can be asked about.

use hane_geom::{Point, Rect};

use crate::View;

impl View {
    /// The document-space bounding box of the screen rectangle `screen`, grown
    /// by `margin` screen pixels on every side.
    ///
    /// Hand the result to [`Quadtree::query`](crate::Quadtree::query) to get
    /// the visible set. `margin` is the overdraw band: a scroll that moves less
    /// than `margin` pixels between frames stays inside the set that was
    /// already culled, so a smooth drag does not re-cull and re-render at its
    /// leading edge every frame. It is in screen pixels rather than document
    /// units precisely so that it does not change meaning when the user zooms.
    ///
    /// Under rotation this is the bounding box of the rotated viewport, not the
    /// viewport, so it over-selects by up to 2x at 45 degrees. That is the
    /// intended reading of #28 and it is the right trade: the index answers
    /// axis-aligned queries, and an exact tilted test would have to run
    /// per-item, costing more than drawing the few extra items it would reject.
    ///
    /// A viewport with no area -- one shrunk away by a negative `margin`, a
    /// canvas not laid out yet, [`Rect::EMPTY`] -- comes back as
    /// [`Rect::EMPTY`], which matches nothing.
    #[must_use]
    pub fn visible_bounds(&self, screen: Rect, margin: f64) -> Rect {
        let s = screen.inflate(margin);
        // Mirrors `Rect::transform`, and for the same reason: `Rect::EMPTY`'s
        // corners are inverted infinities, whose products with a rotation give
        // an infinity in one axis and a NaN in the other. Unioned, the four of
        // them cover the whole plane -- so a viewport of no area would cull
        // nothing at all, which is the opposite of the answer. NaN bounds land
        // here too, since `is_empty` is a negated comparison.
        if s.is_empty() {
            return Rect::EMPTY;
        }
        // `Rect::EMPTY` is the union identity, so the fold accumulates the
        // bound of the four mapped corners with no special case for the first.
        [
            Point::new(s.x0, s.y0),
            Point::new(s.x1, s.y0),
            Point::new(s.x0, s.y1),
            Point::new(s.x1, s.y1),
        ]
        .into_iter()
        .fold(Rect::EMPTY, |acc, corner| {
            acc.union_point(self.to_document(corner))
        })
    }
}

#[cfg(test)]
mod tests {
    use hane_geom::{Rect, Vec2};

    use crate::{Quadtree, View};

    const SCREEN: Rect = Rect::new(0.0, 0.0, 1280.0, 720.0);

    /// The oracle: what a scan over the same items says is visible, using the
    /// same rectangle. Culling must never be more clever than the index.
    fn brute(items: &[(u32, Rect)], area: Rect) -> Vec<u32> {
        let mut v: Vec<u32> = items
            .iter()
            .filter(|(_, b)| b.overlaps(area))
            .map(|&(id, _)| id)
            .collect();
        v.sort_unstable();
        v
    }

    fn cull(tree: &Quadtree, view: &View, margin: f64) -> Vec<u32> {
        let mut v = Vec::new();
        tree.query(view.visible_bounds(SCREEN, margin), &mut v);
        v.sort_unstable();
        v
    }

    fn scene(rng: &mut hane_geom::fuzz::Rng, n: u32) -> Vec<(u32, Rect)> {
        (0..n)
            .map(|id| {
                let x = rng.below(4000) as f64 - 2000.0;
                let y = rng.below(4000) as f64 - 2000.0;
                let (w, h) = (rng.below(60) as f64 + 1.0, rng.below(60) as f64 + 1.0);
                (id, Rect::new(x, y, x + w, y + h))
            })
            .collect()
    }

    #[test]
    fn the_visible_set_is_exactly_what_overlaps_the_query_box() {
        let mut rng = hane_geom::fuzz::Rng::new(1);
        let items = scene(&mut rng, 2000);
        let tree = Quadtree::bulk_load(Rect::new(-2100.0, -2100.0, 2100.0, 2100.0), &items);
        let mut view = View::new();
        for step in 0..40 {
            view.pan_by(Vec2::new(37.0, -19.0));
            view.zoom_about(hane_geom::Point::new(640.0, 360.0), 1.07);
            if step % 5 == 0 {
                view.rotate_about(hane_geom::Point::new(640.0, 360.0), 0.31);
            }
            for margin in [0.0, 128.0] {
                let area = view.visible_bounds(SCREEN, margin);
                assert_eq!(
                    cull(&tree, &view, margin),
                    brute(&items, area),
                    "step {step}"
                );
            }
        }
    }

    #[test]
    fn an_unrotated_viewport_maps_to_its_own_corners() {
        let mut view = View::new();
        view.pan_by(Vec2::new(-100.0, -50.0));
        view.zoom_about(hane_geom::Point::ORIGIN, 2.0);
        // Zooming about the origin re-anchors the pan to (-200,-100), so the
        // screen box (0,0)..(1280,720) is document (100,50)..(740,410). Exact:
        // every operand is a power of two or an integer, so there is nothing
        // here for rounding to eat.
        let b = view.visible_bounds(SCREEN, 0.0);
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (100.0, 50.0, 740.0, 410.0));
        // The margin is screen pixels, so at zoom 2 it is half that many
        // document units -- the point of measuring it on the screen side.
        let m = view.visible_bounds(SCREEN, 100.0);
        assert_eq!((m.x0, m.y0, m.x1, m.y1), (50.0, 0.0, 790.0, 460.0));
    }

    #[test]
    fn rotation_grows_the_box_to_the_bound_of_the_turned_viewport() {
        let mut view = View::new();
        let square = Rect::new(0.0, 0.0, 100.0, 100.0);
        let upright = view.visible_bounds(square, 0.0);
        view.rotate_about(hane_geom::Point::ORIGIN, core::f64::consts::FRAC_PI_4);
        let turned = view.visible_bounds(square, 0.0);
        // A square turned 45 degrees has a bounding box sqrt(2) times as wide.
        assert!((turned.width() - upright.width() * 2f64.sqrt()).abs() < 1e-9);
        assert!((turned.height() - upright.height() * 2f64.sqrt()).abs() < 1e-9);
        // And it must still contain every corner of the viewport, which is the
        // property that stops items being culled that are actually on screen.
        for corner in [(0.0, 0.0), (100.0, 0.0), (0.0, 100.0), (100.0, 100.0)] {
            let doc = view.to_document(hane_geom::Point::new(corner.0, corner.1));
            assert!(turned.inflate(1e-9).contains(doc), "{corner:?}");
        }
    }

    #[test]
    fn a_degenerate_viewport_culls_everything_rather_than_nothing() {
        // Without the `is_empty` guard this returns the whole plane: the
        // inverted infinities map to a mix of infinities and NaNs whose union
        // is unbounded, so a canvas with no area would draw the entire
        // document instead of nothing.
        let mut view = View::new();
        view.rotate_about(hane_geom::Point::new(3.0, 4.0), 0.9);
        let nan = f64::NAN;
        for screen in [
            Rect::EMPTY,
            Rect::ZERO,
            Rect::new(nan, nan, nan, nan),
            Rect::new(0.0, 0.0, 1280.0, 720.0),
        ] {
            // The last one is a real viewport, shrunk away by the margin.
            let b = view.visible_bounds(screen, -1000.0);
            assert!(!b.overlaps(Rect::new(-1e9, -1e9, 1e9, 1e9)), "{b:?}");
        }
    }
}
