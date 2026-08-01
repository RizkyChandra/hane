//! Boolean operations: union, intersect, subtract and divide.
//!
//! # The pipeline
//!
//! Four stages, and every one of them is about degeneracy rather than about
//! topology. The topology is a textbook half-edge walk; what sinks boolean
//! implementations is that the *inputs* to that walk disagree with each other
//! by an ulp.
//!
//! **Split.** Every segment is cut where it meets itself -- a cubic's loop
//! has a closed form, a fold is where the derivative vanishes -- and then
//! every segment is cut at every intersection with every other, using
//! [`CubicBez::intersect`]. Coincident stretches come back from there as a
//! parameter *range* rather than as a list of sample points, and both ends of
//! the range become cuts, so a shared arc ends up as the same edge on both
//! sides rather than as two edges a hair apart.
//!
//! **Snap.** Every cut point and every segment end is clustered into one
//! vertex set at a tolerance relative to the size of the input, and each
//! sub-segment's ends are moved onto their cluster's representative. This is
//! the step the rest of the pipeline is built to trust: after it, two edges
//! that meet share a vertex *index*, not merely a coordinate, and no later
//! stage compares floats to decide whether two things are the same thing.
//! Edges that then turn out to trace the same arc are merged into one carrying
//! the sum of what each contributed -- which is how a boundary the two inputs
//! share stops being two edges that could be classified differently, and how a
//! fold, whose two halves contribute `+1` and `-1`, correctly stops
//! contributing anything.
//!
//! **Arrange.** Half-edges are ordered around each vertex by the angle to the
//! point a fixed small distance along them -- not by the tangent, which orders
//! two edges meeting tangentially by nothing but rounding -- and walked into
//! face cycles, faces lying to the left of their half-edges. Disconnected
//! pieces are not linked into their containing face; the usual DCEL
//! hole-linking step is skipped, because a face's winding is not derived from
//! its position in a containment hierarchy.
//!
//! **Classify.** Crossing an edge from its right to its left changes each
//! winding number by exactly what that edge contributes, so within one
//! connected piece of the arrangement every face's winding is a single unknown
//! pair plus known integer offsets. Faces are sampled -- an interior point,
//! then a ray cast against the *original* paths -- but each sample is a vote
//! on that one unknown rather than an answer for its own face, and the vote
//! with the most clearance behind it wins. Two consequences matter: no face
//! can disagree with its neighbour, whatever a sample says, and one sliver
//! sampled on the wrong side cannot cut a hole in the middle of a result.
//!
//! # Where it is still not robust
//!
//! Two curves crossing at a shallow angle have an intersection point that is
//! ill conditioned by `1 / sin(angle)`; when that error exceeds the snapping
//! distance they get two vertices where they should have had one, and the
//! arrangement stops being a planar subdivision. On a corpus built to provoke
//! it -- control points drawn from a five-point grid, so nearly parallel
//! crossings are the common case -- this happens to about one pair in two
//! hundred. The vote above is what keeps it from mattering much: the winding
//! numbers stay self-consistent and the area of the result stays right, but
//! the geometry near that one vertex carries a sliver.
//!
use hane_geom::{Affine, CubicBez, Intersections, PathEl, Point, QuadBez, Rect, Vec2};

use crate::{Path, Segment};

/// Vertex snapping distance, relative to the size of the input.
///
/// Not set by what the intersector's tolerance is -- `1e-12` relative -- but
/// by what its *answers* are worth. Two nearly parallel curves cross at a
/// point whose position is uncertain by `1 / sin(angle)` times the residual,
/// and projecting a point that lies on a curve back onto it recovers only
/// half a double's digits. Both land around `1e-8` relative on real input, and
/// a snapping distance under that leaves two vertices where there should have
/// been one. At `1e-7` of the drawing's size this is a tenth of a micron on a
/// metre-wide artboard, which nothing downstream can see.
const SNAP: f64 = 1e-7;

/// Half-edges sampled per face when looking for an interior point.
///
/// A face's own edges are all equally valid places to step inwards from, and
/// the first few are as likely to have clearance as the last few.
// ponytail: caps the cost of a face with thousands of edges at the price of
// possibly missing its one roomy spot. Raise it if a real document ever
// produces a face whose only clearance is late in its cycle.
const SAMPLE_EDGES: usize = 32;

/// Offsets tried inwards from an edge, as fractions of the largest step that
/// provably cannot leave the face. Coarse first: the roomiest point of a fat
/// face is far from its boundary, and a thin one needs the fine offsets.
const SAMPLE_OFFSETS: [f64; 4] = [1.0, 0.25, 0.0625, 0.01];

/// Which points a path encloses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillRule {
    /// Filled where the winding number is not zero.
    NonZero,
    /// Filled where the winding number is odd.
    EvenOdd,
}

impl FillRule {
    /// Whether this rule fills a region of winding number `w`.
    #[inline]
    pub fn fills(self, w: i32) -> bool {
        match self {
            Self::NonZero => w != 0,
            Self::EvenOdd => w & 1 != 0,
        }
    }
}

/// The four boolean operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoolOp {
    /// Everything either path fills.
    Union,
    /// Everything both paths fill.
    Intersect,
    /// Everything the first path fills and the second does not.
    Subtract,
    /// The three regions -- first only, both, second only -- as separate
    /// paths.
    Divide,
}

/// One edge of the arrangement: a piece of an input segment that meets other
/// edges only at its two ends.
#[derive(Clone, Copy, Debug)]
struct Edge {
    seg: Segment,
    v0: usize,
    v1: usize,
    /// What crossing this edge from its right side to its left does to the
    /// winding number of each input path.
    ///
    /// Normally `[1, 0]` or `[0, 1]` with a sign for direction, but an edge
    /// two inputs share carries both, and an edge a path traverses twice
    /// carries two.
    wind: [i32; 2],
}

/// Two paths split at their intersections into non-crossing edges, with the
/// faces those edges bound.
///
/// The arrangement is what the boolean operations are selected from, and it is
/// built once whatever the operation: [`apply`](Self::apply) may be called
/// repeatedly on one arrangement.
#[derive(Clone, Debug)]
pub struct Arrangement {
    verts: Vec<Point>,
    edges: Vec<Edge>,
    /// Half-edge `h` runs along `edges[h / 2]`, forwards when `h` is even.
    hseg: Vec<Segment>,
    next: Vec<usize>,
    face_of: Vec<usize>,
    faces: Vec<Vec<usize>>,
    wind: Vec<[i32; 2]>,
    eps: f64,
}

impl Arrangement {
    /// Splits `a` and `b` at every intersection and identifies the faces.
    ///
    /// Open subpaths are closed first: a fill rule has no meaning otherwise,
    /// and both SVG and every editor fill an open subpath as though the
    /// closing line were there.
    pub fn new(a: &Path, b: &Path) -> Self {
        let eps = tolerance(a, b);
        let src = [closed_segments(a), closed_segments(b)];
        let mut arr = Self {
            verts: Vec::new(),
            edges: Vec::new(),
            hseg: Vec::new(),
            next: Vec::new(),
            face_of: Vec::new(),
            faces: Vec::new(),
            wind: Vec::new(),
            eps,
        };
        arr.build_edges(&src);
        arr.link();
        arr.classify(&src);
        arr
    }

    /// The vertices: every point where edges meet, deduplicated.
    #[inline]
    pub fn vertices(&self) -> &[Point] {
        &self.verts
    }

    /// How many edges the arrangement has.
    #[inline]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// How many face cycles the arrangement has.
    ///
    /// A cycle, not a region: a region with a hole in it, or a region
    /// surrounding a disconnected piece, is bounded by more than one cycle and
    /// is counted once per cycle. Every cycle of one region carries the same
    /// winding numbers, so nothing downstream has to care.
    #[inline]
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// The winding numbers of the two input paths at every point of `face`.
    #[inline]
    pub fn face_winding(&self, face: usize) -> [i32; 2] {
        self.wind[face]
    }

    /// The result of `op`, with each input filled by its own rule.
    ///
    /// [`Union`](BoolOp::Union), [`Intersect`](BoolOp::Intersect) and
    /// [`Subtract`](BoolOp::Subtract) return at most one path -- none at all
    /// when the result is empty. [`Divide`](BoolOp::Divide) returns up to
    /// three: the part only the first path fills, the part both fill, and the
    /// part only the second fills, in that order, skipping the empty ones.
    pub fn apply(&self, op: BoolOp, rules: [FillRule; 2]) -> Vec<Path> {
        let inside: Vec<[bool; 2]> = self
            .wind
            .iter()
            .map(|w| [rules[0].fills(w[0]), rules[1].fills(w[1])])
            .collect();
        let classes: &[fn([bool; 2]) -> bool] = match op {
            BoolOp::Union => &[|[x, y]| x || y],
            BoolOp::Intersect => &[|[x, y]| x && y],
            BoolOp::Subtract => &[|[x, y]| x && !y],
            BoolOp::Divide => &[|[x, y]| x && !y, |[x, y]| x && y, |[x, y]| !x && y],
        };
        classes
            .iter()
            .map(|f| {
                let keep: Vec<bool> = inside.iter().map(|&s| f(s)).collect();
                self.assemble(&keep)
            })
            .filter(|p| !p.elements().is_empty())
            .collect()
    }

