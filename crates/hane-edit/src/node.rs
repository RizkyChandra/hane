//! Path nodes: the node-and-handle view of a [`Path`], the edits a node tool
//! makes to one, and the undoable command that installs the result.
//!
//! # Why a second representation
//!
//! [`PathEl`] stores what SVG stores, which is what a *renderer* needs. A node
//! tool needs the other decomposition: an anchor with the two control points
//! either side of it, because a smooth node's two handles belong to different
//! elements and have to move together. Deriving that view on demand and
//! writing it back is what [`Nodes::from_path`] and [`Nodes::to_path`] do; the
//! tool keeps one `Nodes` alive for the shape it is editing, because the node
//! *kinds* below exist nowhere in the path itself.
//!
//! # The symmetry rule lives in one place
//!
//! [`Node::set_handle`] is the only code that decides where the opposite
//! handle goes when a handle moves. The pen tool's alt-to-break-symmetry is
//! not a second rule: alt makes the node a [`NodeType::Corner`], and a corner's
//! handles are independent by that same function.
//!
//! # Quads become cubics
//!
//! A node holds one handle per side, and a [`QuadTo`](PathEl::QuadTo)'s single
//! control point belongs to both, so quads are degree-elevated on the way in.
//! The elevation is exact in exact arithmetic and off by a rounding step in
//! f64, so a path is only rewritten if it is actually edited -- reading nodes
//! for a hit test changes nothing.

use crate::document::{Document, document_length};
use crate::undo::Command;
use hane_geom::{Affine, CubicBez, PathEl, Point, QuadBez, Vec2};
use hane_path::Path;
use hane_scene::{NodeId, View};

/// How a node's two handles are tied together.
///
/// Not stored in a [`Path`] -- SVG has no such concept -- so
/// [`Nodes::from_path`] infers it from the geometry it finds, and a node whose
/// handles happen to be collinear comes back as [`Smooth`](NodeType::Smooth)
/// whoever drew it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeType {
    /// Handles move independently; the path may kink here.
    #[default]
    Corner,
    /// Handles stay opposite each other, keeping their own lengths.
    Smooth,
    /// Handles stay opposite each other and the same length: a mirror.
    Symmetric,
}

/// Which of a node's two handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The control point of the segment arriving at the node.
    Back,
    /// The control point of the segment leaving the node.
    Forward,
}

impl Side {
    /// The other side.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Back => Self::Forward,
            Self::Forward => Self::Back,
        }
    }
}

/// One anchor point and the handles either side of it.
///
/// A handle is an absolute point, not an offset: that is what the path stores,
/// so writing back is a copy rather than an addition that would round. `None`
/// means the handle sits on the anchor, which is what makes the adjoining
/// segment a straight line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Node {
    /// Where the curve passes through.
    pub point: Point,
    /// Control point of the segment arriving here.
    pub back: Option<Point>,
    /// Control point of the segment leaving here.
    pub forward: Option<Point>,
    /// How the two handles are tied together.
    pub kind: NodeType,
}

impl Node {
    /// A node with no handles: the path corners here.
    #[must_use]
    pub fn corner(point: Point) -> Self {
        Self {
            point,
            back: None,
            forward: None,
            kind: NodeType::Corner,
        }
    }

    /// The handle on `side`, defaulting to the anchor when there is none.
    #[must_use]
    pub fn handle(&self, side: Side) -> Point {
        match side {
            Side::Back => self.back,
            Side::Forward => self.forward,
        }
        .unwrap_or(self.point)
    }

    /// Moves the handle on `side` to `to`, dragging the opposite one along as
    /// [`kind`](Node::kind) requires.
    ///
    /// This is the whole of handle symmetry, for the pen tool and the node tool
    /// alike. A drag that puts a handle exactly on the anchor leaves the
    /// opposite one alone rather than collapsing it too: a zero-length handle
    /// has no direction to mirror, and the alternative loses the other handle's
    /// length for good.
    pub fn set_handle(&mut self, side: Side, to: Point) {
        let other = self.handle(side.opposite());
        let d = to - self.point;
        let opposite = match self.kind {
            NodeType::Corner => other,
            // A reflection, so a second drag of the same handle returns the
            // pair to where it was rather than drifting.
            NodeType::Symmetric => self.point - d,
            NodeType::Smooth if d.length_squared() == 0.0 => other,
            NodeType::Smooth => self.point - d.normalize() * (other - self.point).length(),
        };
        self.set(side, to);
        self.set(side.opposite(), opposite);
    }

