//! A scene turned into GPU draw data: buffers, uniforms and an ordered command
//! list, with no GL call anywhere (D-010).
//!
//! # The rasterization approach, and why this one
//!
//! WebGL2 has no compute shaders (D-003), which leaves two shapes: stencil-then-
//! cover, or coverage computed in a fragment shader. This is the second, in its
//! most literal form -- **the fragment shader runs the oracle's algorithm**.
//! For each pixel it walks the same sixteen sample lines, intersects the same
//! edges, sorts the crossings that land inside that pixel, and accumulates the
//! same analytic horizontal spans.
//!
//! That choice is not about speed, it is about what the acceptance criterion
//! says. "Matches the P1 oracle within tolerance" over a corpus containing
//! `pentagram` (winding 2 against 0 inside one pixel), `nested_triangles`
//! (winding 3) and `annulus_same_winding` rules out the usual GPU answer,
//! signed-area accumulation with `min(|a|, 1)`: that integrates the *winding*
//! over the pixel, which equals coverage only where the winding is 0 or 1, and
//! at each of the pentagram's five crossing vertices it is out by up to a
//! quarter of a pixel -- 60 counts, thirty times the tolerance. Stencil-then-
//! cover has the same problem from the other end: it is exact about the winding
//! and has no anti-aliasing at all without MSAA, which `glctx.rs` deliberately
//! turned off because a second, differently-quantised antialiasing puts the
//! output permanently out of the oracle's reach.
//!
//! So the measurement that chose it is the tolerance table itself. Sorting a
//! handful of crossings per pixel is more work per fragment than either
//! alternative; it is the only one of the three that can be *right*, and the
//! tile binning below is what keeps the edge loop short enough to pay for.
//!
//! # Which edges a tile needs
//!
//! Not the ones [`TileBinner`](crate::TileBinner) answers with. That question
//! is "which tiles does this segment cross", which is what a stencil or a cache
//! wants. A shader computing a winding number needs "which edges can change the
//! winding at a pixel of this tile", and that includes every edge **to the
//! left** in the same band of rows -- the winding at a pixel is a count of
//! crossings from `x = -inf`, and an edge two tiles to the left is as
//! load-bearing as one inside.
//!
//! So a tile takes every edge that overlaps its band of sixteen rows and whose
//! leftmost point in that band is left of the tile's right edge. Handing a tile
//! *extra* edges is always safe -- a crossing outside the pixel changes nothing
//! but the loop count -- which is why the margin below is generous rather than
//! exact.
//!
//! ponytail: that is `O(edges * columns)` in the worst case, because a shape on
//! the far left is listed in every tile of its rows. The upgrade is a per-tile
//! backdrop winding with edges clipped to tile rows, as Pathfinder does it;
//! it is a real piece of work and it buys nothing until a canvas is wide.

use crate::tile::TILE_SIZE;
use hane_geom::PathEl;
use hane_raster::{BlendMode, Color, FillRule, Node, Paint, Scene, Spread, fill_edges};

/// The flattening tolerance, in pixels.
///
/// Not a knob: it is the oracle's own constant. A GPU that flattened a circle
/// into different chords than the CPU did would differ from it everywhere along
/// the outline for a reason that has nothing to do with the renderer.
pub const FLATTEN_TOLERANCE: f64 = 1e-3;

/// The most colour stops one gradient can carry.
///
/// A uniform array, so it is a fixed size and a fixed cost per draw. Sixteen is
/// past what any SVG gradient in the corpus or in the wild uses; a longer ramp
/// is rejected rather than silently truncated.
pub const MAX_STOPS: usize = 16;

/// The most clip paths that may be intersected for one fill.
///
/// The mask itself has no depth limit -- intersection is a product accumulated
/// in place, so the tenth clip costs exactly what the first did. What bounds it
/// is precision: each level multiplies into a half-float mask and loses up to
/// half of its last bit, about 1 part in 2048. Eight levels is under 0.4% of
/// coverage, comfortably inside the 2-count tolerance, and is a depth no real
/// document reaches.
pub const MAX_CLIP_DEPTH: usize = 8;