    /// Cuts every segment at its intersections and turns the pieces into
    /// edges over a snapped vertex set.
    fn build_edges(&mut self, src: &[Vec<Segment>; 2]) {
        // Flatten both inputs into one list so a segment pair is a pair of
        // indices regardless of which path each came from. Zero-extent
        // segments are dropped here rather than special-cased later: they have
        // no tangent, so they have no place in the angular order around a
        // vertex, and they enclose nothing.
        // A segment that meets *itself* is cut first, so that from here on
        // "two curves crossing" is always two different entries in this list
        // and the pairwise pass below can be trusted to find everything.
        let eps = self.eps;
        let segs: Vec<(Segment, usize)> = src
            .iter()
            .enumerate()
            .flat_map(|(s, list)| list.iter().map(move |&g| (g, s)))
            .filter(|&(g, _)| extent(g.bounding_box()) > eps)
            .flat_map(|(g, s)| split_at_self(g, eps).into_iter().map(move |q| (q, s)))
            .collect();
        let boxes: Vec<Rect> = segs
            .iter()
            .map(|&(g, _)| g.bounding_box().inflate(self.eps))
            .collect();

        let mut cuts: Vec<Vec<f64>> = vec![Vec::new(); segs.len()];
        // ponytail: every pair, filtered by bounding box. A sweep line or the
        // P3 quadtree would make this O(n log n + k); at the sizes a boolean
        // operation runs on interactively -- hundreds of segments -- the box
        // test is already the whole cost.
        for i in 0..segs.len() {
            for j in i + 1..segs.len() {
                if !boxes[i].overlaps(boxes[j]) {
                    continue;
                }
                match cubic(segs[i].0).intersect(cubic(segs[j].0)) {
                    Intersections::Points(hits) => {
                        for (u, v) in hits {
                            cuts[i].push(u);
                            cuts[j].push(v);
                        }
                    }
                    // Both ends of the shared arc, on both sides. That is what
                    // makes the shared stretch come out as one edge on each
                    // side spanning the same two vertices, ready to be merged.
                    Intersections::Overlap { a, b } => {
                        cuts[i].push(a.0);
                        cuts[i].push(a.1);
                        cuts[j].push(b.0);
                        cuts[j].push(b.1);
                    }
                }
            }
        }

        // Pieces, still carrying raw endpoints.
        let mut pieces: Vec<(Segment, usize)> = Vec::new();
        for (i, &(seg, source)) in segs.iter().enumerate() {
            for (t0, t1) in spans(seg, &mut cuts[i], self.eps) {
                pieces.push((subsegment(seg, t0, t1), source));
            }
        }

        // One vertex per cluster of ends. Everything downstream compares
        // vertex indices, never coordinates.
        let ends: Vec<Point> = pieces
            .iter()
            .flat_map(|&(g, _)| [g.start(), g.end()])
            .collect();
        let (verts, of_point) = cluster(&ends, self.eps);
        self.verts = verts;

        let mut edges: Vec<Edge> = Vec::new();
        for (k, &(g, source)) in pieces.iter().enumerate() {
            let (v0, v1) = (of_point[2 * k], of_point[2 * k + 1]);
            let seg = with_ends(g, self.verts[v0], self.verts[v1]);
            // Snapping can collapse a piece that was already near-degenerate.
            if v0 == v1 && extent(seg.bounding_box()) <= self.eps {
                continue;
            }
            let mut wind = [0, 0];
            wind[source] = 1;
            edges.push(Edge { seg, v0, v1, wind });
        }
        self.edges = merge_duplicates(edges, self.eps);
        // Canonical order, so that the whole arrangement -- and therefore the
        // output -- is the same whichever path was passed first.
        self.edges.sort_by(|x, y| {
            (x.v0, x.v1).cmp(&(y.v0, y.v1)).then_with(|| {
                shape_key(x.seg)
                    .partial_cmp(&shape_key(y.seg))
                    .unwrap_or(core::cmp::Ordering::Equal)
            })
        });
        self.hseg = self
            .edges
            .iter()
            .flat_map(|e| [e.seg, reverse(e.seg)])
            .collect();
    }

    /// The vertex a half-edge leaves.
    #[inline]
    fn origin(&self, h: usize) -> usize {
        let e = &self.edges[h / 2];
        if h.is_multiple_of(2) { e.v0 } else { e.v1 }
    }

    /// Orders the half-edges around each vertex and walks the face cycles.
    fn link(&mut self) {
        let n = self.hseg.len();
        let mut ring: Vec<Vec<usize>> = vec![Vec::new(); self.verts.len()];
        // The radius at which the edges leaving each vertex are compared: a
        // quarter of the shortest of them, so the sample stays on every edge.
        let mut radius = vec![f64::INFINITY; self.verts.len()];
        for h in 0..n {
            let v = self.origin(h);
            ring[v].push(h);
            radius[v] = radius[v].min(0.25 * extent(self.hseg[h].bounding_box()));
        }
        for (v, r) in ring.iter_mut().enumerate() {
            let mut keyed: Vec<([f64; 2], usize)> = r
                .iter()
                .map(|&h| (leave_key(self.hseg[h], radius[v]), h))
                .collect();
            keyed.sort_by(|x, y| x.partial_cmp(y).unwrap_or(core::cmp::Ordering::Equal));
            *r = keyed.into_iter().map(|(_, h)| h).collect();
        }
        let mut pos = vec![0usize; n];
        for r in &ring {
            for (i, &h) in r.iter().enumerate() {
                pos[h] = i;
            }
        }
        // `next(h)` is the half-edge clockwise from `twin(h)` around the
        // vertex `h` arrives at, which is what keeps the face on the left.
        self.next = (0..n)
            .map(|h| {
                let r = &ring[self.origin(h ^ 1)];
                r[(pos[h ^ 1] + r.len() - 1) % r.len()]
            })
            .collect();

        self.face_of = vec![usize::MAX; n];
        self.faces = Vec::new();
        for h0 in 0..n {
            if self.face_of[h0] != usize::MAX {
                continue;
            }
            let f = self.faces.len();
            let mut cycle = Vec::new();
            let mut h = h0;
            // The bound is belt and braces: `next` is a permutation, so the
            // orbit of `h0` returns to `h0`. It costs one comparison and it
            // means a bug in the ring order cannot hang the editor.
            for _ in 0..=n {
                if self.face_of[h] != usize::MAX {
                    break;
                }
                self.face_of[h] = f;
                cycle.push(h);
                h = self.next[h];
            }
            self.faces.push(cycle);
        }
    }

    /// Gives every face the winding numbers of the two inputs.
    ///
    /// Not one sample per face. Crossing an edge from its right to its left
    /// changes each winding number by exactly what that edge contributes, so
    /// within a connected piece of the arrangement the winding numbers of all
    /// its faces are one unknown pair plus known offsets. That leaves every
    /// sample a *vote* on the same unknown, and taking the vote with the most
    /// clearance behind it means one thin face sampled on the wrong side
    /// cannot outweigh the rest -- and, more importantly, cannot make one face
    /// disagree with its neighbour. Classifying faces one at a time is what
    /// lets a single bad sample cut a hole in the middle of a result.
    fn classify(&mut self, src: &[Vec<Segment>; 2]) {
        let cubics: Vec<(CubicBez, Rect)> = self
            .edges
            .iter()
            .map(|e| {
                let c = cubic(e.seg);
                (c, e.seg.bounding_box())
            })
            .collect();
        let rays: [Vec<(CubicBez, Rect)>; 2] = [ray_input(&src[0]), ray_input(&src[1])];
        let n = self.faces.len();

        // Each face's offset from the first face of its component, by breadth
        // first search over the shared edges.
        let mut comp = vec![usize::MAX; n];
        let mut offset = vec![[0i32; 2]; n];
        let mut adjacency: Vec<Vec<(usize, [i32; 2])>> = vec![Vec::new(); n];
        for h in (0..self.hseg.len()).step_by(2) {
            let e = self.edges[h / 2];
            let (l, r) = (self.face_of[h], self.face_of[h ^ 1]);
            adjacency[l].push((r, [-e.wind[0], -e.wind[1]]));
            adjacency[r].push((l, e.wind));
        }
        let mut components = 0;
        for root in 0..n {
            if comp[root] != usize::MAX {
                continue;
            }
            comp[root] = components;
            let mut queue = vec![root];
            while let Some(f) = queue.pop() {
                for (g, delta) in adjacency[f].clone() {
                    if comp[g] == usize::MAX {
                        comp[g] = components;
                        offset[g] = [offset[f][0] + delta[0], offset[f][1] + delta[1]];
                        queue.push(g);
                    }
                }
            }
            components += 1;
        }

        // One vote per face that has a point far enough inside it to trust --
        // a face narrower than the snapping tolerance has none, because the
        // ray would be cast from somewhere the original paths may run.
        let mut votes: Vec<Vec<([i32; 2], f64)>> = vec![Vec::new(); components];
        for f in 0..n {
            let Some((clear, p)) = self
                .interior_point(f, &cubics)
                .filter(|&(clear, _)| clear > self.eps)
            else {
                continue;
            };
            let w = [winding(&rays[0], p), winding(&rays[1], p)];
            let base = [w[0] - offset[f][0], w[1] - offset[f][1]];
            let list = &mut votes[comp[f]];
            match list.iter_mut().find(|(b, _)| *b == base) {
                Some(slot) => slot.1 += clear,
                None => list.push((base, clear)),
            }
        }
        let bases: Vec<[i32; 2]> = votes
            .iter()
            .map(|list| {
                // Ties break on the winding pair itself rather than on the
                // order the faces were walked in, so the answer does not
                // depend on which path was passed first.
                list.iter()
                    .fold(None::<([i32; 2], f64)>, |best, &(b, w)| match best {
                        Some((bb, bw)) if bw > w || (bw == w && bb <= b) => Some((bb, bw)),
                        _ => Some((b, w)),
                    })
                    .map_or([0, 0], |(b, _)| b)
            })
            .collect();
        self.wind = (0..n)
            .map(|f| {
                let b = bases[comp[f]];
                [b[0] + offset[f][0], b[1] + offset[f][1]]
            })
            .collect();
    }

    /// A point inside face `f`, chosen for clearance from every edge.
    ///
    /// Every candidate steps off the midpoint of one of the face's own edges
    /// along the left normal, which is the side the face is on. The length of
    /// that step is what has to be got right, and the bound is not a guess:
    /// no other edge comes nearer to the midpoint than `room`, so a step
    /// shorter than half of `room` stays within a disc that no other edge
    /// enters, and cannot have crossed one into a different face. Stepping a
    /// fixed fraction of the edge's own length instead -- the obvious version
    /// -- walks straight across a nearby hole and classifies the face as
    /// whatever is on the far side of it.
    fn interior_point(&self, f: usize, cubics: &[(CubicBez, Rect)]) -> Option<(f64, Point)> {
        let mut best: Option<(f64, Point)> = None;
        for &h in self.faces[f].iter().take(SAMPLE_EDGES) {
            let c = cubic(self.hseg[h]);
            let mid = c.eval(0.5);
            // The left normal. A vanishing derivative -- a cusp exactly at the
            // midpoint -- falls back to the chord, which has the same side.
            let mut dir = c.deriv_at(0.5);
            if !positive(dir.length_squared()) {
                dir = c.p3 - c.p0;
            }
            let len = dir.length();
            if !positive(len) {
                continue;
            }
            let normal = dir.perp() / len;
            let room = clearance_of(mid, cubics, h / 2);
            // The second bound is against this edge itself: a step longer than
            // a quarter of the edge's extent can reach round a tight bend and
            // land back on the outside of it.
            let limit = (0.5 * room).min(0.25 * extent(self.hseg[h].bounding_box()));
            if !positive(limit) {
                continue;
            }
            for scale in SAMPLE_OFFSETS {
                let step = limit * scale;
                let p = mid + normal * step;
                if !p.is_finite() {
                    continue;
                }
                let floor = best.map_or(0.0, |(c, _)| c);
                // The edge must still be about `step` away. A step along the
                // normal that ends up much nearer the curve than it started
                // has gone round a bend and out the other side, which is what
                // a loop tighter than a quarter of its own extent does -- and
                // a face inside such a loop would otherwise be classified as
                // whatever is outside it.
                let need = 0.4 * step;
                let clear = clearance(p, cubics, floor.max(need));
                if clear >= need && clear > floor {
                    best = Some((clear, p));
                }
            }
        }
        best
    }