    /// Writes one handle with no symmetry applied, dropping it when it lands
    /// on the anchor so the segment becomes a line again.
    fn set(&mut self, side: Side, to: Point) {
        let handle = (to != self.point).then_some(to);
        match side {
            Side::Back => self.back = handle,
            Side::Forward => self.forward = handle,
        }
    }

    /// The kind the geometry implies, used when reading a path back in.
    fn inferred(&self) -> NodeType {
        let (Some(back), Some(forward)) = (self.back, self.forward) else {
            return NodeType::Corner;
        };
        let (a, b) = (back - self.point, forward - self.point);
        let (la, lb) = (a.length(), b.length());
        // Relative on both tests: coordinates reach 1e9 in this engine, where
        // an absolute epsilon is below an ulp and nothing is ever collinear.
        if a.cross(b).abs() > 1e-9 * la * lb || a.dot(b) >= 0.0 {
            NodeType::Corner
        } else if (la - lb).abs() <= 1e-9 * (la + lb) {
            NodeType::Symmetric
        } else {
            NodeType::Smooth
        }
    }

    /// Demotes a symmetric node to smooth, for the edits that change one
    /// handle's length without touching the other.
    fn relax(&mut self) {
        if self.kind == NodeType::Symmetric {
            self.kind = NodeType::Smooth;
        }
    }
}

/// A path seen as nodes: the working representation of the node tool.
///
/// Subpaths are kept flat in one vector so a node index addresses the whole
/// path, which is what a multi-node selection wants to hold.
#[derive(Clone, Debug, Default)]
pub struct Nodes {
    nodes: Vec<Node>,
    /// The first node of each subpath, and whether that subpath closes.
    /// Strictly increasing, and non-empty whenever `nodes` is.
    starts: Vec<(usize, bool)>,
}

impl Nodes {
    /// One subpath made of `nodes`, closed or not.
    #[must_use]
    pub fn subpath(nodes: Vec<Node>, closed: bool) -> Self {
        let starts = if nodes.is_empty() {
            Vec::new()
        } else {
            vec![(0, closed)]
        };
        Self { nodes, starts }
    }