/// The width of the edge texture, in texels.
///
/// The edge array is a texture because a uniform array cannot hold thousands of
/// edges. 512 keeps the height under the 2048-texel floor of the WebGL2 spec
/// for a million edges.
pub const EDGE_TEX_WIDTH: usize = 512;

/// One command in the ordered list the submitter executes.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Set the clip mask to 1 everywhere: no clip.
    ClipReset,
    /// Multiply the clip mask by the coverage of one path.
    ClipPath {
        /// First tile instance, and how many. Covers the whole grid, because a
        /// tile the path misses has to be multiplied by *zero*, not left alone.
        instances: (u32, u32),
        /// Which winding rule reads the clip path.
        rule: FillRule,
    },
    /// Fill a path.
    Fill {
        /// First tile instance, and how many. Only tiles with edges.
        instances: (u32, u32),
        /// Which winding rule reads the path.
        rule: FillRule,
        /// How the result meets the backdrop.
        blend: BlendMode,
        /// What to paint with. Boxed because it is a fixed-size uniform block
        /// of 340 bytes and every other command is a handful of words; inline
        /// it would make the ops vector twenty times larger than it is used.
        paint: Box<PaintData>,
        /// Whether the clip mask multiplies this fill's coverage.
        clipped: bool,
    },
    /// Start an isolated group: draw onto a fresh transparent surface.
    PushGroup,
    /// Finish the innermost group and composite it onto its parent.
    PopGroup {
        /// How the finished layer meets its parent.
        blend: BlendMode,
        /// The group's opacity.
        alpha: f32,
    },
}

/// A paint, flattened into the numbers a uniform block holds.
///
/// Colours are **premultiplied on the 0..=255 scale**, the same form
/// `hane-raster` interpolates its ramp in -- straight interpolation would part
/// company with the oracle at every stop whose alpha differs from its
/// neighbour's, which is exactly what `gradient_partial_coverage` is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaintData {
    /// 0 solid, 1 linear, 2 radial.
    pub kind: u32,
    /// 0 pad, 1 repeat, 2 reflect.
    pub spread: u32,
    /// Linear: the start of the axis. Radial: the centre.
    pub g0: [f32; 2],
    /// Linear: the end of the axis. Radial: the focus.
    pub g1: [f32; 2],
    /// Radial: the radius of the end circle.
    pub radius: f32,
    /// How many of [`PaintData::offsets`] are live. A solid colour is one stop.
    pub stop_count: u32,
    /// Stop positions, ascending.
    pub offsets: [f32; MAX_STOPS],
    /// Stop colours, four floats each, premultiplied on 0..=255.
    pub colors: [f32; MAX_STOPS * 4],
}

/// Everything one scene needs, as bytes and commands.
#[derive(Clone, Debug)]
pub struct DrawData {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// Four floats per edge -- `ax, ay, bx, by`, in the direction the path runs
    /// through it -- padded to whole rows of [`EDGE_TEX_WIDTH`] texels.
    ///
    /// This is the one place `f64` becomes `f32` (D-004).
    pub edges: Vec<f32>,
    /// Four floats per tile instance: tile origin `x`, `y`, first edge, edge
    /// count.
    pub instances: Vec<f32>,
    /// The draw commands, in order.
    pub ops: Vec<Op>,
}

impl DrawData {
    /// The edge texture's dimensions in texels.
    pub fn edge_texture_size(&self) -> (u32, u32) {
        let rows = self.edges.len() / (EDGE_TEX_WIDTH * 4);
        (EDGE_TEX_WIDTH as u32, rows as u32)
    }

