//! The pen tool: click for a corner, drag for a smooth node, click the first
//! node to close.
//!
//! The tool owns nothing but the nodes placed so far and where the cursor is.
//! It touches no [`Document`](crate::document::Document): a path being drawn is
//! not yet an object, so nothing about it belongs in the undo log until it is
//! finished and inserted, which is one undo step rather than one per click.
//!
//! Handle symmetry is not implemented here. Dragging out a handle makes the
//! node [`Symmetric`](NodeType::Symmetric) and alt makes it a
//! [`Corner`](NodeType::Corner); [`Node::set_handle`] does the rest, so the pen
//! and the node tool cannot disagree about what a smooth node is.

use crate::document::document_length;
use crate::node::{Node, NodeType, Nodes, Side};
use hane_geom::Point;
use hane_path::Path;
use hane_scene::View;

/// A path being drawn.
#[derive(Clone, Debug, Default)]
pub struct Pen {
    nodes: Vec<Node>,
    /// Where the cursor last was, in document space: the pending segment runs
    /// to here. `None` before the first press.
    cursor: Option<Point>,
    /// Whether the button is down, and so whether a move shapes the last
    /// node's handles instead of moving the pending segment.
    dragging: bool,
}

impl Pen {
    /// A pen with nothing drawn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a path is being drawn.
    #[must_use]
    pub fn is_drawing(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// The nodes placed so far.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Pointer down at `screen`.
    ///
    /// Returns the finished outline when the click lands within `close_px` of
    /// the first node, which closes the path and ends the drawing.
    ///
    /// ponytail: closing ends the path immediately, so a drag on the closing
    /// click does not shape the first node's incoming handle. Doing that needs
    /// the shape to exist first; until then the node tool adjusts it.
    pub fn press(&mut self, view: &View, screen: Point, close_px: f64) -> Option<Path> {
        let point = view.to_document(screen);
        if self.nodes.len() >= 2
            && point.distance(self.nodes[0].point) <= document_length(view, close_px)
        {
            let nodes = std::mem::take(&mut self.nodes);
            self.cursor = None;
            self.dragging = false;
            return Some(Nodes::subpath(nodes, true).to_path());
        }
        self.nodes.push(Node::corner(point));
        self.cursor = Some(point);
        self.dragging = true;
        None
    }

    /// Pointer moved with the button down: shapes the handles of the node just
    /// placed.
    ///
    /// `alt` breaks symmetry -- the node becomes a corner and the handle
    /// already on the other side stays where it is, which is what lets one
    /// gesture end a curve and start a straight line.
    pub fn drag(&mut self, view: &View, screen: Point, alt: bool) {
        if !self.dragging {
            return;
        }
        let point = view.to_document(screen);
        self.cursor = Some(point);
        if let Some(node) = self.nodes.last_mut() {
            node.kind = if alt {
                NodeType::Corner
            } else {
                NodeType::Symmetric
            };
            // The drag pulls the *outgoing* handle; the incoming one is
            // whatever this node's kind says it is.
            node.set_handle(Side::Forward, point);
        }
    }

    /// Pointer up: the node just placed is finished.
    pub fn release(&mut self) {
        self.dragging = false;
    }

    /// Pointer moved with the button up: the pending segment follows it.
    pub fn hover(&mut self, view: &View, screen: Point) {
        if self.is_drawing() {
            self.cursor = Some(view.to_document(screen));
        }
    }

    /// The path drawn so far, with the pending segment to the cursor.
    ///
    /// This is what the tool draws every frame, so it is a whole path and not
    /// a diff: an outline being drawn is a few nodes, and rebuilding it costs
    /// less than tracking which part of it changed.
    #[must_use]
    pub fn preview(&self) -> Path {
        let mut nodes = self.nodes.clone();
        // While dragging, the cursor is the handle being pulled and is already
        // on the path; there is no pending segment to draw.
        if let Some(cursor) = self.cursor.filter(|_| !self.dragging && !nodes.is_empty()) {
            nodes.push(Node::corner(cursor));
        }
        Nodes::subpath(nodes, false).to_path()
    }

    /// Ends an open path -- escape or enter -- returning what was drawn.
    ///
    /// `None` for a path of fewer than two nodes: a single click that ends
    /// there draws nothing, and inserting an invisible object for it is worse
    /// than dropping it.
    pub fn finish(&mut self) -> Option<Path> {
        let nodes = std::mem::take(&mut self.nodes);
        self.cursor = None;
        self.dragging = false;
        (nodes.len() >= 2).then(|| Nodes::subpath(nodes, false).to_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeTarget;
    use hane_geom::{Affine, PathEl};

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    /// A view at 2x zoom, so nothing here can pass by treating screen pixels
    /// as document units.
    fn view() -> View {
        let mut view = View::new();
        view.zoom_about(Point::ORIGIN, 2.0);
        view
    }

    /// Clicks at a document point, converting to screen the way the caller
    /// would.
    fn click(pen: &mut Pen, view: &View, doc: Point) -> Option<Path> {
        pen.press(view, view.to_screen(doc), 8.0)
    }

    /// A whole click that places a node rather than closing the path.
    fn tap(pen: &mut Pen, view: &View, doc: Point) {
        assert!(click(pen, view, doc).is_none(), "unexpectedly closed");
        pen.release();
    }

    #[test]
    fn clicks_place_corners_and_enter_ends_the_path() {
        let view = view();
        let mut pen = Pen::new();
        assert!(!pen.is_drawing());
        for q in [p(0.0, 0.0), p(10.0, 0.0), p(10.0, 10.0)] {
            tap(&mut pen, &view, q);
        }
        assert!(pen.is_drawing());
        let path = pen.finish().unwrap();
        assert_eq!(
            path.elements(),
            &[
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::LineTo(p(10.0, 0.0)),
                PathEl::LineTo(p(10.0, 10.0)),
            ]
        );
        assert!(!pen.is_drawing());
        assert!(pen.finish().is_none(), "nothing left to finish");
    }

    #[test]
    fn a_drag_places_a_smooth_node_with_mirrored_handles() {
        let view = view();
        let mut pen = Pen::new();
        assert!(click(&mut pen, &view, p(0.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(3.0, 0.0)), false);
        pen.release();
        assert!(click(&mut pen, &view, p(10.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(13.0, 4.0)), false);
        pen.release();

        let node = pen.nodes()[1];
        assert_eq!(node.kind, NodeType::Symmetric);
        assert_eq!(node.forward, Some(p(13.0, 4.0)));
        assert_eq!(node.back, Some(p(7.0, -4.0)), "mirrored about the anchor");

        let path = pen.finish().unwrap();
        assert_eq!(
            path.elements(),
            &[
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::CurveTo(p(3.0, 0.0), p(7.0, -4.0), p(10.0, 0.0)),
            ]
        );
    }

    #[test]
    fn alt_breaks_the_symmetry_while_drawing() {
        let view = view();
        let mut pen = Pen::new();
        tap(&mut pen, &view, p(0.0, 0.0));
        assert!(click(&mut pen, &view, p(10.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(13.0, 4.0)), false);
        let mirrored = pen.nodes()[1].back;
        // Alt part-way through the same drag: the handle behind stops
        // following, which is the whole point -- the curve already drawn keeps
        // its shape while the next one leaves in a new direction.
        pen.drag(&view, view.to_screen(p(11.0, 9.0)), true);
        let node = pen.nodes()[1];
        assert_eq!(node.kind, NodeType::Corner);
        assert_eq!(node.forward, Some(p(11.0, 9.0)));
        assert_eq!(node.back, mirrored, "the far handle stayed put");
    }

    #[test]
    fn clicking_the_first_node_closes_the_path() {
        let view = view();
        let mut pen = Pen::new();
        tap(&mut pen, &view, p(0.0, 0.0));
        tap(&mut pen, &view, p(10.0, 0.0));
        tap(&mut pen, &view, p(10.0, 10.0));
        // Not exactly on the first node: within the grab radius, which is
        // eight screen pixels and so four document units at this zoom.
        let path = click(&mut pen, &view, p(1.5, 1.5)).expect("closed");
        assert_eq!(path.elements().last(), Some(&PathEl::ClosePath));
        assert_eq!(path.elements().len(), 4, "no stray node at the click");
        assert!(!pen.is_drawing());
    }

    #[test]
    fn the_close_radius_is_screen_pixels_not_document_units() {
        let mut view = View::new();
        view.zoom_about(Point::ORIGIN, 100.0);
        let mut pen = Pen::new();
        tap(&mut pen, &view, p(0.0, 0.0));
        tap(&mut pen, &view, p(10.0, 0.0));
        // 1.5 document units is 150 pixels away at this zoom: a click there is
        // a new node, not a close.
        tap(&mut pen, &view, p(1.5, 0.0));
        assert!(pen.is_drawing());
        assert!(click(&mut pen, &view, p(0.04, 0.0)).is_some());
    }

    #[test]
    fn the_pending_segment_follows_the_cursor() {
        let view = view();
        let mut pen = Pen::new();
        assert!(pen.preview().elements().is_empty(), "nothing drawn yet");
        tap(&mut pen, &view, p(0.0, 0.0));
        pen.hover(&view, view.to_screen(p(5.0, 5.0)));
        assert_eq!(
            pen.preview().elements(),
            &[PathEl::MoveTo(p(0.0, 0.0)), PathEl::LineTo(p(5.0, 5.0))]
        );
        pen.hover(&view, view.to_screen(p(6.0, 5.0)));
        assert_eq!(
            pen.preview().elements().last(),
            Some(&PathEl::LineTo(p(6.0, 5.0)))
        );
        // The preview never adds a node: the pending segment is drawn, not
        // committed.
        assert_eq!(pen.nodes().len(), 1);

        // Mid-drag the cursor is the handle, so the preview shows the curve
        // being shaped rather than a segment to the pointer.
        assert!(click(&mut pen, &view, p(10.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(14.0, 0.0)), false);
        assert_eq!(pen.preview().elements().len(), 2);
    }

    #[test]
    fn what_the_pen_draws_is_what_the_node_tool_reads_back() {
        let view = view();
        let mut pen = Pen::new();
        assert!(click(&mut pen, &view, p(0.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(2.0, -3.0)), false);
        pen.release();
        assert!(click(&mut pen, &view, p(10.0, 0.0)).is_none());
        pen.drag(&view, view.to_screen(p(12.0, 3.0)), false);
        pen.release();
        let path = click(&mut pen, &view, p(0.0, 0.0)).expect("closed");

        let nodes = Nodes::from_path(&path);
        assert_eq!(nodes.len(), 2);
        // The kinds are inferred, not stored, so this is the check that the
        // pen's idea of a smooth node survives being written to a path.
        assert_eq!(nodes.nodes()[0].kind, NodeType::Symmetric);
        assert_eq!(nodes.nodes()[1].kind, NodeType::Symmetric);
        assert_eq!(
            crate::node::node_hit(
                &nodes,
                Affine::IDENTITY,
                &view,
                view.to_screen(p(2.0, -3.0)),
                8.0
            ),
            Some(NodeTarget::Handle(0, Side::Forward))
        );
    }
}