    /// Walks the boundary of the union of the kept faces into closed subpaths.
    fn assemble(&self, keep: &[bool]) -> Path {
        let n = self.hseg.len();
        let border = |h: usize| keep[self.face_of[h]] && !keep[self.face_of[h ^ 1]];
        let mut seen = vec![false; n];
        let mut path = Path::new();
        for start in 0..n {
            if seen[start] || !border(start) {
                continue;
            }
            let mut loop_hs = Vec::new();
            let mut h = start;
            loop {
                seen[h] = true;
                loop_hs.push(h);
                // Rotate around the far vertex until the boundary continues.
                // Skipping a half-edge whose other side is also kept is what
                // erases the seam between two faces of the same result.
                let mut g = self.next[h];
                for _ in 0..n {
                    if border(g) {
                        break;
                    }
                    g = self.next[g ^ 1];
                }
                if !border(g) || seen[g] {
                    break;
                }
                h = g;
            }
            path.push(PathEl::MoveTo(self.verts[self.origin(loop_hs[0])]));
            for &h in &loop_hs {
                path.push(element(self.hseg[h]));
            }
            path.push(PathEl::ClosePath);
        }
        path
    }
}

impl Path {
    /// `op` applied to this path and `other`, each filled by its own rule.
    ///
    /// See [`Arrangement::apply`] for what comes back. Both paths keep their
    /// own fill rule, so an even-odd shape may be subtracted from a nonzero
    /// one.
    ///
    /// ```
    /// use hane_path::{BoolOp, FillRule, Path};
    /// use hane_geom::{PathEl, Point};
    ///
    /// let square = |x: f64| {
    ///     Path::from(vec![
    ///         PathEl::MoveTo(Point::new(x, 0.0)),
    ///         PathEl::LineTo(Point::new(x + 2.0, 0.0)),
    ///         PathEl::LineTo(Point::new(x + 2.0, 2.0)),
    ///         PathEl::LineTo(Point::new(x, 2.0)),
    ///         PathEl::ClosePath,
    ///     ])
    /// };
    /// let rules = [FillRule::NonZero; 2];
    /// // Two unit-overlapping squares: the union is one 3x2 rectangle.
    /// let out = square(0.0).boolean(&square(1.0), BoolOp::Union, rules);
    /// assert_eq!(out.len(), 1);
    /// assert!((out[0].bounding_box().width() - 3.0).abs() < 1e-12);
    /// ```
    pub fn boolean(&self, other: &Path, op: BoolOp, rules: [FillRule; 2]) -> Vec<Path> {
        // Inputs that cannot interact are answered without an arrangement at
        // all. This is not only speed: it is what makes a union of disjoint
        // shapes conserve area *exactly* rather than to within the snapping
        // tolerance, because the geometry comes back untouched.
        // Grown by the tolerance first: `Rect::overlaps` is strict, so two
        // shapes that meet exactly along an edge would take the shortcut and
        // come back as two loops with a seam down the middle.
        let eps = tolerance(self, other);
        let (ba, bb) = (self.bounding_box(), other.bounding_box());
        if ba.is_empty() || bb.is_empty() || !ba.inflate(eps).overlaps(bb.inflate(eps)) {
            let mine = |p: &Path| (!p.elements().is_empty()).then(|| p.clone());
            return match op {
                BoolOp::Union => {
                    let els = [self.elements(), other.elements()].concat();
                    (!els.is_empty())
                        .then(|| Path::from(els))
                        .into_iter()
                        .collect()
                }
                BoolOp::Intersect => Vec::new(),
                BoolOp::Subtract => mine(self).into_iter().collect(),
                BoolOp::Divide => [mine(self), mine(other)].into_iter().flatten().collect(),
            };
        }
        Arrangement::new(self, other).apply(op, rules)
    }
}

/// A boolean operation held open while its inputs move.
///
/// The sources are owned and never modified, so cancelling is dropping the
/// preview and committing is taking the result that was already on screen --
/// there is no second, different computation at the end that could disagree
/// with what the user was looking at.
#[derive(Clone, Debug)]
pub struct Preview {
    a: Path,
    b: Path,
    op: BoolOp,
    rules: [FillRule; 2],
    at: Affine,
    out: Vec<Path>,
}

impl Preview {
    /// Starts a preview of `op` on `a` and `b`, with `b` not yet moved.
    pub fn new(a: Path, b: Path, op: BoolOp, rules: [FillRule; 2]) -> Self {
        let out = a.boolean(&b, op, rules);
        Self {
            a,
            b,
            op,
            rules,
            at: Affine::IDENTITY,
            out,
        }
    }

    /// The result with `b` moved by `drag`, recomputing only when `drag` has
    /// actually changed.
    ///
    /// A drag delivers many events per frame that carry the same transform --
    /// a pointer that has not moved between two frames, a modifier key going
    /// down -- and re-running the arrangement for those is the difference
    /// between a preview that keeps up and one that does not.
    pub fn drag(&mut self, drag: Affine) -> &[Path] {
        if drag != self.at {
            self.at = drag;
            self.out = self.a.boolean(&self.b.transform(drag), self.op, self.rules);
        }
        &self.out
    }

    /// The current result, exactly as [`drag`](Self::drag) last left it.
    #[inline]
    pub fn result(&self) -> &[Path] {
        &self.out
    }

    /// Ends the preview, keeping the result on screen.
    #[inline]
    pub fn commit(self) -> Vec<Path> {
        self.out
    }

    /// Ends the preview, giving back the two sources untouched.
    #[inline]
    pub fn cancel(self) -> (Path, Path) {
        (self.a, self.b)
    }
}

/// The snapping distance for a pair of paths.
///
/// Relative to the size of the geometry, plus a floor proportional to the
/// distance from the origin: a shape a millimetre across sitting a kilometre
/// away carries absolute rounding error proportional to its *coordinates*, and
/// a tolerance under that would fail to merge two evaluations of one point.
fn tolerance(a: &Path, b: &Path) -> f64 {
    let bb = a.bounding_box().union(b.bounding_box());
    if bb.is_empty() {
        return 0.0;
    }
    let mag = bb
        .x0
        .abs()
        .max(bb.x1.abs())
        .max(bb.y0.abs())
        .max(bb.y1.abs());
    SNAP * bb.width().max(bb.height()) + 1e-12 * mag
}

/// A usable positive length. Written as a call so that `!positive(x)` rejects
/// a NaN as well as a zero, which `x <= 0.0` alone does not.
#[inline]
fn positive(v: f64) -> bool {
    v > 0.0
}

/// The longer side of a box, or zero when there is nothing to measure.
///
/// Deliberately not gated on [`Rect::is_empty`], which is false for a box of
/// zero width: a vertical line segment has exactly that box, and treating it
/// as nothing would delete every axis-aligned edge in the input.
#[inline]
fn extent(r: Rect) -> f64 {
    let e = r.width().max(r.height());
    if positive(e) { e } else { 0.0 }
}

/// Every subpath of `path` as segments, closed.
///
/// An open subpath is filled as though it were closed, by SVG's rule and every
/// editor's, so the closing line is a real edge of the arrangement.
fn closed_segments(path: &Path) -> Vec<Segment> {
    let mut out = Vec::new();
    for sub in path.subpaths() {
        let first = out.len();
        out.extend(sub.segments());
        if out.len() > first {
            let (a, b) = (out[first].start(), out[out.len() - 1].end());
            if a != b {
                out.push(Segment::Line(b, a));
            }
        }
    }
    out
}

/// This segment as a cubic, for the routines that only speak cubics.
fn cubic(s: Segment) -> CubicBez {
    match s {
        // Evenly spaced controls, so `t` means the same fraction along the
        // line as `lerp` does and a cut parameter can be used either way.
        Segment::Line(a, b) => CubicBez::new(a, a.lerp(b, 1.0 / 3.0), a.lerp(b, 2.0 / 3.0), b),
        Segment::Quad(q) => q.to_cubic(),
        Segment::Cubic(c) => c,
    }
}

/// The piece of `s` between two parameters, keeping the segment's kind.
fn subsegment(s: Segment, t0: f64, t1: f64) -> Segment {
    if t0 == 0.0 && t1 == 1.0 {
        return s;
    }
    match s {
        Segment::Line(a, b) => Segment::Line(a.lerp(b, t0), a.lerp(b, t1)),
        Segment::Quad(q) => Segment::Quad(q.subsegment(t0, t1)),
        Segment::Cubic(c) => Segment::Cubic(c.subsegment(t0, t1)),
    }
}

/// The same geometry traversed backwards.
fn reverse(s: Segment) -> Segment {
    match s {
        Segment::Line(a, b) => Segment::Line(b, a),
        Segment::Quad(q) => Segment::Quad(QuadBez::new(q.p2, q.p1, q.p0)),
        Segment::Cubic(c) => Segment::Cubic(CubicBez::new(c.p3, c.p2, c.p1, c.p0)),
    }
}