    /// Turns a scene into draw data.
    ///
    /// # Panics
    ///
    /// If a gradient has more than [`MAX_STOPS`] stops or a fill more than
    /// [`MAX_CLIP_DEPTH`] clip paths. Both are silent wrongness otherwise: a
    /// truncated ramp and a clip that is too wide are pictures that look
    /// plausible and are not the scene.
    pub fn build(scene: &Scene) -> Self {
        let mut b = Builder {
            data: Self {
                width: scene.width,
                height: scene.height,
                edges: Vec::new(),
                instances: Vec::new(),
                ops: Vec::new(),
            },
            cols: scene.width.div_ceil(TILE_SIZE),
            rows: scene.height.div_ceil(TILE_SIZE),
            clipped: false,
        };
        b.nodes(&scene.nodes);
        // The texture is uploaded whole, so the tail of the last row has to
        // exist. Zeroed edges are horizontal and cross no sample line, so the
        // padding is inert rather than merely unread.
        let stride = EDGE_TEX_WIDTH * 4;
        let pad = (stride - b.data.edges.len() % stride) % stride;
        b.data.edges.resize(b.data.edges.len() + pad, 0.0);
        b.data
    }
}

struct Builder {
    data: DrawData,
    cols: u32,
    rows: u32,
    /// Whether the clip mask currently holds something other than 1.
    clipped: bool,
}

impl Builder {
    fn nodes(&mut self, nodes: &[Node]) {
        for node in nodes {
            match node {
                Node::Fill(d) => self.fill(d),
                Node::Group {
                    nodes,
                    blend,
                    alpha,
                } => {
                    self.data.ops.push(Op::PushGroup);
                    self.nodes(nodes);
                    self.data.ops.push(Op::PopGroup {
                        blend: *blend,
                        alpha: *alpha as f32,
                    });
                }
            }
        }
    }

    fn fill(&mut self, d: &hane_raster::Draw) {
        assert!(
            d.clip.len() <= MAX_CLIP_DEPTH,
            "clip is {} deep, the bound is {MAX_CLIP_DEPTH}",
            d.clip.len()
        );
        if d.clip.is_empty() {
            // Only worth a command when the mask is dirty: resetting a mask
            // that is already 1 is a full-canvas draw for nothing.
            if self.clipped {
                self.data.ops.push(Op::ClipReset);
                self.clipped = false;
            }
        } else {
            self.data.ops.push(Op::ClipReset);
            self.clipped = true;
            for (path, rule) in &d.clip {
                // Every tile, not just the path's: a tile the clip path misses
                // must come out zero, and a tile that is not drawn keeps the 1
                // the reset put there.
                let instances = self.push_instances(path, true);
                self.data.ops.push(Op::ClipPath {
                    instances,
                    rule: *rule,
                });
            }
        }
        let instances = self.push_instances(&d.path, false);
        self.data.ops.push(Op::Fill {
            instances,
            rule: d.rule,
            blend: d.blend,
            paint: Box::new(PaintData::from_paint(&d.paint)),
            clipped: !d.clip.is_empty(),
        });
    }