    /// The nodes of every subpath, in path order.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node-and-handle view of `path`.
    ///
    /// A closed subpath whose last element lands back on the start point folds
    /// that duplicate into the first node, so the two handles that meet there
    /// belong to one node and stay tied -- without it, closing a path would
    /// leave a permanent kink no edit could remove.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let mut out = Self::default();
        for subpath in path.subpaths() {
            let start = out.nodes.len();
            let mut closed = false;
            out.nodes.push(Node::corner(subpath.start()));
            for &el in subpath.elements() {
                let last = out.nodes.len() - 1;
                match el {
                    PathEl::MoveTo(p) | PathEl::LineTo(p) => out.nodes.push(Node::corner(p)),
                    PathEl::QuadTo(c, p) => {
                        let c = QuadBez::new(out.nodes[last].point, c, p).to_cubic();
                        out.nodes[last].set(Side::Forward, c.p1);
                        let mut node = Node::corner(p);
                        node.set(Side::Back, c.p2);
                        out.nodes.push(node);
                    }
                    PathEl::CurveTo(c1, c2, p) => {
                        out.nodes[last].set(Side::Forward, c1);
                        let mut node = Node::corner(p);
                        node.set(Side::Back, c2);
                        out.nodes.push(node);
                    }
                    PathEl::ClosePath => {
                        closed = true;
                        let last = out.nodes.len() - 1;
                        if last > start && out.nodes[last].point == out.nodes[start].point {
                            out.nodes[start].back = out.nodes[last].back;
                            out.nodes.pop();
                        }
                    }
                }
            }
            out.starts.push((start, closed));
        }
        for node in &mut out.nodes {
            node.kind = node.inferred();
        }
        out
    }

    /// These nodes as a path.
    ///
    /// Round-trips with [`from_path`](Nodes::from_path) for any path already in
    /// this form: lines, cubics, and a close that either returns to the start
    /// point or is left to [`ClosePath`](PathEl::ClosePath) to draw.
    #[must_use]
    pub fn to_path(&self) -> Path {
        let mut els = Vec::with_capacity(self.nodes.len() + self.starts.len());
        for (k, &(start, closed)) in self.starts.iter().enumerate() {
            let end = self.end_of(k);
            els.push(PathEl::MoveTo(self.nodes[start].point));
            for i in start..end.saturating_sub(1) {
                els.push(element(&self.nodes[i], &self.nodes[i + 1]));
            }
            if closed {
                let (a, b) = (&self.nodes[end - 1], &self.nodes[start]);
                // A closing line is what `ClosePath` already draws, so emitting
                // it as well would leave a zero-length segment behind on every
                // round trip.
                if a.forward.is_some() || b.back.is_some() {
                    els.push(element(a, b));
                }
                els.push(PathEl::ClosePath);
            }
        }
        Path::from(els)
    }

    /// Moves node `i` to `to` exactly, taking its handles with it.
    pub fn move_node(&mut self, i: usize, to: Point) {
        let Some(node) = self.nodes.get_mut(i) else {
            return;
        };
        // The anchor is assigned, not offset: `p + (to - p)` misses `to` by a
        // rounding step once the magnitudes differ, and a node dragged onto a
        // snap target has to land on it exactly.
        let delta = to - node.point;
        node.point = to;
        node.back = node.back.map(|h| h + delta);
        node.forward = node.forward.map(|h| h + delta);
    }

    /// Moves every node in `indices` by `delta`, handles included.
    ///
    /// This is a multi-node drag, so it is a translation and not a set of
    /// destinations. Apply it to a copy of the nodes taken when the drag
    /// started -- never to the current ones -- or the gesture compounds.
    pub fn move_nodes(&mut self, indices: &[usize], delta: Vec2) {
        for &i in indices {
            if let Some(node) = self.nodes.get_mut(i) {
                node.point += delta;
                node.back = node.back.map(|h| h + delta);
                node.forward = node.forward.map(|h| h + delta);
            }
        }
    }

    /// Moves one handle of node `i`, obeying that node's symmetry.
    pub fn move_handle(&mut self, i: usize, side: Side, to: Point) {
        if let Some(node) = self.nodes.get_mut(i) {
            node.set_handle(side, to);
        }
    }

    /// Switches node `i` to `kind`, moving the handles to satisfy it.
    ///
    /// The forward handle is the reference, so switching to
    /// [`Symmetric`](NodeType::Symmetric) mirrors it onto the back one and the
    /// segment leaving the node is the one that keeps its shape.
    pub fn set_type(&mut self, i: usize, kind: NodeType) {
        let Some(node) = self.nodes.get_mut(i) else {
            return;
        };
        node.kind = kind;
        // Re-driving the forward handle through the rule is what enforces the
        // new kind; a corner needs no enforcement at all.
        if kind != NodeType::Corner {
            let reference = if node.forward.is_some() {
                Side::Forward
            } else {
                Side::Back
            };
            let to = node.handle(reference);
            node.set_handle(reference, to);
        }
    }

    /// Inserts a node on the segment leaving node `i`, at parameter `t`.
    ///
    /// The two halves are a [`split`](CubicBez::split), so the new node sits on
    /// the old curve and the geometry either side of it is unchanged: the
    /// shape does not move when a node is added to it.
    pub fn insert(&mut self, i: usize, t: f64) {
        let Some(j) = self.next_of(i) else {
            return;
        };
        let (a, b) = (self.nodes[i], self.nodes[j]);
        let mut node = match segment(&a, &b) {
            None => Node::corner(a.point.lerp(b.point, t)),
            Some(c) => {
                let (left, right) = c.split(t);
                self.nodes[i].set(Side::Forward, left.p1);
                self.nodes[i].relax();
                self.nodes[j].set(Side::Back, right.p2);
                self.nodes[j].relax();
                // `left.p3` and `right.p0` are the same bits, so the two
                // halves meet exactly on the new node.
                let mut node = Node::corner(left.p3);
                node.set(Side::Back, left.p2);
                node.set(Side::Forward, right.p1);
                node
            }
        };
        // De Casteljau leaves the split point between two collinear controls,
        // so this comes back smooth -- but read the geometry rather than
        // assert it, so a degenerate split is called what it is.
        node.kind = node.inferred();
        // `j` is `i + 1` except on the closing segment, where the new node
        // still belongs at the end of this subpath rather than at its start.
        let at = i + 1;
        self.nodes.insert(at, node);
        self.shift(at, 1);
    }

    /// Removes node `i`, fitting a segment through the gap it leaves.
    ///
    /// The replacement inverts a split at the midpoint: doubling each
    /// neighbour's handle recovers the control points a single curve would
    /// have had, exactly (to a rounding step) when the node being removed was
    /// inserted at `t = 0.5`, and sensibly otherwise -- the surviving curve
    /// keeps both neighbours' tangent directions. Two straight neighbours stay
    /// straight instead of bulging towards the vertex that was deleted.
    pub fn remove(&mut self, i: usize) {
        let Some((start, end, _)) = self.span(i) else {
            return;
        };
        let k = self.starts.partition_point(|&(s, _)| s <= i) - 1;
        if let (Some(p), Some(n)) = (self.prev_of(i), self.next_of(i))
            && p != n
        {
            let (a, b) = (self.nodes[p], self.nodes[n]);
            let straight = a.forward.is_none() && b.back.is_none();
            if !straight {
                let mid = self.nodes[i].point;
                self.nodes[p].set(Side::Forward, double(a.point, a.forward, mid));
                self.nodes[p].relax();
                self.nodes[n].set(Side::Back, double(b.point, b.back, mid));
                self.nodes[n].relax();
            }
        }
        self.nodes.remove(i);
        self.shift(i + 1, -1);
        // The subpath is gone, and an entry with no nodes would make `span`
        // hand out a range that is not there.
        if end - start == 1 {
            self.starts.remove(k);
        }
    }

    /// The nearest point on the outline to `p`, as the segment's start node,
    /// its parameter and the distance -- what "insert a node here" needs.
    #[must_use]
    pub fn nearest_segment(&self, p: Point) -> Option<(usize, f64, f64)> {
        let mut best: Option<(usize, f64, f64)> = None;
        for i in 0..self.nodes.len() {
            let Some(j) = self.next_of(i) else { continue };
            let (a, b) = (self.nodes[i], self.nodes[j]);
            let (t, d) = match segment(&a, &b) {
                Some(c) => c.nearest(p),
                None => {
                    let t = line_parameter(a.point, b.point, p);
                    (t, a.point.lerp(b.point, t).distance(p))
                }
            };
            if best.is_none_or(|(_, _, best_d)| d < best_d) {
                best = Some((i, t, d));
            }
        }
        best
    }

    /// The node after `i` in its subpath, wrapping only when that subpath is
    /// closed. `None` when no segment leaves `i`.
    #[must_use]
    pub fn next_of(&self, i: usize) -> Option<usize> {
        let (start, end, closed) = self.span(i)?;
        if i + 1 < end {
            Some(i + 1)
        } else if closed && end - start > 1 {
            Some(start)
        } else {
            None
        }
    }

    /// The node before `i` in its subpath, wrapping only when that subpath is
    /// closed.
    #[must_use]
    pub fn prev_of(&self, i: usize) -> Option<usize> {
        let (start, end, closed) = self.span(i)?;
        if i > start {
            Some(i - 1)
        } else if closed && end - start > 1 {
            Some(end - 1)
        } else {
            None
        }
    }

    /// The half-open node range of the subpath containing `i`, and whether it
    /// closes.
    fn span(&self, i: usize) -> Option<(usize, usize, bool)> {
        if i >= self.nodes.len() {
            return None;
        }
        let k = self.starts.partition_point(|&(s, _)| s <= i) - 1;
        Some((self.starts[k].0, self.end_of(k), self.starts[k].1))
    }

    /// One past the last node of subpath `k`.
    fn end_of(&self, k: usize) -> usize {
        self.starts.get(k + 1).map_or(self.nodes.len(), |&(s, _)| s)
    }

    /// Slides the subpath starts at or after `at` by `delta` nodes.
    fn shift(&mut self, at: usize, delta: isize) {
        for (s, _) in &mut self.starts {
            if *s >= at {
                *s = s.wrapping_add_signed(delta);
            }
        }
    }
}