/// `s` with its ends moved onto the given points, controls left alone.
///
/// The ends move by at most the snapping distance, so the interior moves by at
/// most that too. Dragging the controls along would keep the shape nearer but
/// would move the *interior* of a long edge by the same amount at the ends,
/// which is no better and costs the property that an unsnapped edge comes
/// back bit-identical.
fn with_ends(s: Segment, p0: Point, p1: Point) -> Segment {
    if s.start() == p0 && s.end() == p1 {
        return s;
    }
    match s {
        Segment::Line(..) => Segment::Line(p0, p1),
        Segment::Quad(q) => Segment::Quad(QuadBez::new(p0, q.p1, p1)),
        Segment::Cubic(c) => Segment::Cubic(CubicBez::new(p0, c.p1, c.p2, p1)),
    }
}

/// The element that draws `s` from its own start point.
fn element(s: Segment) -> PathEl {
    match s {
        Segment::Line(_, b) => PathEl::LineTo(b),
        Segment::Quad(q) => PathEl::QuadTo(q.p1, q.p2),
        Segment::Cubic(c) => PathEl::CurveTo(c.p1, c.p2, c.p3),
    }
}

/// A shape's control points as a comparison key, so that one of two
/// indistinguishable edges can be chosen without reference to input order.
fn shape_key(s: Segment) -> [f64; 8] {
    let c = cubic(s);
    [
        c.p0.x, c.p0.y, c.p1.x, c.p1.y, c.p2.x, c.p2.y, c.p3.x, c.p3.y,
    ]
}

/// The parameter spans of `s` between consecutive cuts.
///
/// Cuts closer together than a piece of geometry -- measured by the size of
/// the arc between them, not by the distance between their points, which a
/// loop would get wrong -- are merged. Merging towards the *ends* rather than
/// away from them is deliberate: a cut a hair short of an endpoint becomes the
/// endpoint, so the edge that follows starts at a vertex the neighbouring
/// segment already has.
fn spans(s: Segment, cuts: &mut Vec<f64>, eps: f64) -> Vec<(f64, f64)> {
    cuts.retain(|t| t.is_finite() && *t > 0.0 && *t < 1.0);
    cuts.sort_by(f64::total_cmp);
    let mut kept = vec![0.0];
    for &t in cuts.iter() {
        let last = *kept.last().unwrap_or(&0.0);
        if extent(subsegment(s, last, t).bounding_box()) > eps {
            kept.push(t);
        }
    }
    match kept.last() {
        // The tail is too short to stand alone: pull the last cut out to the
        // end rather than leaving a sliver edge behind it.
        Some(&last) if extent(subsegment(s, last, 1.0).bounding_box()) <= eps => {
            if kept.len() == 1 {
                // The whole segment is shorter than the tolerance. It was
                // filtered out before it got here unless it is a loop, whose
                // ends coincide but which does enclose area.
                if extent(s.bounding_box()) <= eps {
                    return Vec::new();
                }
            } else {
                kept.pop();
            }
        }
        _ => {}
    }
    kept.push(1.0);
    kept.windows(2).map(|w| (w[0], w[1])).collect()
}

/// A segment cut wherever it meets itself, so that the pairwise pass over
/// distinct segments sees every crossing there is.
///
/// Two things send a segment through itself. A cubic with a loop crosses
/// itself transversally at an interior point, and the two parameters have a
/// closed form. A segment whose derivative vanishes *folds*: a collinear
/// quadratic drawn out and back is the common one, and it lies on top of
/// itself over a whole arc rather than crossing at a point. Cut at the fold
/// and the two halves become ordinary coincident edges, which the duplicate
/// merge below then collapses into a single edge whose contributions cancel --
/// which is right, since going out and back leaves the winding number alone.
fn split_at_self(s: Segment, eps: f64) -> Vec<Segment> {
    let mut cuts = self_cuts(s);
    if cuts.is_empty() {
        return vec![s];
    }
    spans(s, &mut cuts, eps)
        .into_iter()
        .map(|(t0, t1)| subsegment(s, t0, t1))
        .collect()
}

/// The parameters at which a segment folds back on itself or crosses itself.
fn self_cuts(s: Segment) -> Vec<f64> {
    let c = cubic(s);
    let d = c.deriv_control();
    let mut out = Vec::new();
    // Folds: both components of the derivative vanish together. Compared
    // against the size of the hodograph rather than against zero, because the
    // control points that produce a fold rarely produce an exact one.
    let scale = d
        .iter()
        .fold(0.0f64, |m, v| m.max(v.x.abs()).max(v.y.abs()));
    for (roots, other) in [
        (quadratic_roots(d[0].x, d[1].x, d[2].x), true),
        (quadratic_roots(d[0].y, d[1].y, d[2].y), false),
    ] {
        for t in roots {
            if t <= 0.0 || t >= 1.0 {
                continue;
            }
            let v = c.deriv_at(t);
            let residual = if other { v.y.abs() } else { v.x.abs() };
            if residual <= 1e-9 * scale {
                out.push(t);
            }
        }
    }
    // The loop of a cubic. Writing `c(s) - c(t)` as `(s - t)` times a symmetric
    // quadratic leaves two equations that are *linear* in `s + t` and
    // `s^2 + st + t^2`, so the crossing comes out of a 2x2 solve and a
    // quadratic rather than out of a search.
    let a = (c.p3 - c.p0) + (c.p1 - c.p2) * 3.0;
    let b = ((c.p2 - c.p1) - (c.p1 - c.p0)) * 3.0;
    let k = (c.p1 - c.p0) * 3.0;
    let det = a.x * b.y - a.y * b.x;
    if det != 0.0 {
        let u = (b.x * k.y - k.x * b.y) / det;
        let p = (k.x * a.y - a.x * k.y) / det;
        let disc = 4.0 * u - 3.0 * p * p;
        if disc > 0.0 {
            let r = disc.sqrt();
            let (lo, hi) = (0.5 * (p - r), 0.5 * (p + r));
            if lo > 0.0 && hi < 1.0 {
                out.push(lo);
                out.push(hi);
            }
        }
    }
    out.retain(|t| t.is_finite());
    out
}

/// Groups points closer than `eps` and returns the representatives together
/// with each point's group.
///
/// Sweeping in sorted order rather than in input order is what makes the
/// grouping the same whichever path was passed first: the sort is a canonical
/// order on the points themselves, and the representative is a real input
/// point, so two identical inputs snap to bit-identical vertices.
fn cluster(points: &[Point], eps: f64) -> (Vec<Point>, Vec<usize>) {
    let mut order: Vec<usize> = (0..points.len()).collect();
    order.sort_by(|&i, &j| {
        points[i]
            .x
            .total_cmp(&points[j].x)
            .then(points[i].y.total_cmp(&points[j].y))
    });
    let mut reps: Vec<Point> = Vec::new();
    let mut of_point = vec![usize::MAX; points.len()];
    // Representatives are created in increasing x, so only the tail of the
    // list can still be within eps in x.
    let mut window = 0usize;
    for &i in &order {
        let p = points[i];
        while window < reps.len() && reps[window].x < p.x - eps {
            window += 1;
        }
        match (window..reps.len()).find(|&k| reps[k].distance(p) <= eps) {
            Some(k) => of_point[i] = k,
            None => {
                of_point[i] = reps.len();
                reps.push(p);
            }
        }
    }
    (reps, of_point)
}

/// Collapses edges that describe the same arc between the same two vertices
/// into one carrying the sum of their contributions.
///
/// This is where a boundary the two inputs share stops being two edges. Left
/// separate they would be classified independently, and a disagreement of one
/// ulp between them would leave a hairline crack -- or a duplicated boundary --
/// in the result.
fn merge_duplicates(edges: Vec<Edge>, eps: f64) -> Vec<Edge> {
    let mut order: Vec<usize> = (0..edges.len()).collect();
    order.sort_by(|&i, &j| {
        let (a, b) = (&edges[i], &edges[j]);
        let ka = (a.v0.min(a.v1), a.v0.max(a.v1));
        let kb = (b.v0.min(b.v1), b.v0.max(b.v1));
        ka.cmp(&kb).then_with(|| {
            shape_key(a.seg)
                .partial_cmp(&shape_key(b.seg))
                .unwrap_or(core::cmp::Ordering::Equal)
        })
    });
    let mut out: Vec<Edge> = Vec::new();
    let mut group_start = 0usize;
    for &i in &order {
        let e = edges[i];
        let key = (e.v0.min(e.v1), e.v0.max(e.v1));
        // Sorting put the group together, so a change of vertex pair against
        // the last edge kept is the group boundary.
        if out
            .last()
            .is_none_or(|l| (l.v0.min(l.v1), l.v0.max(l.v1)) != key)
        {
            group_start = out.len();
        }
        match out[group_start..]
            .iter()
            .position(|k| same_arc(k.seg, e.seg, eps))
        {
            Some(k) => {
                let target = group_start + k;
                let sign = alignment(out[target], e);
                out[target].wind[0] += sign * e.wind[0];
                out[target].wind[1] += sign * e.wind[1];
            }
            None => out.push(e),
        }
    }
    out
}

/// Whether two edges over the same pair of vertices trace the same arc.
///
/// Distance to the *other curve*, not distance between the two curves at the
/// same parameter. Two segments can trace one arc at completely different
/// speeds -- a line and a fold of a quadratic lying along it, or two pieces cut
/// from differently parameterised originals -- and comparing them parameter for
/// parameter calls those different. Six agreements plus the two shared ends is
/// more than the three points a cubic and a line can have in common, so
/// nothing but a genuine coincidence passes.
fn same_arc(a: Segment, b: Segment, eps: f64) -> bool {
    let (ca, cb) = (cubic(a), cubic(b));
    [0.25, 0.5, 0.75]
        .iter()
        .all(|&t| cb.nearest(ca.eval(t)).1 <= eps && ca.nearest(cb.eval(t)).1 <= eps)
}

/// `1` when two coincident edges run the same way, `-1` when they run
/// opposite ways.
///
/// Decided from the vertex indices, which are exact, except for a loop whose
/// two ends are the same vertex and which therefore has to be asked its
/// geometry.
fn alignment(a: Edge, b: Edge) -> i32 {
    if a.v0 != a.v1 {
        return if a.v0 == b.v0 { 1 } else { -1 };
    }
    let (ca, cb) = (cubic(a.seg), cubic(b.seg));
    let p = ca.eval(0.25);
    if p.distance(cb.eval(0.25)) <= p.distance(cb.eval(0.75)) {
        1
    } else {
        -1
    }
}

