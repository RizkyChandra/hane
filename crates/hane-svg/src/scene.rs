//! What an imported SVG becomes: the document in our own terms (D-008).
//!
//! The scene graph is the document; SVG is a format we read and write. So this
//! is deliberately *not* an SVG DOM. Shapes are all [`Path`], because a rect
//! and a circle differ only in how they were written down; style is the
//! resolved [`Style`], because the cascade is an SVG problem and not a
//! rendering one; and the group hierarchy survives only because a transform and
//! a clip need somewhere to hang.
//!
//! What that costs is recorded rather than hidden: every element, attribute and
//! feature the import could not represent lands in [`Scene::losses`], which is
//! the deliverable half of "lossy by design".

use crate::style::Style;
use core::fmt;
use hane_geom::{Affine, Point, Rect};
use hane_path::Path;
use hane_scene::{Arena, NodeId};

/// A whole imported document.
pub struct Scene {
    /// Every node, group and leaf alike. Navigate from [`roots`](Self::roots);
    /// a node is reachable from exactly one parent.
    pub nodes: Arena<Node>,
    /// The children of the `<svg>` element, in paint order.
    pub roots: Vec<NodeId>,
    /// Gradient definitions. A paint value of `url(#id)` names one of these by
    /// [`Gradient::id`].
    pub gradients: Vec<Gradient>,
    /// Clip definitions, named by [`Node::clip_path`].
    pub clips: Vec<Clip>,
    /// The root `viewBox`, as the rectangle it denotes rather than the
    /// `x y width height` it is written as.
    pub view_box: Option<Rect>,
    /// The root `width` attribute verbatim, units and percentages included.
    ///
    /// Kept as text because `width="100%"` is not a number and re-writing it as
    /// one would change what the document means on a different viewport.
    pub width: Option<String>,
    /// The root `height` attribute, verbatim.
    pub height: Option<String>,
    /// Everything the import could not represent, in document order.
    pub losses: Vec<Loss>,
}

/// One node of the scene graph.
pub struct Node {
    /// Whether this node groups others or draws geometry.
    pub kind: NodeKind,
    /// This node's own transform, applied before its parent's.
    pub transform: Affine,
    /// The computed style, with the cascade already resolved.
    pub style: Style,
    /// The id of a [`Clip`] this node is clipped by, if any.
    pub clip_path: Option<String>,
}

/// A node's content.
pub enum NodeKind {
    /// Children in paint order.
    Group(Vec<NodeId>),
    /// Geometry, in this node's own coordinate system.
    Path(Path),
}

/// A gradient definition.
pub struct Gradient {
    /// The `id` it is referenced by. Never empty: a gradient nothing can name
    /// is not imported.
    pub id: String,
    /// Linear or radial, with the geometry of that kind.
    pub kind: GradientKind,
    /// `gradientTransform`.
    pub transform: Affine,
    /// True for `gradientUnits="userSpaceOnUse"`. False -- the SVG default --
    /// means the coordinates are fractions of the filled shape's bounding box.
    pub user_space: bool,
    /// `spreadMethod`: `pad`, `reflect` or `repeat`, kept as written for the
    /// same reason paint is (see [`Style`]).
    pub spread: String,
    /// The colour stops, in order.
    pub stops: Vec<Stop>,
}

/// The geometry of a gradient, which is all that differs between the two kinds.
pub enum GradientKind {
    /// A linear gradient's axis.
    Linear {
        /// Where offset 0 sits.
        start: Point,
        /// Where offset 1 sits.
        end: Point,
    },
    /// A radial gradient's end circle and focal point.
    Radial {
        /// Centre of the end circle.
        center: Point,
        /// Radius of the end circle.
        radius: f64,
        /// The focal point, which defaults to the centre.
        focus: Point,
    },
}

/// One colour stop.
pub struct Stop {
    /// Position along the gradient, clamped to `0..=1`.
    pub offset: f64,
    /// The computed `stop-color`, as text.
    pub color: String,
    /// The computed `stop-opacity`, as text.
    pub opacity: String,
}

/// A clip definition: geometry, already in the coordinate system of whatever it
/// clips.
///
/// Flattened to bare paths rather than kept as a subtree because SVG 1.1 allows
/// only shapes inside `<clipPath>`, so a group in here could not be written
/// back out.
pub struct Clip {
    /// The `id` it is referenced by.
    pub id: String,
    /// The clipping geometry. A point is inside the clip when it is inside any
    /// of these.
    pub paths: Vec<Path>,
}

/// Something the document said that the scene graph cannot hold.
///
/// Import never fails on one of these -- the rest of the file still opens --
/// but a user who is about to save over their original needs to be told, so
/// these are returned rather than logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    /// What was dropped, and what that means, without position.
    pub message: String,
    /// 1-based line of the element it was dropped from.
    pub line: usize,
    /// 1-based column of the element it was dropped from.
    pub column: usize,
}

impl fmt::Display for Loss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl Scene {
    /// Every leaf path in paint order, in the root coordinate system.
    ///
    /// This is the "nested groups flatten correctly" property made available
    /// rather than merely tested: a renderer wants world-space geometry, and
    /// composing the chain of group transforms is the only way to get it.
    #[must_use]
    pub fn flatten(&self) -> Vec<Path> {
        let mut out = Vec::new();
        for &root in &self.roots {
            self.flatten_node(root, Affine::IDENTITY, &mut out);
        }
        out
    }

    fn flatten_node(&self, id: NodeId, parent: Affine, out: &mut Vec<Path>) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        // The parent's transform applies last, so it is on the left.
        let ctm = parent * node.transform;
        match &node.kind {
            NodeKind::Group(children) => {
                for &child in children {
                    self.flatten_node(child, ctm, out);
                }
            }
            NodeKind::Path(path) => out.push(path.transform(ctm)),
        }
    }
}