/// The cubic between two nodes, or `None` when neither has a facing handle and
/// the segment is a straight line.
fn segment(a: &Node, b: &Node) -> Option<CubicBez> {
    (a.forward.is_some() || b.back.is_some()).then(|| {
        CubicBez::new(
            a.point,
            a.handle(Side::Forward),
            b.handle(Side::Back),
            b.point,
        )
    })
}

/// The path element drawing the segment from `a` to `b`.
fn element(a: &Node, b: &Node) -> PathEl {
    match segment(a, b) {
        None => PathEl::LineTo(b.point),
        Some(c) => PathEl::CurveTo(c.p1, c.p2, c.p3),
    }
}

/// The control point a neighbour needs after the node between it and `mid` is
/// deleted: its handle, doubled about the anchor.
///
/// A missing handle stands in as the straight segment's own cubic control, one
/// third of the way to `mid`, so the reconnected curve leaves the anchor in the
/// direction the line did.
fn double(anchor: Point, handle: Option<Point>, mid: Point) -> Point {
    let h = handle.unwrap_or_else(|| anchor.lerp(mid, 1.0 / 3.0));
    anchor + (h - anchor) * 2.0
}

/// Where `p` projects onto the segment `a`-`b`, clamped to it.
fn line_parameter(a: Point, b: Point, p: Point) -> f64 {
    let d = b - a;
    let len = d.length_squared();
    if len == 0.0 {
        0.0
    } else {
        ((p - a).dot(d) / len).clamp(0.0, 1.0)
    }
}