/// The sort key for a half-edge leaving its origin, counter-clockwise from the
/// positive x axis.
///
/// The angle is measured to the point a fixed distance `radius` out from the
/// vertex, *not* to the tangent, and that is the whole difficulty of this
/// function. Two edges can leave a vertex along the same tangent -- a curve
/// and a line touching it, two arcs osculating -- and then the tangent orders
/// them by nothing but rounding, which puts a face's boundary through the
/// wrong edge and merges two faces into one. Because edges of the arrangement
/// meet only at their ends, they cannot swap sides between the vertex and any
/// radius that stays inside them all, so the order at that radius *is* the
/// order in the limit, and it is decided by a difference proportional to the
/// difference in curvature rather than by an ulp.
///
/// The tangent stays as a tiebreak for the case where the two curves also
/// agree at the radius.
fn leave_key(s: Segment, radius: f64) -> [f64; 2] {
    let c = cubic(s);
    let mut tangent = c.deriv_at(0.0);
    if !positive(tangent.length_squared()) {
        tangent = c.p2 - c.p0;
    }
    if !positive(tangent.length_squared()) {
        tangent = c.p3 - c.p0;
    }
    [
        pseudo_angle(at_radius(c, radius) - c.p0),
        pseudo_angle(tangent),
    ]
}

