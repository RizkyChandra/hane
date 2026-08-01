//! A drawing as data: the one description both rasterizers read (D-002).
//!
//! The oracle's fixtures used to be functions that called [`Pixmap`] methods,
//! which meant the GPU renderer could not draw them without a second copy of
//! all twenty-eight. A copy that drifts is worse than no oracle at all, so a
//! fixture is now a value -- a list of fills with their paint, rule, clip and
//! blend mode -- and both sides consume the same value.
//!
//! Nothing here is clever. It is deliberately the smallest structure that can
//! express what P1 can draw, because every field is a field `hane-gpu` has to
//! turn into a draw command and `hane-wasm` has to submit.

use crate::clip::Clip;
use crate::fill::{BlendMode, Color, FillRule, Pixmap};
use crate::paint::Paint;
use hane_geom::PathEl;

/// One fill: a path, what to paint it with, and how it lands.
#[derive(Clone, Debug)]
pub struct Draw {
    /// The path to fill, in pixels.
    pub path: Vec<PathEl>,
    /// What to paint it with.
    pub paint: Paint,
    /// Which winding rule decides the inside.
    pub rule: FillRule,
    /// How the result combines with the backdrop.
    pub blend: BlendMode,
    /// Paths whose coverage multiplies this fill's, innermost last.
    ///
    /// A list and not a [`Clip`] because the GPU builds its mask from the same
    /// paths and would otherwise have to reverse-engineer one from a byte
    /// buffer. Empty is unclipped.
    pub clip: Vec<(Vec<PathEl>, FillRule)>,
}

/// A node of a scene: a fill, or a group of nodes composited as one.
#[derive(Clone, Debug)]
pub enum Node {
    /// One fill.
    Fill(Draw),
    /// An **isolated group**: `nodes` composite onto a transparent surface of
    /// their own, and that surface then composites onto the parent at `alpha`
    /// through `blend`.
    ///
    /// Isolation is the point. A member's blend mode sees only what the group
    /// drew, so a `Multiply` inside a group does not reach through to the page.
    Group {
        /// What the group draws, in order.
        nodes: Vec<Node>,
        /// How the finished group lands on the parent.
        blend: BlendMode,
        /// The group's own opacity, in `[0, 1]`.
        alpha: f64,
    },
}

/// A canvas and everything drawn onto it, in order.
#[derive(Clone, Debug)]
pub struct Scene {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// The nodes, painted back to front.
    pub nodes: Vec<Node>,
}

impl Scene {
    /// An empty `width` by `height` scene.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            nodes: Vec::new(),
        }
    }

    /// Appends a nonzero-rule solid fill -- the shorthand most of the corpus is
    /// written in, and the exact equivalent of [`Pixmap::fill_path`].
    pub fn fill(&mut self, path: Vec<PathEl>, color: Color) -> &mut Self {
        self.push(Draw {
            path,
            paint: Paint::Solid(color),
            rule: FillRule::NonZero,
            blend: BlendMode::Normal,
            clip: Vec::new(),
        })
    }

    /// Appends a fill.
    pub fn push(&mut self, draw: Draw) -> &mut Self {
        self.nodes.push(Node::Fill(draw));
        self
    }

    /// Appends an isolated group.
    pub fn push_group(&mut self, nodes: Vec<Node>, blend: BlendMode, alpha: f64) -> &mut Self {
        self.nodes.push(Node::Group {
            nodes,
            blend,
            alpha,
        });
        self
    }

    /// Renders this scene with the CPU rasterizer: the oracle's answer.
    pub fn render(&self) -> Pixmap {
        let mut pm = Pixmap::new(self.width, self.height);
        paint_nodes(&mut pm, &self.nodes);
        pm
    }
}