/// What the pointer is over in the node tool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NodeTarget {
    /// The anchor of a node.
    Node(usize),
    /// One of a node's handles.
    Handle(usize, Side),
    /// A point at parameter `.1` on the segment leaving node `.0` -- where a
    /// click inserts.
    Segment(usize, f64),
}

/// What `screen` is over in `nodes`, an outline placed by `transform`.
///
/// The nearest anchor or handle within the radius wins, an anchor breaking an
/// exact tie with a handle sitting on top of it. The outline is only offered
/// when neither is in reach: a click on it inserts a node, and doing that
/// because a grab missed by a pixel is the worst outcome of the three.
///
/// The single screen-to-document conversion for the node tool. The grab radius
/// arrives in pixels and is divided by the shape's scale on the way into shape
/// space, so a handle stays the same size on screen however the shape is
/// transformed or the view zoomed.
#[must_use]
pub fn node_hit(
    nodes: &Nodes,
    transform: Affine,
    view: &View,
    screen: Point,
    radius_px: f64,
) -> Option<NodeTarget> {
    let inverse = transform.inverse()?;
    let point = inverse * view.to_document(screen);
    let scale = transform.max_scale();
    if scale == 0.0 {
        return None;
    }
    let radius = document_length(view, radius_px) / scale;

    /// Keeps the nearest candidate within `radius`.
    fn consider(d: f64, radius: f64, target: NodeTarget, best: &mut Option<(f64, NodeTarget)>) {
        if d <= radius && best.is_none_or(|(bd, _)| d < bd) {
            *best = Some((d, target));
        }
    }

    let mut best: Option<(f64, NodeTarget)> = None;
    for (i, node) in nodes.nodes().iter().enumerate() {
        // The anchor first, so it keeps the tie against a handle on top of it.
        consider(
            node.point.distance(point),
            radius,
            NodeTarget::Node(i),
            &mut best,
        );
        for side in [Side::Back, Side::Forward] {
            let handle = match side {
                Side::Back => node.back,
                Side::Forward => node.forward,
            };
            if let Some(h) = handle {
                consider(
                    h.distance(point),
                    radius,
                    NodeTarget::Handle(i, side),
                    &mut best,
                );
            }
        }
    }
    if let Some((_, target)) = best {
        return Some(target);
    }
    let (i, t, d) = nodes.nearest_segment(point)?;
    (d <= radius).then_some(NodeTarget::Segment(i, t))
}

/// An undoable replacement of one shape's outline.
///
/// The whole outline, not a node index and an offset. Two reasons: every edit
/// above rebuilds the path anyway, so a command holding the previous path costs
/// the same order as the edit that produced it; and putting a path back
/// verbatim is exact, where re-deriving one from a delta is not -- the undo
/// fuzz property in [`crate::undo`] fails a delta at large coordinates.
pub struct EditPath {
    id: NodeId,
    /// The outline to install.
    after: Path,
    /// What was there before, captured by the first `apply`. `None` for a
    /// shape that has been deleted meanwhile, which `revert` then leaves alone.
    before: Option<Path>,
}

impl EditPath {
    /// The command installing `path` as the outline of `id`.
    #[must_use]
    pub fn new(id: NodeId, path: Path) -> Self {
        Self {
            id,
            after: path,
            before: None,
        }
    }
}

impl Command for EditPath {
    type Doc = Document;

    fn apply(&mut self, doc: &mut Document) {
        self.before = doc.set_path(self.id, self.after.clone());
    }

    fn revert(&mut self, doc: &mut Document) {
        if let Some(before) = self.before.take() {
            doc.set_path(self.id, before);
        }
    }

    fn merge(&mut self, next: Self) -> Option<Self> {
        // The drag case: every frame rebuilds the whole outline from the nodes
        // captured when the drag began, so the newest one already describes the
        // whole gesture and `self.before` still holds what it started from.
        if self.id == next.id && self.before.is_some() {
            self.after = next.after;
            return None;
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::{rect_path, rect_shape};
    use crate::undo::UndoLog;
    use hane_geom::Rect;
    use hane_geom::fuzz::{Rng, check};

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    /// A closed square with one curved side, the fixture for most of these.
    fn fixture() -> Path {
        Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::CurveTo(p(1.0, -1.0), p(3.0, -1.0), p(4.0, 0.0)),
            PathEl::LineTo(p(4.0, 4.0)),
            PathEl::LineTo(p(0.0, 4.0)),
            PathEl::ClosePath,
        ])
    }