    /// Bins `path` and appends its edges and tile instances.
    ///
    /// `whole_grid` emits an instance for every tile including the empty ones,
    /// which is what a clip mask needs and a fill does not.
    fn push_instances(&mut self, path: &[PathEl], whole_grid: bool) -> (u32, u32) {
        let first_instance = (self.data.instances.len() / 4) as u32;
        let size = TILE_SIZE as f64;

        // The narrowing point (D-004). Everything from here on is what the
        // shader will actually see, including the binning below -- binning on
        // the f64 coordinates could put an edge in a tile the f32 one misses.
        let edges: Vec<[f32; 4]> = fill_edges(path)
            .into_iter()
            .map(|e| e.map(|v| v as f32))
            .collect();

        for r in 0..self.rows {
            let (band0, band1) = (f64::from(r) * size, f64::from(r + 1) * size);
            // First edge index this row's runs start at; each tile in the row
            // gets a contiguous run, so a tile is one texture range.
            let mut row_start: Vec<u32> = vec![self.cols; edges.len()];
            // One past the last column any edge of this band can reach. Every
            // subpath is closed, so the winding number right of the rightmost
            // crossing on a sample line is zero and a tile out there is empty
            // whatever is to its left.
            let mut row_end = 0;
            for (i, e) in edges.iter().enumerate() {
                let (ax, ay, bx, by) = (
                    f64::from(e[0]),
                    f64::from(e[1]),
                    f64::from(e[2]),
                    f64::from(e[3]),
                );
                let (top, bot) = if ay < by { (ay, by) } else { (by, ay) };
                // The edge covers [top, bot); a band it only touches at its
                // last row is a band it crosses no sample line of.
                if !(top < band1 && bot > band0) {
                    continue;
                }
                // Where the edge is at the two ends of the band it occupies:
                // it is straight, so its leftmost point there is one of them.
                let at = |y: f64| {
                    let t = ((y - ay) / (by - ay)).clamp(0.0, 1.0);
                    (1.0 - t) * ax + t * bx
                };
                let (xa, xb) = (at(band0.max(top)), at(band1.min(bot)));
                let (min_x, max_x) = (xa.min(xb), xa.max(xb));
                if !min_x.is_finite() || !max_x.is_finite() {
                    continue;
                }
                // A hair of margin either way: the shader interpolates in `f32`
                // and may land a fraction outside this. An extra tile costs a
                // loop iteration; a missing one is a wrong winding number.
                let c0 = ((min_x - 0.01) / size).floor();
                if c0 >= f64::from(self.cols) {
                    continue; // entirely right of the canvas
                }
                row_start[i] = c0.max(0.0) as u32;
                let c1 = ((max_x + 0.01) / size).floor() + 1.0;
                row_end = row_end.max(c1.clamp(0.0, f64::from(self.cols)) as u32);
            }

            let last = if whole_grid { self.cols } else { row_end };
            for c in 0..last {
                let start = (self.data.edges.len() / 4) as u32;
                for (i, e) in edges.iter().enumerate() {
                    if row_start[i] <= c {
                        self.data.edges.extend_from_slice(e);
                    }
                }
                let count = (self.data.edges.len() / 4) as u32 - start;
                if count == 0 && !whole_grid {
                    // Nothing can cover a pixel of this tile.
                    continue;
                }
                self.data.instances.extend_from_slice(&[
                    (c * TILE_SIZE) as f32,
                    (r * TILE_SIZE) as f32,
                    start as f32,
                    count as f32,
                ]);
            }
        }

        let count = (self.data.instances.len() / 4) as u32 - first_instance;
        (first_instance, count)
    }
}

impl PaintData {
    /// The uniform values for `paint`.
    fn from_paint(paint: &Paint) -> Self {
        let mut out = Self {
            kind: 0,
            spread: 0,
            g0: [0.0; 2],
            g1: [0.0; 2],
            radius: 0.0,
            stop_count: 0,
            offsets: [0.0; MAX_STOPS],
            colors: [0.0; MAX_STOPS * 4],
        };
        let stops = match paint {
            Paint::Solid(c) => {
                out.kind = 0;
                out.stop_count = 1;
                out.colors[..4].copy_from_slice(&premul(*c));
                return out;
            }
            Paint::Linear {
                start,
                end,
                stops,
                spread,
            } => {
                out.kind = 1;
                out.g0 = [start.x as f32, start.y as f32];
                out.g1 = [end.x as f32, end.y as f32];
                out.spread = spread_code(*spread);
                stops
            }
            Paint::Radial {
                center,
                radius,
                focus,
                stops,
                spread,
            } => {
                out.kind = 2;
                out.g0 = [center.x as f32, center.y as f32];
                out.g1 = [focus.x as f32, focus.y as f32];
                out.radius = *radius as f32;
                out.spread = spread_code(*spread);
                stops
            }
        };
        assert!(
            stops.len() <= MAX_STOPS,
            "gradient has {} stops, the bound is {MAX_STOPS}",
            stops.len()
        );
        out.stop_count = stops.len() as u32;
        for (i, s) in stops.iter().enumerate() {
            out.offsets[i] = s.offset as f32;
            out.colors[i * 4..i * 4 + 4].copy_from_slice(&premul(s.color));
        }
        out
    }
}