fn paint_nodes(pm: &mut Pixmap, nodes: &[Node]) {
    for node in nodes {
        match node {
            Node::Fill(d) => {
                // Built per fill rather than cached: a clip is a list of paths
                // and two draws sharing one is not a case the corpus has. The
                // GPU side rebuilds its mask per fill for the same reason.
                let clip = build_clip(pm.width(), pm.height(), &d.clip);
                pm.fill_path_blend(&d.path, &d.paint, d.rule, clip.as_ref(), d.blend);
            }
            Node::Group {
                nodes,
                blend,
                alpha,
            } => {
                let mut layer = Pixmap::new(pm.width(), pm.height());
                paint_nodes(&mut layer, nodes);
                pm.composite(&layer, *blend, *alpha);
            }
        }
    }
}

fn build_clip(w: u32, h: u32, paths: &[(Vec<PathEl>, FillRule)]) -> Option<Clip> {
    let (first, rest) = paths.split_first()?;
    let mut clip = Clip::from_path(w, h, &first.0, first.1);
    for (path, rule) in rest {
        clip.intersect_path(path, *rule);
    }
    Some(clip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::Point;

    const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<PathEl> {
        vec![
            PathEl::MoveTo(Point::new(x0, y0)),
            PathEl::LineTo(Point::new(x1, y0)),
            PathEl::LineTo(Point::new(x1, y1)),
            PathEl::LineTo(Point::new(x0, y1)),
            PathEl::ClosePath,
        ]
    }

    /// The shorthand has to be the same bytes as the method it stands in for,
    /// or every golden moved when the corpus was turned into data.
    #[test]
    fn fill_matches_the_pixmap_method() {
        let mut sc = Scene::new(8, 8);
        sc.fill(rect(1.0, 1.5, 6.25, 7.0), WHITE);

        let mut pm = Pixmap::new(8, 8);
        pm.fill_path(&rect(1.0, 1.5, 6.25, 7.0), WHITE);
        assert_eq!(sc.render().data(), pm.data());
    }

    /// A group over a transparent surface is invisible to what it covers, and
    /// its alpha scales the finished layer rather than each member -- two
    /// overlapping opaque squares at group alpha 0.5 must not show a seam.
    #[test]
    fn a_group_composites_as_one_surface() {
        let mut sc = Scene::new(8, 8);
        sc.push_group(
            vec![
                Node::Fill(Draw {
                    path: rect(0.0, 0.0, 5.0, 8.0),
                    paint: Paint::Solid(WHITE),
                    rule: FillRule::NonZero,
                    blend: BlendMode::Normal,
                    clip: Vec::new(),
                }),
                Node::Fill(Draw {
                    path: rect(3.0, 0.0, 8.0, 8.0),
                    paint: Paint::Solid(WHITE),
                    rule: FillRule::NonZero,
                    blend: BlendMode::Normal,
                    clip: Vec::new(),
                }),
            ],
            BlendMode::Normal,
            0.5,
        );
        let pm = sc.render();
        let alpha = |x: u32| pm.data()[((x) * 4 + 3) as usize];
        assert_eq!(alpha(1), 128);
        assert_eq!(alpha(4), 128, "the overlap is one surface, not two");
        assert_eq!(alpha(6), 128);
    }

    /// Nested clip paths intersect, and a clip list is applied in full.
    #[test]
    fn a_clip_list_intersects() {
        let mut sc = Scene::new(8, 8);
        sc.push(Draw {
            path: rect(0.0, 0.0, 8.0, 8.0),
            paint: Paint::Solid(WHITE),
            rule: FillRule::NonZero,
            blend: BlendMode::Normal,
            clip: vec![
                (rect(0.0, 0.0, 6.0, 8.0), FillRule::NonZero),
                (rect(2.0, 0.0, 8.0, 8.0), FillRule::NonZero),
            ],
        });
        let pm = sc.render();
        let alpha = |x: u32| pm.data()[((x) * 4 + 3) as usize];
        assert_eq!(alpha(1), 0);
        assert_eq!(alpha(3), 255);
        assert_eq!(alpha(7), 0);
    }
}