    /// Bit-exact comparison: two paths that flatten alike but store different
    /// numbers are not the same path here.
    fn bits(path: &Path) -> Vec<u64> {
        let mut out = Vec::new();
        let mut push = |tag: u64, points: &[Point]| {
            // The tag as well as the points: two paths differing only in
            // which element type carries a point are not the same path.
            out.push(tag);
            for q in points {
                out.push(q.x.to_bits());
                out.push(q.y.to_bits());
            }
        };
        for &el in path.elements() {
            match el {
                PathEl::MoveTo(a) => push(0, &[a]),
                PathEl::LineTo(a) => push(1, &[a]),
                PathEl::QuadTo(a, b) => push(2, &[a, b]),
                PathEl::CurveTo(a, b, c) => push(3, &[a, b, c]),
                PathEl::ClosePath => push(4, &[]),
            }
        }
        out
    }

    /// Points along the outline, for "the shape did not move" comparisons.
    fn sampled(path: &Path) -> Vec<Point> {
        path.segments()
            .flat_map(|s| {
                (0..=8).map(move |k| {
                    let t = f64::from(k) / 8.0;
                    match s {
                        hane_path::Segment::Line(a, b) => a.lerp(b, t),
                        hane_path::Segment::Quad(q) => q.eval(t),
                        hane_path::Segment::Cubic(c) => c.eval(t),
                    }
                })
            })
            .collect()
    }