fn spread_code(s: Spread) -> u32 {
    match s {
        Spread::Pad => 0,
        Spread::Repeat => 1,
        Spread::Reflect => 2,
    }
}

/// A straight colour as premultiplied `f32` on the 0..=255 scale, by the same
/// multiply-then-divide the oracle uses so an opaque fill lands on its own
/// colour rather than one below it.
fn premul(c: Color) -> [f32; 4] {
    let a = f64::from(c.a);
    [
        (f64::from(c.r) * a / 255.0) as f32,
        (f64::from(c.g) * a / 255.0) as f32,
        (f64::from(c.b) * a / 255.0) as f32,
        a as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::Point;
    use hane_raster::{Draw, Stop};

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<PathEl> {
        vec![
            PathEl::MoveTo(Point::new(x0, y0)),
            PathEl::LineTo(Point::new(x1, y0)),
            PathEl::LineTo(Point::new(x1, y1)),
            PathEl::LineTo(Point::new(x0, y1)),
            PathEl::ClosePath,
        ]
    }

    const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    /// A shape confined to one tile is binned into that tile and stops there:
    /// the winding right of every crossing is zero, so the columns beyond it
    /// are not listed at all.
    #[test]
    fn a_small_shape_stops_at_its_own_tile() {
        let mut sc = Scene::new(64, 64);
        sc.fill(rect(2.0, 2.0, 10.0, 10.0), WHITE);
        let d = DrawData::build(&sc);
        let Op::Fill { instances, .. } = d.ops[0] else {
            panic!("expected a fill, got {:?}", d.ops[0]);
        };
        assert_eq!(instances, (0, 1), "one tile, its own");
        assert_eq!(&d.instances[..2], &[0.0, 0.0]);
        // Two, not four: the oracle's flattener drops horizontal edges, which
        // cross no sample line, and the GPU has to inherit exactly that.
        assert_eq!(d.instances[3], 2.0);
    }

    /// The rule that makes the winding number right: a tile with no edge of its
    /// own still needs the edges to its left, or every pixel inside a wide
    /// shape comes out empty.
    #[test]
    fn a_tile_inside_a_shape_gets_the_edges_left_of_it() {
        let mut sc = Scene::new(64, 16);
        // Vertical sides at 4 and 60, so the two middle tiles hold no edge of
        // their own and are entirely inside the shape.
        sc.fill(rect(4.0, 0.0, 60.0, 16.0), WHITE);
        let d = DrawData::build(&sc);
        let Op::Fill { instances, .. } = d.ops[0] else {
            panic!("expected a fill");
        };
        assert_eq!(instances.1, 4, "every column of the one row");
        for c in 0..4 {
            let inst = &d.instances[c * 4..c * 4 + 4];
            assert_eq!(inst[0], (c * 16) as f32);
            assert!(inst[3] >= 1.0, "column {c} has no edge to count from");
        }
        // The left edge is in every tile; the right one only from its own
        // column on, which is the whole point of binning by leftmost x.
        assert_eq!(d.instances[3], 1.0, "column 0: only its own left edge");
        assert_eq!(d.instances[15], 2.0, "column 3: both");
    }

    /// An empty path draws nothing at all rather than an empty tile per grid
    /// cell -- `far_offscreen` and `degenerate_empty` are the fixtures.
    #[test]
    fn an_empty_path_emits_no_instances() {
        let mut sc = Scene::new(64, 64);
        sc.fill(Vec::new(), WHITE);
        sc.fill(rect(1.0e12, 1.0e12, 1.0e12 + 8.0, 1.0e12 + 8.0), WHITE);
        let d = DrawData::build(&sc);
        assert_eq!(d.instances.len(), 0);
        for op in &d.ops {
            let Op::Fill { instances, .. } = op else {
                panic!("expected fills");
            };
            assert_eq!(instances.1, 0);
        }
    }

    /// A clip covers the whole grid, because a tile the clip path misses has to
    /// be multiplied by zero and an undrawn tile keeps the reset's 1.
    #[test]
    fn a_clip_path_covers_every_tile() {
        let mut sc = Scene::new(64, 64);
        sc.push(Draw {
            path: rect(0.0, 0.0, 64.0, 64.0),
            paint: Paint::Solid(WHITE),
            rule: FillRule::NonZero,
            blend: BlendMode::Normal,
            clip: vec![(rect(2.0, 2.0, 6.0, 6.0), FillRule::NonZero)],
        });
        let d = DrawData::build(&sc);
        assert_eq!(d.ops[0], Op::ClipReset);
        let Op::ClipPath { instances, .. } = d.ops[1] else {
            panic!("expected a clip path");
        };
        assert_eq!(instances.1, 16, "4x4 tiles");
    }

    /// The mask is only reset when it is dirty. An unclipped scene must not pay
    /// a full-canvas draw per fill for a clip nobody asked for.
    #[test]
    fn an_unclipped_scene_has_no_clip_commands() {
        let mut sc = Scene::new(32, 32);
        sc.fill(rect(0.0, 0.0, 8.0, 8.0), WHITE);
        sc.fill(rect(8.0, 8.0, 16.0, 16.0), WHITE);
        let d = DrawData::build(&sc);
        assert!(
            d.ops.iter().all(|o| matches!(o, Op::Fill { .. })),
            "{:?}",
            d.ops
        );
    }

    /// A group is a balanced pair around its members, so the submitter can keep
    /// a plain stack of render targets.
    #[test]
    fn a_group_brackets_its_members() {
        let mut sc = Scene::new(32, 32);
        sc.push_group(
            vec![Node::Fill(Draw {
                path: rect(0.0, 0.0, 8.0, 8.0),
                paint: Paint::Solid(WHITE),
                rule: FillRule::NonZero,
                blend: BlendMode::Multiply,
                clip: Vec::new(),
            })],
            BlendMode::Screen,
            0.5,
        );
        let d = DrawData::build(&sc);
        assert_eq!(d.ops[0], Op::PushGroup);
        assert!(matches!(d.ops[1], Op::Fill { .. }));
        assert_eq!(
            d.ops[2],
            Op::PopGroup {
                blend: BlendMode::Screen,
                alpha: 0.5
            }
        );
    }

    /// The ramp is premultiplied here so the shader can lerp it directly, the
    /// same form and the same scale the oracle interpolates in.
    #[test]
    fn stops_arrive_premultiplied_on_the_byte_scale() {
        let paint = Paint::Linear {
            start: Point::ORIGIN,
            end: Point::new(1.0, 0.0),
            stops: vec![
                Stop {
                    offset: 0.0,
                    color: Color {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Color {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 128,
                    },
                },
            ],
            spread: Spread::Pad,
        };
        let p = PaintData::from_paint(&paint);
        assert_eq!(p.kind, 1);
        assert_eq!(p.stop_count, 2);
        assert_eq!(&p.colors[..4], &[255.0, 0.0, 0.0, 255.0]);
        assert_eq!(&p.colors[4..8], &[128.0, 0.0, 0.0, 128.0]);
    }

    /// The edge texture is uploaded whole, so its length has to be a whole
    /// number of rows however many edges there were.
    #[test]
    fn the_edge_array_is_padded_to_whole_texture_rows() {
        let mut sc = Scene::new(64, 64);
        sc.fill(rect(2.0, 2.0, 10.0, 10.0), WHITE);
        let d = DrawData::build(&sc);
        assert_eq!(d.edges.len() % (EDGE_TEX_WIDTH * 4), 0);
        assert_eq!(d.edge_texture_size(), (EDGE_TEX_WIDTH as u32, 1));
    }

    #[test]
    #[should_panic(expected = "the bound is 16")]
    fn too_many_stops_is_rejected_rather_than_truncated() {
        let stops = (0..17)
            .map(|i| Stop {
                offset: f64::from(i) / 16.0,
                color: WHITE,
            })
            .collect();
        PaintData::from_paint(&Paint::Linear {
            start: Point::ORIGIN,
            end: Point::new(1.0, 0.0),
            stops,
            spread: Spread::Pad,
        });
    }
}