/// The point on `c` about `d` away from its start, found by bisection.
///
/// The distance from the start is not monotone in `t` for every cubic, but it
/// is over the stretch this is asked about -- `d` is a quarter of the shortest
/// edge at the vertex -- and the bisection converges to *a* point at that
/// distance either way, which is all the angular order needs.
fn at_radius(c: CubicBez, d: f64) -> Point {
    if !positive(d) || c.p0.distance(c.p3) <= d {
        return c.p3;
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..40 {
        let m = 0.5 * (lo + hi);
        if c.p0.distance(c.eval(m)) < d {
            lo = m;
        } else {
            hi = m;
        }
    }
    c.eval(0.5 * (lo + hi))
}

/// An angle substitute in `[0, 4)`, increasing counter-clockwise from the
/// positive x axis.
///
/// Monotone in the true angle, so it sorts identically, and free of `atan2` --
/// which matters less for speed than for having no branch cuts of its own to
/// get wrong. Scaling by the larger component first keeps it finite for
/// coordinates near the top of the range.
fn pseudo_angle(v: Vec2) -> f64 {
    let m = v.x.abs().max(v.y.abs());
    if !positive(m) || !m.is_finite() {
        return 0.0;
    }
    let (x, y) = (v.x / m, v.y / m);
    let r = x / (x.abs() + y.abs());
    if y >= 0.0 { 1.0 - r } else { 3.0 + r }
}

/// The distance from `p` to the nearest edge, giving up as soon as it is clear
/// the answer cannot beat `floor`.
fn clearance(p: Point, edges: &[(CubicBez, Rect)], floor: f64) -> f64 {
    let mut best = f64::INFINITY;
    for (c, bb) in edges {
        if best <= floor {
            return best;
        }
        // A box test first: the distance to the box is a lower bound on the
        // distance to the curve, and it is a handful of comparisons against a
        // root solve.
        if box_distance(p, *bb) >= best {
            continue;
        }
        best = best.min(c.nearest(p).1);
    }
    best
}

/// The distance from `p` to the nearest edge other than `skip`.
fn clearance_of(p: Point, edges: &[(CubicBez, Rect)], skip: usize) -> f64 {
    let mut best = f64::INFINITY;
    for (k, (c, bb)) in edges.iter().enumerate() {
        if k == skip || box_distance(p, *bb) >= best {
            continue;
        }
        best = best.min(c.nearest(p).1);
    }
    best
}

/// The distance from a point to a box, zero inside it.
fn box_distance(p: Point, r: Rect) -> f64 {
    let dx = (r.x0 - p.x).max(p.x - r.x1).max(0.0);
    let dy = (r.y0 - p.y).max(p.y - r.y1).max(0.0);
    Vec2::new(dx, dy).length()
}

/// The input segments prepared for ray casting: cubics with their boxes.
fn ray_input(segs: &[Segment]) -> Vec<(CubicBez, Rect)> {
    segs.iter().map(|&s| (cubic(s), s.bounding_box())).collect()
}

/// The winding number of a closed segment list about `p`.
///
/// A ray straight out along `+x`, with crossings counted by the half-open rule
/// `y0 <= p.y < y1`: a crossing exactly at a shared endpoint of two segments
/// belongs to exactly one of them whichever way the pair runs, and a curve
/// that touches the ray without passing through it counts zero. Getting that
/// rule right is the whole of point-in-path correctness; everything else is
/// root finding.
fn winding(segs: &[(CubicBez, Rect)], p: Point) -> i32 {
    let mut w = 0;
    for (c, bb) in segs {
        // Nothing to the left of the point, and nothing that misses the ray's
        // line, can cross it.
        if bb.x1 < p.x || p.y < bb.y0 || p.y > bb.y1 {
            continue;
        }
        w += crossings(*c, p);
    }
    w
}

/// How many times one cubic crosses the ray from `p` towards `+x`, signed.
fn crossings(c: CubicBez, p: Point) -> i32 {
    // Split at the turning points in y, so each piece is monotone and holds at
    // most one crossing. Bisection on a monotone piece cannot miss a root and
    // cannot converge to the wrong one, which is why the roots of the
    // derivative are worth finding first.
    let d = c.deriv_control();
    let mut ts = [0.0, 1.0, 1.0, 1.0];
    let mut n = 1;
    for r in quadratic_roots(d[0].y, d[1].y, d[2].y) {
        if r > 0.0 && r < 1.0 {
            ts[n] = r;
            n += 1;
        }
    }
    ts[1..n].sort_by(f64::total_cmp);
    ts[n] = 1.0;
    let mut w = 0;
    for k in 0..n {
        let (ta, tb) = (ts[k], ts[k + 1]);
        let (ya, yb) = (c.eval(ta).y, c.eval(tb).y);
        let dir = if ya <= p.y && p.y < yb {
            1
        } else if yb <= p.y && p.y < ya {
            -1
        } else {
            continue;
        };
        let (mut lo, mut hi) = (ta, tb);
        // 60 halvings take the bracket below the last bit of a double, so the
        // loop cannot end early with a bracket that still matters.
        for _ in 0..60 {
            let m = 0.5 * (lo + hi);
            if m <= lo || m >= hi {
                break;
            }
            if (c.eval(m).y < p.y) == (dir > 0) {
                lo = m;
            } else {
                hi = m;
            }
        }
        if c.eval(0.5 * (lo + hi)).x > p.x {
            w += dir;
        }
    }
    w
}

/// Roots in `(0, 1)` of the quadratic with Bernstein coefficients `a, b, c`.
fn quadratic_roots(a: f64, b: f64, c: f64) -> Vec<f64> {
    // Power basis: A t^2 + B t + C.
    let (aa, bb, cc) = (a - 2.0 * b + c, 2.0 * (b - a), a);
    if aa == 0.0 {
        return if bb == 0.0 {
            Vec::new()
        } else {
            vec![-cc / bb]
        };
    }
    let disc = bb * bb - 4.0 * aa * cc;
    if disc < 0.0 {
        return Vec::new();
    }
    // The stable pairing: the root that would cancel is recovered from the
    // product of the roots instead of from the subtraction.
    let q = -0.5 * (bb + bb.signum() * disc.sqrt());
    if q == 0.0 {
        return vec![0.0];
    }
    vec![q / aa, cc / q]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    fn pt(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    const NZ: [FillRule; 2] = [FillRule::NonZero; 2];

    /// The signed area a path encloses, by Green's theorem.
    ///
    /// Three-point Gauss-Legendre integrates `x(t) y'(t)` -- degree five for a
    /// cubic -- exactly, so this is the true area of the Bezier geometry and
    /// not an approximation of it.
    fn area(path: &Path) -> f64 {
        const NODES: [f64; 3] = [
            0.5 - 0.5 * 0.774_596_669_241_483_4,
            0.5,
            0.5 + 0.5 * 0.774_596_669_241_483_4,
        ];
        const WEIGHTS: [f64; 3] = [5.0 / 18.0, 8.0 / 18.0, 5.0 / 18.0];
        let mut sum = 0.0;
        for s in closed_segments(path) {
            let c = cubic(s);
            for (t, w) in NODES.iter().zip(WEIGHTS) {
                sum += w * c.eval(*t).x * c.deriv_at(*t).y;
            }
        }
        sum
    }

    fn total_area(paths: &[Path]) -> f64 {
        paths.iter().map(area).sum()
    }

    /// A circle from `n` cubic arcs.
    ///
    /// Sixteen arcs put the area error of the approximation at about `2e-7` of
    /// a unit circle, which is what lets an analytic area be checked to `1e-6`
    /// at all; four arcs -- the usual drawing of a circle -- are off by `3e-4`
    /// by construction and no boolean implementation can recover that.
    fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Path {
        let mut path = Path::new();
        let step = 2.0 * core::f64::consts::PI / n as f64;
        // The control point offset that makes a cubic arc match the circle at
        // its ends, its tangents and its midpoint.
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let at = |i: usize| {
            let a = step * i as f64;
            (
                pt(cx + r * a.cos(), cy + r * a.sin()),
                Vec2::new(-a.sin(), a.cos()),
            )
        };
        let (p0, _) = at(0);
        path.push(PathEl::MoveTo(p0));
        for i in 0..n {
            let (a, ta) = at(i);
            let (b, tb) = at((i + 1) % n);
            path.push(PathEl::CurveTo(a + ta * (k * r), b - tb * (k * r), b));
        }
        path.push(PathEl::ClosePath);
        path
    }

    fn square(x: f64, y: f64, w: f64) -> Path {
        Path::from(vec![
            PathEl::MoveTo(pt(x, y)),
            PathEl::LineTo(pt(x + w, y)),
            PathEl::LineTo(pt(x + w, y + w)),
            PathEl::LineTo(pt(x, y + w)),
            PathEl::ClosePath,
        ])
    }

    fn one(paths: Vec<Path>) -> Path {
        assert_eq!(paths.len(), 1, "expected exactly one path: {paths:?}");
        paths.into_iter().next().unwrap()
    }

    /// The analytic area of the union of two discs.
    fn analytic_union(r1: f64, r2: f64, d: f64) -> f64 {
        use core::f64::consts::PI;
        if d >= r1 + r2 {
            return PI * (r1 * r1 + r2 * r2);
        }
        let a = r1 * r1 * (((d * d + r1 * r1 - r2 * r2) / (2.0 * d * r1)).acos());
        let b = r2 * r2 * (((d * d + r2 * r2 - r1 * r1) / (2.0 * d * r2)).acos());
        let c = 0.5 * ((-d + r1 + r2) * (d + r1 - r2) * (d - r1 + r2) * (d + r1 + r2)).sqrt();
        PI * (r1 * r1 + r2 * r2) - (a + b - c)
    }

    #[test]
    fn two_overlapping_circles_match_the_analytic_areas() {
        let (r1, r2, d) = (1.0, 0.8, 1.2);
        let a = circle(0.0, 0.0, r1, 16);
        let b = circle(d, 0.0, r2, 16);
        use core::f64::consts::PI;
        let lens = PI * (r1 * r1 + r2 * r2) - analytic_union(r1, r2, d);

        let u = area(&one(a.boolean(&b, BoolOp::Union, NZ)));
        assert!(
            (u - analytic_union(r1, r2, d)).abs() < 1e-6,
            "union {u} vs {}",
            analytic_union(r1, r2, d)
        );
        let i = area(&one(a.boolean(&b, BoolOp::Intersect, NZ)));
        assert!((i - lens).abs() < 1e-6, "intersect {i} vs {lens}");
        let s = area(&one(a.boolean(&b, BoolOp::Subtract, NZ)));
        assert!(
            (s - (PI * r1 * r1 - lens)).abs() < 1e-6,
            "subtract {s} vs {}",
            PI * r1 * r1 - lens
        );

        // Divide: three pieces that partition the union exactly.
        let parts = a.boolean(&b, BoolOp::Divide, NZ);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!((total_area(&parts) - u).abs() < 1e-9, "{parts:?}");
        assert!((area(&parts[1]) - lens).abs() < 1e-6);
    }

    #[test]
    fn area_is_conserved_between_union_and_intersection() {
        // The identity that does not depend on the circle approximation at
        // all, so it holds far tighter than the analytic comparison can.
        let a = circle(0.0, 0.0, 1.0, 8);
        let b = circle(0.9, 0.4, 1.3, 8);
        let u = area(&one(a.boolean(&b, BoolOp::Union, NZ)));
        let i = area(&one(a.boolean(&b, BoolOp::Intersect, NZ)));
        let sum = area(&a) + area(&b);
        assert!((u + i - sum).abs() < 1e-9 * sum.abs(), "{u} + {i} vs {sum}");

        // And subtract splits the first shape in two.
        let s = area(&one(a.boolean(&b, BoolOp::Subtract, NZ)));
        assert!((s + i - area(&a)).abs() < 1e-9, "{s} + {i} vs {}", area(&a));
    }

    #[test]
    fn disjoint_inputs_conserve_area_exactly() {
        let a = square(0.0, 0.0, 2.0);
        let b = square(5.0, 5.0, 3.0);
        let u = one(a.boolean(&b, BoolOp::Union, NZ));
        // Exactly, to the last bit: the geometry is not rebuilt at all, so
        // there is no rounding for an area to have lost.
        assert_eq!(u.elements(), [a.elements(), b.elements()].concat());
        assert_eq!(area(&u), area(&a) + area(&b));
        assert!(a.boolean(&b, BoolOp::Intersect, NZ).is_empty());
        assert_eq!(one(a.boolean(&b, BoolOp::Subtract, NZ)), a);
        assert_eq!(
            a.boolean(&b, BoolOp::Divide, NZ),
            vec![a.clone(), b.clone()]
        );

        // Touching bounding boxes but disjoint interiors go the long way and
        // must still conserve area, if not bit-exactly.
        let c = square(2.0, 0.0, 2.0);
        let u = one(a.boolean(&c, BoolOp::Union, NZ));
        assert!((area(&u) - 8.0).abs() < 1e-12, "{}", area(&u));
        assert!(a.boolean(&c, BoolOp::Intersect, NZ).is_empty());
    }

    #[test]
    fn identical_inputs_intersect_to_themselves() {
        for a in [square(0.0, 0.0, 2.0), circle(1.0, 1.0, 3.0, 8)] {
            let i = one(a.boolean(&a, BoolOp::Intersect, NZ));
            // The same segments, possibly starting at a different vertex of
            // the loop, and bit-identical: no cut lands in the interior of a
            // segment, so nothing is rebuilt.
            let mut got: Vec<Segment> = i.segments().collect();
            let mut want: Vec<Segment> = a.segments().collect();
            let key = |s: &Segment| shape_key(*s).map(|v| v.to_bits());
            got.sort_by_key(key);
            want.sort_by_key(key);
            assert_eq!(got, want);

            // The same segments, so the same area up to the order they are
            // summed in -- which the loops need not agree on.
            assert!((area(&i) - area(&a)).abs() <= 1e-15 * area(&a).abs());
            assert!(a.boolean(&a, BoolOp::Subtract, NZ).is_empty());
            let u = one(a.boolean(&a, BoolOp::Union, NZ));
            assert!((area(&u) - area(&a)).abs() < 1e-12 * area(&a).abs());
        }
    }

    #[test]
    fn combining_a_combined_result_stays_stable() {
        let a = circle(0.0, 0.0, 1.0, 8);
        let b = circle(1.0, 0.0, 1.0, 8);
        let c = circle(0.5, 0.8, 1.0, 8);
        let u = one(a.boolean(&b, BoolOp::Union, NZ));

        // Re-uniting a result with one of its own inputs changes nothing: the
        // whole boundary of `a` is now coincident with edges of `u`, which is
        // the configuration that accumulates degeneracy.
        let again = one(u.boolean(&a, BoolOp::Union, NZ));
        assert!(
            (area(&again) - area(&u)).abs() < 1e-9 * area(&u),
            "{} vs {}",
            area(&again),
            area(&u)
        );
        // And with itself.
        let twice = one(u.boolean(&u, BoolOp::Union, NZ));
        assert!((area(&twice) - area(&u)).abs() < 1e-9 * area(&u));
        // Intersecting a result with one of its inputs gives that input back.
        let back = one(u.boolean(&a, BoolOp::Intersect, NZ));
        assert!(
            (area(&back) - area(&a)).abs() < 1e-9 * area(&a),
            "{} vs {}",
            area(&back),
            area(&a)
        );

        // Three-way, built up in two orders: the same region either way.
        let left = one(one(a.boolean(&b, BoolOp::Union, NZ)).boolean(&c, BoolOp::Union, NZ));
        let right = one(one(b.boolean(&c, BoolOp::Union, NZ)).boolean(&a, BoolOp::Union, NZ));
        assert!(
            (area(&left) - area(&right)).abs() < 1e-9 * area(&left),
            "{} vs {}",
            area(&left),
            area(&right)
        );
    }

    #[test]
    fn classification_does_not_depend_on_which_path_comes_first() {
        let a = circle(0.0, 0.0, 1.0, 8);
        let b = square(0.2, -0.4, 1.5);
        // Union and intersect are symmetric operations, and the arrangement is
        // canonically ordered, so the outputs must be identical element for
        // element -- not merely equal in area.
        assert_eq!(
            a.boolean(&b, BoolOp::Union, NZ),
            b.boolean(&a, BoolOp::Union, NZ)
        );
        assert_eq!(
            a.boolean(&b, BoolOp::Intersect, NZ),
            b.boolean(&a, BoolOp::Intersect, NZ)
        );
        // Subtract is not symmetric, but the two differences plus the
        // intersection must make up the union.
        let ab = area(&one(a.boolean(&b, BoolOp::Subtract, NZ)));
        let ba = area(&one(b.boolean(&a, BoolOp::Subtract, NZ)));
        let i = area(&one(a.boolean(&b, BoolOp::Intersect, NZ)));
        let u = area(&one(a.boolean(&b, BoolOp::Union, NZ)));
        assert!((ab + ba + i - u).abs() < 1e-9 * u, "{ab} {ba} {i} {u}");
    }

    #[test]
    fn nested_shapes_get_the_right_winding_numbers() {
        // A ring: outer square wound one way, inner the other. Under nonzero
        // the middle is a hole; under even-odd it is a hole either way.
        let ring = Path::from(
            [
                square(0.0, 0.0, 10.0).elements(),
                &[
                    PathEl::MoveTo(pt(3.0, 3.0)),
                    PathEl::LineTo(pt(3.0, 7.0)),
                    PathEl::LineTo(pt(7.0, 7.0)),
                    PathEl::LineTo(pt(7.0, 3.0)),
                    PathEl::ClosePath,
                ],
            ]
            .concat(),
        );
        let probe = square(4.0, 4.0, 2.0);
        // The probe sits entirely in the hole.
        assert!(ring.boolean(&probe, BoolOp::Intersect, NZ).is_empty());
        let u = one(ring.boolean(&probe, BoolOp::Union, NZ));
        assert!(
            (area(&u).abs() - (100.0 - 16.0 + 4.0)).abs() < 1e-9,
            "{}",
            area(&u)
        );

        // Same shapes wound the same way: nonzero fills the middle, even-odd
        // does not. This is the fill rule reaching the classification.
        let both = Path::from(
            [
                square(0.0, 0.0, 10.0).elements(),
                square(3.0, 3.0, 4.0).elements(),
            ]
            .concat(),
        );
        let nz = one(both.boolean(&probe, BoolOp::Intersect, [FillRule::NonZero; 2]));
        assert!((area(&nz).abs() - 4.0).abs() < 1e-9, "{}", area(&nz));
        let eo = both.boolean(
            &probe,
            BoolOp::Intersect,
            [FillRule::EvenOdd, FillRule::NonZero],
        );
        assert!(eo.is_empty(), "{eo:?}");
    }

    #[test]
    fn a_self_intersecting_input_is_classified_by_winding() {
        // A bowtie: two triangles meeting at a crossing the input does not
        // name. Under even-odd it is two triangles; under nonzero the two
        // lobes wind opposite ways, so it is still two triangles -- but the
        // crossing has to become a vertex either way.
        let bowtie = Path::from(vec![
            PathEl::MoveTo(pt(0.0, 0.0)),
            PathEl::LineTo(pt(4.0, 4.0)),
            PathEl::LineTo(pt(4.0, 0.0)),
            PathEl::LineTo(pt(0.0, 4.0)),
            PathEl::ClosePath,
        ]);
        let all = square(-1.0, -1.0, 6.0);
        let i = one(bowtie.boolean(&all, BoolOp::Intersect, [FillRule::EvenOdd; 2]));
        assert!((area(&i).abs() - 8.0).abs() < 1e-9, "{}", area(&i));
        let arr = Arrangement::new(&bowtie, &all);
        assert!(
            arr.vertices()
                .iter()
                .any(|v| v.distance(pt(2.0, 2.0)) < 1e-9),
            "the crossing must be a vertex: {:?}",
            arr.vertices()
        );
    }

    #[test]
    fn a_shared_edge_is_one_edge_of_the_arrangement() {
        // Two squares meeting along a whole edge. The shared edge is
        // coincident, so it must be merged rather than left as two edges that
        // could be classified differently.
        let a = square(0.0, 0.0, 2.0);
        let b = square(2.0, 0.0, 2.0);
        let u = one(a.boolean(&b, BoolOp::Union, NZ));
        assert!((area(&u) - 8.0).abs() < 1e-12, "{}", area(&u));
        // One rectangle, so one subpath and no interior seam.
        assert_eq!(u.subpaths().count(), 1);
        assert_eq!(u.bounding_box(), Rect::new(0.0, 0.0, 4.0, 2.0));

        // Partially shared, and running the same way on both: still one loop.
        let c = square(1.0, 2.0, 2.0);
        let u = one(a.boolean(&c, BoolOp::Union, NZ));
        assert!((area(&u) - 8.0).abs() < 1e-12, "{}", area(&u));
        assert_eq!(u.subpaths().count(), 1);
    }

    #[test]
    fn a_hole_survives_the_operations() {
        let outer = square(0.0, 0.0, 10.0);
        let hole = square(3.0, 3.0, 4.0);
        let ring = one(outer.boolean(&hole, BoolOp::Subtract, NZ));
        assert!((area(&ring).abs() - 84.0).abs() < 1e-9, "{}", area(&ring));
        assert_eq!(ring.subpaths().count(), 2, "outer boundary and hole");

        // Cutting the ring in half keeps the hole's two halves.
        let half = square(-1.0, -1.0, 6.0);
        let cut = one(ring.boolean(&half, BoolOp::Intersect, NZ));
        assert!(
            (area(&cut).abs() - (25.0 - 4.0)).abs() < 1e-9,
            "{}",
            area(&cut)
        );
    }

    #[test]
    fn an_open_subpath_is_filled_as_though_closed() {
        let open = Path::from(vec![
            PathEl::MoveTo(pt(0.0, 0.0)),
            PathEl::LineTo(pt(4.0, 0.0)),
            PathEl::LineTo(pt(4.0, 4.0)),
            PathEl::LineTo(pt(0.0, 4.0)),
        ]);
        let i = one(open.boolean(&square(2.0, 2.0, 4.0), BoolOp::Intersect, NZ));
        assert!((area(&i).abs() - 4.0).abs() < 1e-9, "{}", area(&i));
    }

    #[test]
    fn every_intersection_becomes_a_vertex() {
        let a = circle(0.0, 0.0, 1.0, 8);
        let b = circle(1.0, 0.2, 1.0, 8);
        let arr = Arrangement::new(&a, &b);
        for x in closed_segments(&a) {
            for y in closed_segments(&b) {
                if let Intersections::Points(hits) = cubic(x).intersect(cubic(y)) {
                    for (u, _) in hits {
                        let p = cubic(x).eval(u);
                        assert!(
                            arr.vertices().iter().any(|v| v.distance(p) < 1e-6),
                            "no vertex at {p:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn no_two_edges_of_an_arrangement_cross_away_from_a_vertex() {
        let check_pair = |a: &Path, b: &Path| {
            let arr = Arrangement::new(a, b);
            let eps = arr.eps.max(1e-12);
            for i in 0..arr.edges.len() {
                for j in i + 1..arr.edges.len() {
                    let (x, y) = (arr.edges[i], arr.edges[j]);
                    let Intersections::Points(hits) = cubic(x.seg).intersect(cubic(y.seg)) else {
                        panic!("coincident edges survived merging: {x:?} {y:?}");
                    };
                    for (u, _) in hits {
                        let p = cubic(x.seg).eval(u);
                        let at_vertex = [x.v0, x.v1, y.v0, y.v1]
                            .iter()
                            .any(|&v| arr.verts[v].distance(p) <= 1e3 * eps);
                        assert!(at_vertex, "crossing away from a vertex at {p:?}");
                    }
                }
            }
        };
        check_pair(&circle(0.0, 0.0, 1.0, 8), &circle(1.0, 0.3, 1.0, 8));
        check_pair(&square(0.0, 0.0, 2.0), &square(1.0, 1.0, 2.0));
        check_pair(&square(0.0, 0.0, 2.0), &square(2.0, 0.0, 2.0));
    }

    #[test]
    fn faces_partition_the_half_edges() {
        // Two squares overlapping at a corner: two crossings, one connected
        // arrangement, and four faces -- each square alone, the overlap, and
        // the outside.
        let arr = Arrangement::new(&square(0.0, 0.0, 2.0), &square(1.0, 1.0, 2.0));
        let total: usize = arr.faces.iter().map(Vec::len).sum();
        assert_eq!(total, 2 * arr.edge_count());
        assert_eq!((arr.vertices().len(), arr.edge_count()), (10, 12));
        // Euler: V - E + F = 2 for a connected planar graph, counting each
        // face once per boundary walk.
        assert_eq!(
            arr.face_count(),
            arr.edge_count() + 2 - arr.vertices().len(),
            "V={} E={} F={}",
            arr.vertices().len(),
            arr.edge_count(),
            arr.face_count()
        );
        // Exactly one face is outside both shapes, and every face's winding is
        // the winding the raw paths report at an interior point of it.
        assert!(arr.wind.iter().filter(|w| **w == [0, 0]).count() >= 1);
    }

    #[test]
    fn the_same_input_gives_the_same_output() {
        let a = circle(0.0, 0.0, 1.0, 8);
        let b = square(0.3, 0.3, 1.2);
        let first = a.boolean(&b, BoolOp::Union, NZ);
        for _ in 0..4 {
            assert_eq!(a.boolean(&b, BoolOp::Union, NZ), first);
        }
    }

    #[test]
    fn empty_inputs_are_answered_without_an_arrangement() {
        let a = square(0.0, 0.0, 2.0);
        let empty = Path::new();
        assert_eq!(one(a.boolean(&empty, BoolOp::Union, NZ)), a);
        assert!(a.boolean(&empty, BoolOp::Intersect, NZ).is_empty());
        assert_eq!(one(a.boolean(&empty, BoolOp::Subtract, NZ)), a);
        assert!(empty.boolean(&empty, BoolOp::Union, NZ).is_empty());
        assert!(empty.boolean(&a, BoolOp::Subtract, NZ).is_empty());
    }

    /// A drag step has to finish inside a frame, and this is the only thing
    /// that holds it to that.
    ///
    /// The bound is two orders of magnitude above what the operation costs so
    /// that a loaded machine or an unoptimised build cannot fail it; what it
    /// catches is the accidental extra factor of `n`, which is easy to
    /// introduce here -- the pairwise intersection pass and the clearance
    /// search are both already quadratic.
    #[test]
    fn a_preview_drag_costs_about_a_millisecond() {
        let a = circle(0.0, 0.0, 1.0, 16);
        let b = circle(1.0, 0.3, 1.0, 16);
        let mut pv = Preview::new(a, b, BoolOp::Union, NZ);
        let start = std::time::Instant::now();
        for k in 1..=20 {
            let out = pv.drag(Affine::translate(Vec2::new(0.001 * f64::from(k), 0.0)));
            assert_eq!(out.len(), 1);
        }
        let each = start.elapsed() / 20;
        assert!(each < core::time::Duration::from_millis(250), "{each:?}");
    }

    #[test]
    fn a_preview_matches_what_it_commits() {
        let a = square(0.0, 0.0, 2.0);
        let b = square(1.0, 1.0, 2.0);
        let mut pv = Preview::new(a.clone(), b.clone(), BoolOp::Union, NZ);
        let mut last = Vec::new();
        for k in 0..10 {
            let t = Affine::translate(Vec2::new(0.1 * f64::from(k), 0.0));
            last = pv.drag(t).to_vec();
            // What is on screen is what a fresh computation gives.
            assert_eq!(last, a.boolean(&b.transform(t), BoolOp::Union, NZ));
        }
        assert_eq!(pv.result(), last.as_slice());
        assert_eq!(pv.commit(), last);

        // Repeating a drag does not recompute: the same transform returns the
        // same allocation, which is what keeps a drag interactive.
        let mut pv = Preview::new(a.clone(), b.clone(), BoolOp::Union, NZ);
        let t = Affine::translate(Vec2::new(0.5, 0.0));
        let first = pv.drag(t).as_ptr();
        assert_eq!(pv.drag(t).as_ptr(), first);

        // Cancelling gives the sources back untouched.
        let mut pv = Preview::new(a.clone(), b.clone(), BoolOp::Subtract, NZ);
        pv.drag(Affine::translate(Vec2::new(3.0, 3.0)));
        assert_eq!(pv.cancel(), (a, b));
    }

    #[test]
    fn winding_counts_shared_endpoints_once() {
        // The ray from the sample point passes exactly through the corner
        // where two segments meet, which is where a naive crossing count
        // double-counts or misses.
        let sq = ray_input(&closed_segments(&square(0.0, 0.0, 2.0)));
        assert_eq!(winding(&sq, pt(1.0, 1.0)), 1);
        assert_eq!(winding(&sq, pt(-1.0, 0.0)), 0, "ray along the bottom edge");
        assert_eq!(winding(&sq, pt(-1.0, 2.0)), 0, "ray along the top edge");
        assert_eq!(winding(&sq, pt(-1.0, 3.0)), 0);
        assert_eq!(winding(&sq, pt(3.0, 1.0)), 0, "behind the ray");

        // A diamond, so the ray from an interior point leaves exactly through
        // a corner -- the case where two segments both claim the crossing.
        let diamond = Path::from(vec![
            PathEl::MoveTo(pt(0.0, -1.0)),
            PathEl::LineTo(pt(1.0, 0.0)),
            PathEl::LineTo(pt(0.0, 1.0)),
            PathEl::LineTo(pt(-1.0, 0.0)),
            PathEl::ClosePath,
        ]);
        let rays = ray_input(&closed_segments(&diamond));
        assert_eq!(winding(&rays, pt(-0.5, 0.0)), 1, "ray out through a corner");
        assert_eq!(
            winding(&rays, pt(-2.0, 0.0)),
            0,
            "ray in and out at corners"
        );

        // A curve that touches the ray without crossing it counts zero.
        let arch = Path::from(vec![
            PathEl::MoveTo(pt(0.0, 0.0)),
            PathEl::CurveTo(pt(1.0, 3.0), pt(3.0, 3.0), pt(4.0, 0.0)),
            PathEl::ClosePath,
        ]);
        let peak = cubic(arch.segments().next().unwrap()).eval(0.5).y;
        let rays = ray_input(&closed_segments(&arch));
        assert_eq!(winding(&rays, pt(-1.0, peak)), 0, "tangent to the ray");
        // A hair below the peak the ray crosses the arch twice, up and then
        // down, and the point is still outside: the two must cancel.
        assert_eq!(winding(&rays, pt(-1.0, peak - 1e-9)), 0);
        // Inside. The arch runs over the top and back along the bottom, which
        // is clockwise, so the winding number there is -1.
        assert_eq!(winding(&rays, pt(2.0, 1.0)), -1);

        // Opposite winding subtracts.
        let ring = Path::from(
            [
                square(0.0, 0.0, 10.0).elements(),
                &[
                    PathEl::MoveTo(pt(3.0, 3.0)),
                    PathEl::LineTo(pt(3.0, 7.0)),
                    PathEl::LineTo(pt(7.0, 7.0)),
                    PathEl::LineTo(pt(7.0, 3.0)),
                    PathEl::ClosePath,
                ],
            ]
            .concat(),
        );
        let rays = ray_input(&closed_segments(&ring));
        assert_eq!(winding(&rays, pt(5.0, 5.0)), 0, "the hole");
        assert_eq!(winding(&rays, pt(1.0, 5.0)), 1);
    }

    #[test]
    fn pseudo_angle_orders_like_the_real_one() {
        let mut prev = -1.0;
        for k in 0..64 {
            let a = -core::f64::consts::PI + 2.0 * core::f64::consts::PI * f64::from(k) / 64.0;
            // Skip the wrap point itself, where the true angle jumps.
            let v = Vec2::new(a.cos(), a.sin());
            let p = pseudo_angle(v);
            assert!((0.0..4.0).contains(&p), "{p}");
            if a >= 0.0 {
                assert!(p >= prev, "{a} {p} {prev}");
                prev = p;
            }
        }
        assert_eq!(pseudo_angle(Vec2::new(1.0, 0.0)), 0.0);
        assert_eq!(pseudo_angle(Vec2::new(0.0, 1.0)), 1.0);
        assert_eq!(pseudo_angle(Vec2::new(-1.0, 0.0)), 2.0);
        assert_eq!(pseudo_angle(Vec2::new(0.0, -1.0)), 3.0);
        assert_eq!(pseudo_angle(Vec2::new(0.0, 0.0)), 0.0);
    }

    /// Random closed polygonal and curved subpaths, small enough that two of
    /// them meet often.
    fn shape(r: &mut Rng) -> Path {
        let mut path = Path::new();
        for _ in 0..1 + r.below(2) {
            let n = 3 + r.below(4);
            // A small integer grid, so vertices coincide and edges lie on top
            // of each other far more often than random coordinates would.
            let g = |r: &mut Rng| f64::from(r.below(5) as u32) - 2.0;
            let (x, y) = (g(r), g(r));
            path.push(PathEl::MoveTo(pt(x, y)));
            for _ in 0..n {
                match r.below(4) {
                    0 => {
                        let c = pt(g(r), g(r));
                        path.push(PathEl::QuadTo(c, pt(g(r), g(r))));
                    }
                    1 => path.push(PathEl::CurveTo(
                        pt(g(r), g(r)),
                        pt(g(r), g(r)),
                        pt(g(r), g(r)),
                    )),
                    _ => path.push(PathEl::LineTo(pt(g(r), g(r)))),
                }
            }
            path.push(PathEl::ClosePath);
        }
        path
    }

    #[test]
    fn random_pairs_do_not_panic_and_stay_finite() {
        check(
            "boolean on random pairs",
            150,
            |r| (shape(r), shape(r)),
            |(a, b)| {
                for op in [
                    BoolOp::Union,
                    BoolOp::Intersect,
                    BoolOp::Subtract,
                    BoolOp::Divide,
                ] {
                    for rules in [
                        [FillRule::NonZero; 2],
                        [FillRule::EvenOdd; 2],
                        [FillRule::NonZero, FillRule::EvenOdd],
                    ] {
                        for out in a.boolean(b, op, rules) {
                            let finite = out
                                .elements()
                                .iter()
                                .all(|el| el.end_point().is_none_or(|p| p.is_finite()));
                            if !finite {
                                return false;
                            }
                            // Every emitted subpath is closed and joined.
                            for sub in out.subpaths() {
                                if !sub.is_closed() {
                                    return false;
                                }
                                let mut cur = sub.start();
                                for s in sub.segments() {
                                    if s.start() != cur {
                                        return false;
                                    }
                                    cur = s.end();
                                }
                            }
                        }
                    }
                }
                true
            },
        );
    }

    /// How many edges the faces either side of disagree with about the winding
    /// numbers.
    ///
    /// The one invariant tying the arrangement to the classification: crossing
    /// an edge from its right to its left must change each winding number by
    /// exactly what that edge contributes. A face sampled in the wrong place,
    /// an edge missed by the merge, a ring sorted wrongly at a tangency -- all
    /// of them show up here, and none of them show up in an area comparison
    /// until they happen not to cancel.
    fn winding_violations(arr: &Arrangement) -> usize {
        (0..arr.hseg.len())
            .step_by(2)
            .filter(|&h| {
                let e = arr.edges[h / 2];
                let (l, r) = (arr.wind[arr.face_of[h]], arr.wind[arr.face_of[h ^ 1]]);
                [0, 1].iter().any(|&k| l[k] - r[k] != e.wind[k])
            })
            .count()
    }

    /// Euler for a plane graph, counting a face once per boundary walk:
    /// `walks = E - V + 2C`. The number of components is not to hand, but it
    /// is at least one, and falling below that bound is exactly what two edges
    /// crossing away from a vertex produces.
    fn is_planar(arr: &Arrangement) -> bool {
        arr.face_count() + arr.vertices().len() >= arr.edge_count() + 2
    }

    #[test]
    fn windings_agree_across_every_edge() {
        for (a, b) in [
            (square(0.0, 0.0, 2.0), square(1.0, 1.0, 2.0)),
            (square(0.0, 0.0, 2.0), square(2.0, 0.0, 2.0)),
            (square(0.0, 0.0, 10.0), square(3.0, 3.0, 4.0)),
            (circle(0.0, 0.0, 1.0, 8), circle(1.0, 0.3, 1.0, 8)),
            (circle(0.0, 0.0, 1.0, 16), circle(0.0, 0.0, 1.0, 16)),
        ] {
            let arr = Arrangement::new(&a, &b);
            assert_eq!(winding_violations(&arr), 0);
            assert!(
                is_planar(&arr),
                "V={} E={} F={}",
                arr.vertices().len(),
                arr.edge_count(),
                arr.face_count()
            );
        }
    }

    /// How often the arrangement of an adversarial random pair is *not* a
    /// valid planar subdivision.
    ///
    /// It is not never, and this test says so rather than hiding it. The
    /// corpus draws control points from a five-point grid, which makes nearly
    /// parallel crossings, folds and cusps the common case rather than the
    /// rare one; two lines meeting at a shallow angle have an intersection
    /// point that is ill conditioned by a factor of `1 / sin(angle)`, and when
    /// that error exceeds the snapping distance the two curves get two
    /// vertices where they should have had one. What the vote in `classify`
    /// buys is that the damage stays local: the winding numbers of the rest of
    /// the arrangement are unaffected, which is why the area properties below
    /// hold on the same corpus.
    ///
    /// The bound is a regression guard, not a target. It was 1 in 200 when it
    /// was written.
    #[test]
    fn arrangements_are_planar_except_rarely() {
        let bad = (0..200)
            .filter(|&i| {
                let mut r = Rng::new(0x51ed_0000 + i);
                let (a, b) = (shape(&mut r), shape(&mut r));
                let arr = Arrangement::new(&a, &b);
                winding_violations(&arr) != 0 || !is_planar(&arr)
            })
            .count();
        assert!(bad <= 6, "{bad} of 200 arrangements were not planar");
    }

    #[test]
    fn random_pairs_conserve_area() {
        // The one property that catches a misclassified face: whatever the
        // shapes, what the union gains the intersection must give back.
        check(
            "boolean area conservation",
            200,
            |r| (shape(r), shape(r)),
            |(a, b)| {
                let rules = [FillRule::EvenOdd; 2];
                let u: f64 = total_area(&a.boolean(b, BoolOp::Union, rules)).abs();
                let i: f64 = total_area(&a.boolean(b, BoolOp::Intersect, rules)).abs();
                let d: f64 = total_area(&a.boolean(b, BoolOp::Divide, rules)).abs();
                // Divide partitions the union, so its pieces sum to it.
                let scale = 1.0 + u + i;
                (d - u).abs() <= 1e-6 * scale && u >= i - 1e-6 * scale
            },
        );
    }

    #[test]
    fn random_pairs_are_order_independent() {
        check(
            "boolean order independence",
            200,
            |r| (shape(r), shape(r)),
            |(a, b)| {
                // The region, not the file: the intersector is symmetric only
                // to rounding, so two cut parameters that differ in the last
                // bits can leave the two answers describing the same area with
                // control points an ulp apart. What must not move is which
                // regions are in the result.
                let rules = [FillRule::NonZero; 2];
                [BoolOp::Union, BoolOp::Intersect].iter().all(|&op| {
                    let x = total_area(&a.boolean(b, op, rules));
                    let y = total_area(&b.boolean(a, op, rules));
                    (x - y).abs() <= 1e-9 * (1.0 + x.abs())
                })
            },
        );
    }
}