    #[test]
    fn a_path_round_trips_through_nodes() {
        let path = fixture();
        let nodes = Nodes::from_path(&path);
        // Four corners, the duplicate close point folded into the first node.
        assert_eq!(nodes.len(), 4);
        assert_eq!(bits(&nodes.to_path()), bits(&path));

        let open = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 0.0)),
            PathEl::CurveTo(p(2.0, 0.0), p(3.0, 1.0), p(3.0, 2.0)),
        ]);
        assert_eq!(bits(&Nodes::from_path(&open).to_path()), bits(&open));
        assert_eq!(
            bits(&Nodes::from_path(&rect_path(Rect::new(0.0, 0.0, 1.0, 1.0))).to_path()),
            bits(&rect_path(Rect::new(0.0, 0.0, 1.0, 1.0)))
        );
    }

    #[test]
    fn a_closing_curve_ties_the_first_node_together() {
        let path = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::CurveTo(p(1.0, 1.0), p(3.0, 1.0), p(4.0, 0.0)),
            PathEl::CurveTo(p(5.0, -1.0), p(-1.0, -1.0), p(0.0, 0.0)),
            PathEl::ClosePath,
        ]);
        let nodes = Nodes::from_path(&path);
        assert_eq!(nodes.len(), 2);
        // The node at the start point owns both halves of the join, which is
        // what makes it draggable as one smooth node.
        assert_eq!(nodes.nodes()[0].back, Some(p(-1.0, -1.0)));
        assert_eq!(nodes.nodes()[0].forward, Some(p(1.0, 1.0)));
        assert_eq!(nodes.nodes()[0].kind, NodeType::Symmetric);
        assert_eq!(bits(&nodes.to_path()), bits(&path));
    }

    #[test]
    fn the_three_node_types_tie_the_handles_differently() {
        let mut node = Node::corner(p(0.0, 0.0));
        node.back = Some(p(-2.0, 0.0));
        node.forward = Some(p(1.0, 0.0));

        node.kind = NodeType::Corner;
        node.set_handle(Side::Forward, p(0.0, 3.0));
        assert_eq!(node.back, Some(p(-2.0, 0.0)), "a corner is independent");

        node.kind = NodeType::Smooth;
        node.set_handle(Side::Forward, p(3.0, 0.0));
        assert_eq!(node.forward, Some(p(3.0, 0.0)));
        // Opposite direction, its own length kept.
        assert_eq!(node.back, Some(p(-2.0, 0.0)));

        node.kind = NodeType::Symmetric;
        node.set_handle(Side::Forward, p(0.0, 5.0));
        assert_eq!(node.back, Some(p(0.0, -5.0)), "symmetric mirrors exactly");

        // Dragging the back handle drives the forward one just the same.
        node.set_handle(Side::Back, p(-7.0, 0.0));
        assert_eq!(node.forward, Some(p(7.0, 0.0)));
    }

    #[test]
    fn switching_type_straightens_the_handles() {
        let mut nodes = Nodes::from_path(&fixture());
        nodes.move_handle(0, Side::Back, p(-1.0, 2.0));
        nodes.set_type(0, NodeType::Symmetric);
        let node = nodes.nodes()[0];
        let (a, b) = (
            node.back.unwrap() - node.point,
            node.forward.unwrap() - node.point,
        );
        assert!(a.cross(b).abs() < 1e-12, "collinear");
        assert!((a.length() - b.length()).abs() < 1e-12, "equal length");
        assert_eq!(node.forward, Some(p(1.0, -1.0)), "the forward handle led");

        nodes.set_type(0, NodeType::Corner);
        nodes.move_handle(0, Side::Forward, p(2.0, 2.0));
        assert_eq!(nodes.nodes()[0].back, Some(p(-1.0, 1.0)), "corner frees it");
    }

    #[test]
    fn inserting_a_node_does_not_move_the_outline() {
        let path = fixture();
        let before = sampled(&path);
        let mut nodes = Nodes::from_path(&path);
        nodes.insert(0, 0.375);
        assert_eq!(nodes.len(), 5);
        let after = sampled(&nodes.to_path());
        // Not the same sample points -- the parameterisation is split -- so
        // compare the curve the samples lie on by round-tripping the extremes.
        assert_eq!(before.first(), after.first());
        assert_eq!(before.last(), after.last());
        let inserted = nodes.nodes()[1].point;
        let (_, _, d) = Nodes::from_path(&path).nearest_segment(inserted).unwrap();
        assert!(d < 1e-12, "the new node sits on the old curve: {d}");
    }

    #[test]
    fn inserting_on_a_line_keeps_it_straight() {
        let mut nodes = Nodes::from_path(&fixture());
        nodes.insert(1, 0.25);
        assert_eq!(nodes.nodes()[2].point, p(4.0, 1.0));
        assert!(nodes.nodes()[2].forward.is_none());
        assert!(matches!(nodes.to_path().elements()[2], PathEl::LineTo(_)));
    }

    #[test]
    fn deleting_a_node_reconnects_the_neighbours() {
        // A node inserted at the midpoint and deleted again must give the
        // curve back: that is what "fit a segment through the gap" means when
        // there is a right answer to compare against.
        let path = fixture();
        let mut nodes = Nodes::from_path(&path);
        nodes.insert(0, 0.5);
        nodes.remove(1);
        let (a, b) = (nodes.to_path(), path);
        for (x, y) in sampled(&a).iter().zip(sampled(&b)) {
            assert!(x.distance(y) < 1e-12, "{x:?} vs {y:?}");
        }
        assert_eq!(nodes.len(), 4);
    }

    #[test]
    fn deleting_a_corner_between_two_lines_leaves_a_line() {
        let mut nodes = Nodes::from_path(&fixture());
        nodes.remove(2);
        assert_eq!(nodes.len(), 3);
        let els = nodes.to_path().elements().to_vec();
        assert!(
            matches!(els[2], PathEl::LineTo(q) if q == p(0.0, 4.0)),
            "{els:?}"
        );
    }

    #[test]
    fn deleting_an_end_node_just_drops_the_segment() {
        let open = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 0.0)),
            PathEl::LineTo(p(2.0, 0.0)),
        ]);
        let mut nodes = Nodes::from_path(&open);
        nodes.remove(2);
        assert_eq!(nodes.len(), 2);
        nodes.remove(0);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes.to_path().elements(), &[PathEl::MoveTo(p(1.0, 0.0))]);
        // And the last one takes the subpath with it: an entry left behind
        // would describe a range of nodes that are not there.
        nodes.remove(0);
        assert!(nodes.is_empty());
        assert!(nodes.to_path().elements().is_empty());
        nodes.remove(0);
    }

    #[test]
    fn a_multi_node_selection_moves_together() {
        let mut nodes = Nodes::from_path(&fixture());
        let start = nodes.clone();
        nodes.move_nodes(&[0, 1], Vec2::new(10.0, 0.0));
        assert_eq!(nodes.nodes()[0].point, p(10.0, 0.0));
        assert_eq!(nodes.nodes()[1].point, p(14.0, 0.0));
        assert_eq!(nodes.nodes()[2].point, start.nodes()[2].point);
        // Handles come along, or the curve would shear rather than translate.
        assert_eq!(nodes.nodes()[0].forward, Some(p(11.0, -1.0)));
    }

    #[test]
    fn the_pointer_finds_nodes_then_handles_then_the_outline() {
        let nodes = Nodes::from_path(&fixture());
        let mut view = View::new();
        // Eight screen pixels is one document unit here, so the fixture is
        // four grab radii across rather than half of one.
        view.zoom_about(Point::ORIGIN, 8.0);
        let at = |q: Point| node_hit(&nodes, Affine::IDENTITY, &view, view.to_screen(q), 8.0);
        assert_eq!(at(p(0.1, 0.1)), Some(NodeTarget::Node(0)));
        assert_eq!(at(p(1.0, -1.0)), Some(NodeTarget::Handle(0, Side::Forward)));
        assert!(matches!(at(p(4.0, 2.0)), Some(NodeTarget::Segment(1, _))));
        assert_eq!(at(p(100.0, 100.0)), None);

        // A transformed shape: the grab radius is screen pixels, so scaling
        // the shape up must not make its nodes harder to hit.
        let ten = Affine::scale(10.0);
        assert_eq!(
            node_hit(&nodes, ten, &view, view.to_screen(p(40.05, 0.05)), 8.0),
            Some(NodeTarget::Node(1))
        );
    }

    #[test]
    fn a_node_drag_is_one_undo_step_and_reverts_exactly() {
        let mut doc = Document::new(Rect::new(-100.0, -100.0, 100.0, 100.0));
        let id = doc.insert(rect_shape(Rect::new(0.0, 0.0, 10.0, 10.0)));
        let before = bits(&doc.get(id).unwrap().path);
        let start = Nodes::from_path(&doc.get(id).unwrap().path);

        let mut log = UndoLog::new(16);
        for k in 1..=50 {
            let mut nodes = start.clone();
            nodes.move_nodes(&[0], Vec2::new(f64::from(k) * 0.1, 0.0));
            log.edit(&mut doc, EditPath::new(id, nodes.to_path()));
        }
        assert_eq!(log.len(), 1, "a drag is one step");
        let dragged = bits(&doc.get(id).unwrap().path);
        assert!(log.undo(&mut doc));
        assert_eq!(bits(&doc.get(id).unwrap().path), before);
        assert!(log.redo(&mut doc));
        assert_eq!(bits(&doc.get(id).unwrap().path), dragged);
        // The bounds the quadtree holds have to follow the outline, or the
        // shape becomes unfindable where it now is.
        assert_eq!(doc.bounds(id), Some(doc.get(id).unwrap().bounds()));
    }

    #[test]
    fn fuzz_nodes_round_trip_and_edits_keep_the_outline() {
        /// A generated node: an anchor and its two optional handles.
        type Raw = (Point, Option<Point>, Option<Point>);

        fn generate(r: &mut Rng) -> (Vec<Raw>, bool, u64) {
            let n = 2 + r.below(5) as usize;
            let nodes = (0..n)
                .map(|_| {
                    (
                        r.point(),
                        (r.below(2) == 0).then(|| r.point()),
                        (r.below(2) == 0).then(|| r.point()),
                    )
                })
                .collect();
            (nodes, r.below(2) == 0, r.next_u64())
        }

        check("nodes round trip", 2000, generate, |(raw, closed, pick)| {
            let nodes: Vec<Node> = raw
                .iter()
                .map(|&(point, back, forward)| Node {
                    point,
                    // A handle exactly on its anchor is no handle, which is
                    // the invariant `set` keeps and `from_path` reproduces.
                    back: back.filter(|&h| h != point),
                    forward: forward.filter(|&h| h != point),
                    kind: NodeType::Corner,
                })
                .collect();
            let path = Nodes::subpath(nodes, *closed).to_path();
            // Reading a path back and writing it out again is the fixed point
            // the node tool relies on: an edit must not perturb the rest. The
            // first write is not part of it -- a node list can hold handles no
            // path has anywhere to put, such as one before the start of an
            // open subpath.
            let reread = Nodes::from_path(&path);
            if bits(&reread.to_path()) != bits(&path) {
                return false;
            }
            // Inserting a node leaves the outline where it was, everywhere.
            let i = (*pick as usize) % reread.len();
            if reread.next_of(i).is_none() {
                return true;
            }
            let mut edited = reread.clone();
            edited.insert(i, 0.5);
            if edited.len() != reread.len() + 1 {
                return false;
            }
            let inserted = edited.nodes()[i + 1].point;
            let (_, _, d) = reread.nearest_segment(inserted).unwrap();
            // Relative: these coordinates reach 1e17, where an absolute
            // tolerance is many ulps below anything representable.
            d <= 1e-9 * (1.0 + inserted.to_vec2().length())
        });
    }
}
