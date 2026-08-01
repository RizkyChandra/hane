//! Every point two segments have in common.
//!
//! P6's boolean operations are built on this, and boolean operations do not
//! fail on the hard cases, they fail on the awkward ones: a tangency reported
//! twice becomes two vertices where the planar subdivision expects one, a
//! shared endpoint reported twice becomes a zero-length edge, and a pair of
//! coincident segments reported as points becomes an unbounded list. So the
//! contract here is about the awkward cases first and speed second.
//!
//! # The method
//!
//! Three stages, and the split between them is what makes the awkward cases
//! behave.
//!
//! **Isolate.** Both parameter domains are halved together, level by level,
//! and a pair of subdomains survives only when the two tight bounding boxes
//! still meet. Because both curves are always split, the survivors at every
//! level are cells of one uniform `2^d x 2^d` grid over `(t_self, t_other)`,
//! and the survivor set is exactly transposed when the arguments are swapped
//! -- which is where the symmetry guarantee comes from.
//!
//! **Cluster.** The surviving cells are grouped into 8-connected components.
//! *Counting happens here, not after refinement*: a transversal crossing is a
//! handful of cells, a tangency is a long band of them, and both are one
//! component either way. Deciding the count from the topology of the survivor
//! set rather than from refined coordinates is what stops a tangency being
//! reported a thousand times, and it costs nothing in the common case.
//!
//! **Refine.** Each component gets one Newton solve of `self(u) = other(v)`,
//! bracketed by the component's own cells so it cannot walk off to a
//! different intersection, and falling back to closest approach where the
//! Jacobian is singular -- a tangency, exactly. A component whose best pair
//! is still further apart than the tolerance holds no intersection at all and
//! is dropped: box overlap is conservative, and a component that saturated at
//! a coarse depth can be two arcs that merely pass near each other.
//!
//! Refinement does not decide *how many* intersections there are, only where
//! they are, and that division of labour is deliberate. A tangency's parameter
//! is genuinely ill-conditioned -- the two curves stay within `O(s^2)` of each
//! other for `s` either side of the contact -- so refined coordinates there
//! carry only a fraction of the digits a crossing's do, and a count derived
//! from them would wobble.
//!
//! Shared endpoints do not go through any of that. The four endpoint pairs are
//! tested directly, because two segments meeting end to end are the commonest
//! input a boolean operation has and the answer for them has to be exact, not
//! merely converged -- and because a pair of large steep curves can leave the
//! cells coarse enough that one component swallows both ends at once.
//!
//! # Coincident curves
//!
//! Subdivision cannot separate curves that lie on top of each other, so the
//! survivor set stops thinning and saturates. That saturation is the trigger
//! for a direct test: the ends of a shared arc can only be endpoints of one
//! curve or the other, so four projections bound the arc and a sweep of
//! samples along it confirms the two really do coincide. The answer is the
//! shared parameter range on each side, not a list of points.

use crate::{CubicBez, Point, Rect};

/// Subdivision depth cap, i.e. the finest parameter cell is `2^-24`.
///
/// Newton finishes the job from there, so this only has to isolate: it sets
/// how close two separate crossings may be before they are reported as one.
/// Cell indices must stay inside `u32`, which caps this at 31 regardless.
const MAX_DEPTH: u32 = 24;

/// Survivor cap. Reaching it means the survivors have stopped thinning out --
/// the curves are tangent or coincident over an arc -- and going deeper buys
/// resolution at `sqrt` cost while the answer is already one component.
const MAX_CELLS: usize = 1024;

/// Newton step cap. Quadratic convergence needs about six from a cell this
/// small; the rest is headroom for a poor seed inside a wide component.
const MAX_NEWTON: u32 = 20;

/// Closest-approach rounds, used only where Newton has nothing to work with.
/// Each one costs two root isolations, and the sequence is monotone in the
/// distance, so stopping early only leaves a slightly worse pair.
const MAX_APPROACH: u32 = 4;

/// Samples used to confirm a suspected coincident arc, on top of its ends.
///
/// Two distinct cubics can meet at up to nine points, so agreeing at nine
/// interior samples *and* both ends is not a coincidence any real input
/// produces.
const OVERLAP_SAMPLES: u32 = 10;

/// What two segments have in common.
#[derive(Clone, Debug, PartialEq)]
pub enum Intersections {
    /// Isolated intersections as `(t on self, t on other)` pairs, sorted by
    /// the first and deduplicated within tolerance. Empty when the segments
    /// do not meet.
    Points(Vec<(f64, f64)>),
    /// The segments run along the same arc, to within tolerance.
    Overlap {
        /// The shared arc's parameter range on `self`, with `a.0 < a.1`.
        a: (f64, f64),
        /// The same arc's range on `other`, ends corresponding to `a`'s. When
        /// `b.0 > b.1` the two segments traverse the arc in opposite
        /// directions, which is the case a boolean operation has to know
        /// about to orient the shared edge.
        b: (f64, f64),
    },
}

impl CubicBez {
    /// Every intersection with `other`.
    ///
    /// A tangency is reported once, not twice; a shared endpoint comes back
    /// as exactly `1.0` and `0.0`, so both segments agree about it to the
    /// last bit; and segments lying on top of each other are reported as the
    /// shared parameter range rather than as a list of sample points.
    ///
    /// Results are sorted by the parameter on `self` and deduplicated within
    /// tolerance. The tolerance is relative to the geometry: two points
    /// closer than about `1e-12` of the combined extent are the same point.
    ///
    /// Swapping the arguments gives the same intersections with the pairs
    /// swapped -- the same count, and the same positions to rounding. P6
    /// depends on that.
    ///
    /// Quadratics go through [`QuadBez::to_cubic`](crate::QuadBez::to_cubic),
    /// which preserves the parameterisation, so the returned `t` applies
    /// unchanged to the original quadratic.
    ///
    /// ```
    /// use hane_geom::{CubicBez, Intersections, Point};
    ///
    /// // Two straight cubics crossing at their midpoints.
    /// let line = |a: Point, b: Point| {
    ///     CubicBez::new(a, a.lerp(b, 1.0 / 3.0), a.lerp(b, 2.0 / 3.0), b)
    /// };
    /// let a = line(Point::new(0.0, 0.0), Point::new(1.0, 1.0));
    /// let b = line(Point::new(0.0, 1.0), Point::new(1.0, 0.0));
    /// let Intersections::Points(hits) = a.intersect(b) else {
    ///     panic!("crossing lines are not coincident")
    /// };
    /// assert_eq!(hits.len(), 1);
    /// assert!((hits[0].0 - 0.5).abs() < 1e-12 && (hits[0].1 - 0.5).abs() < 1e-12);
    /// ```
    pub fn intersect(self, other: Self) -> Intersections {
        let tol = tolerance(self, other);
        let (cells, depth, saturated) = isolate(self, other, tol);
        // Only worth asking once the survivors have stopped thinning: an
        // ordinary pair of curves never gets here, and the test costs four
        // root isolations plus a sweep.
        if saturated && let Some(overlap) = coincident(self, other, tol) {
            return overlap;
        }
        let Grouped {
            cells,
            label,
            bounds,
        } = components(&cells);
        // Seed each component from its own best cell rather than from the
        // centre of its bounding box: a component is a band or a staircase,
        // not a rectangle, and the centre of the box around a staircase need
        // not be anywhere near either curve. Equal gaps are broken on the
        // *unordered* cell pair, which is the same for a cell and its
        // transpose -- and exact ties are not rare here, since two evaluations
        // at nearly the same place on a curve with large coordinates round to
        // the same point.
        let h = cell_size(depth);
        let mut seeds = vec![(f64::INFINITY, (u32::MAX, u32::MAX), (0u32, 0u32)); bounds.len()];
        for (k, &(i, j)) in cells.iter().enumerate() {
            let (u, v) = ((f64::from(i) + 0.5) * h, (f64::from(j) + 0.5) * h);
            let g = gap(self, other, u, v);
            let key = (i.min(j), i.max(j));
            let seed = &mut seeds[label[k]];
            if g < seed.0 || (g == seed.0 && key < seed.1) {
                *seed = (g, key, (i, j));
            }
        }
        // Shared endpoints, which the subdivision is not allowed to be the
        // only source of. Two segments that meet end to end are the commonest
        // input P6 has, and when both curves are large and steep the cells
        // stay coarse enough that a single component can swallow both ends at
        // once. Four distance tests settle it, and `eval` is exact at 0 and 1
        // so the parameters come back exact.
        let mut hits: Vec<(f64, f64)> = [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]
            .into_iter()
            .filter(|&(u, v)| gap(self, other, u, v) <= tol)
            .collect();
        hits.extend(
            bounds
                .iter()
                .zip(&seeds)
                .filter_map(|(&c, &(_, _, seed))| refine(self, other, c, seed, depth, tol)),
        );
        hits.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
        Intersections::Points(dedup(self, other, hits, 4.0 * cell_size(depth), tol))
    }
}

/// Drops hits that describe the same intersection, keeping the closest-fitting
/// member of each group.
///
/// Grouping by the transitive closure of "same within tolerance", rather than
/// by sweeping the sorted list and comparing neighbours, is what keeps the
/// result independent of that order -- and the order changes when the
/// arguments are swapped, because the sort key does. Two hits that are
/// neighbours in one direction and separated by a third hit in the other would
/// otherwise merge in one direction only.
///
/// Both tests have to pass. Two hits at the same point but far apart in
/// parameter are two intersections, not one: that is a curve crossing itself
/// where the other curve happens to pass, and P6 needs both parameters.
fn dedup(
    a: CubicBez,
    b: CubicBez,
    hits: Vec<(f64, f64)>,
    param_tol: f64,
    tol: f64,
) -> Vec<(f64, f64)> {
    let mut parent: Vec<usize> = (0..hits.len()).collect();
    for (i, x) in hits.iter().enumerate() {
        for (j, y) in hits.iter().enumerate().skip(i + 1) {
            if (x.0 - y.0).abs() <= param_tol
                && (x.1 - y.1).abs() <= param_tol
                && a.eval(x.0).distance(a.eval(y.0)) <= tol
                && b.eval(x.1).distance(b.eval(y.1)) <= tol
            {
                union(&mut parent, i, j);
            }
        }
    }
    // The survivor of a group is its closest-fitting member, so an exact
    // shared endpoint wins over a refined point that landed a hair off it.
    // `union` keeps the smallest index as the root, which makes that index a
    // stable name for the group.
    let mut best: Vec<(usize, f64)> = Vec::new();
    let mut of_root = vec![usize::MAX; hits.len()];
    for (k, &(u, v)) in hits.iter().enumerate() {
        let root = find(&mut parent, k);
        let g = gap(a, b, u, v);
        if of_root[root] == usize::MAX {
            of_root[root] = best.len();
            best.push((k, g));
        } else if g < best[of_root[root]].1 {
            best[of_root[root]] = (k, g);
        }
    }
    // Groups are discovered in sorted order and each survivor is inside its
    // own group, but a later group's survivor can precede an earlier group's,
    // so sort once more rather than assuming.
    let mut out: Vec<(f64, f64)> = best.into_iter().map(|(k, _)| hits[k]).collect();
    out.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
    out
}

/// How close two points have to be to count as one.
///
/// Two terms because two different things go wrong. The first is the design
/// tolerance, relative to the size of the geometry so that a glyph and an
/// artboard get the same answer. The second is the rounding floor: a shape a
/// millimetre across sitting a kilometre from the origin carries absolute
/// error proportional to the *coordinate*, not to the shape, and a tolerance
/// under that would call two evaluations of the same point different.
fn tolerance(a: CubicBez, b: CubicBez) -> f64 {
    let pts = [a.p0, a.p1, a.p2, a.p3, b.p0, b.p1, b.p2, b.p3];
    let mag = pts
        .iter()
        .fold(0.0f64, |m, p| m.max(p.x.abs()).max(p.y.abs()));
    let (lo_x, hi_x) = extent(pts.map(|p| p.x));
    let (lo_y, hi_y) = extent(pts.map(|p| p.y));
    let size = (hi_x - lo_x).max(hi_y - lo_y);
    1e-12 * size + 1e-14 * mag
}

/// Smallest and largest of a coordinate set.
fn extent(vs: [f64; 8]) -> (f64, f64) {
    vs.iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        })
}

/// The width of one parameter cell at `depth`. Exact: a power of two.
#[inline]
fn cell_size(depth: u32) -> f64 {
    1.0 / f64::from(1u32 << depth)
}

/// The tight bounding box of one cell, grown by the tolerance.
///
/// The growth is not optional. [`Rect::overlaps`] is strict, so two boxes that
/// meet along an edge -- which is exactly what a tangency and a shared
/// endpoint produce -- would test as disjoint, and the intersection would be
/// pruned away at the first level.
fn cell_box(c: CubicBez, i: u32, depth: u32, tol: f64) -> Rect {
    let h = cell_size(depth);
    let t0 = f64::from(i) * h;
    // Both are exact multiples of 2^-depth, so `t0 + h` is exact and the last
    // cell ends at exactly 1.0.
    c.subsegment(t0, t0 + h).bounding_box().inflate(tol)
}

/// The distinct cell indices in `idx`, sorted, with their boxes.
fn cell_boxes(
    c: CubicBez,
    idx: impl Iterator<Item = u32>,
    depth: u32,
    tol: f64,
) -> (Vec<u32>, Vec<Rect>) {
    let mut ids: Vec<u32> = idx.collect();
    ids.sort_unstable();
    ids.dedup();
    let boxes = ids.iter().map(|&i| cell_box(c, i, depth, tol)).collect();
    (ids, boxes)
}

/// The cell pairs whose boxes still meet.
fn prune(a: CubicBez, b: CubicBez, cells: &[(u32, u32)], depth: u32, tol: f64) -> Vec<(u32, u32)> {
    let (ids_a, box_a) = cell_boxes(a, cells.iter().map(|c| c.0), depth, tol);
    let (ids_b, box_b) = cell_boxes(b, cells.iter().map(|c| c.1), depth, tol);
    cells
        .iter()
        .copied()
        .filter(|&(i, j)| {
            // Both lookups exist: the id lists were built from these cells.
            let (ka, kb) = (ids_a.binary_search(&i), ids_b.binary_search(&j));
            match (ka, kb) {
                (Ok(ka), Ok(kb)) => box_a[ka].overlaps(box_b[kb]),
                _ => false,
            }
        })
        .collect()
}

/// The surviving cells, the depth they are cells of, and whether the search
/// stopped because they stopped thinning out.
fn isolate(a: CubicBez, b: CubicBez, tol: f64) -> (Vec<(u32, u32)>, u32, bool) {
    let mut cells = prune(a, b, &[(0, 0)], 0, tol);
    let mut depth = 0;
    while depth < MAX_DEPTH && !cells.is_empty() {
        let children: Vec<(u32, u32)> = cells
            .iter()
            .flat_map(|&(i, j)| {
                [
                    (2 * i, 2 * j),
                    (2 * i, 2 * j + 1),
                    (2 * i + 1, 2 * j),
                    (2 * i + 1, 2 * j + 1),
                ]
            })
            .collect();
        let next = prune(a, b, &children, depth + 1, tol);
        if next.len() > MAX_CELLS {
            // ponytail: keep the coarser level and stop. Refinement recovers
            // the position anyway; what is lost is the ability to tell two
            // crossings this close apart. Push the cap up if P6 ever needs
            // finer separation than the cell size at saturation.
            return (cells, depth, true);
        }
        cells = next;
        depth += 1;
    }
    (cells, depth, false)
}

/// Root of `x` with path halving.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Merges two sets, always keeping the smaller index as the root so the
/// result does not depend on the order the pairs arrive in.
fn union(parent: &mut [usize], x: usize, y: usize) {
    let (rx, ry) = (find(parent, x), find(parent, y));
    if rx != ry {
        parent[rx.max(ry)] = rx.min(ry);
    }
}

/// The surviving cells grouped into components.
struct Grouped {
    /// Every surviving cell, sorted.
    cells: Vec<(u32, u32)>,
    /// The component each cell in `cells` belongs to.
    label: Vec<usize>,
    /// Each component's `[i_min, i_max, j_min, j_max]`.
    bounds: Vec<[u32; 4]>,
}

/// Connected components of the surviving cells.
///
/// Eight-connected, corners included: a transversal crossing leaves a
/// diagonal staircase of cells that touches only at the corners, and
/// four-connectivity would split it into a dozen components and report one
/// crossing a dozen times.
fn components(cells: &[(u32, u32)]) -> Grouped {
    let mut sorted = cells.to_vec();
    sorted.sort_unstable();
    let mut parent: Vec<usize> = (0..sorted.len()).collect();
    for k in 0..sorted.len() {
        let (i, j) = sorted[k];
        // Forward neighbours only; the backward ones union the same pairs
        // when their own turn comes.
        let neighbours = [
            Some((i, j + 1)),
            j.checked_sub(1).map(|d| (i + 1, d)),
            Some((i + 1, j)),
            Some((i + 1, j + 1)),
        ];
        for n in neighbours.into_iter().flatten() {
            if let Ok(m) = sorted.binary_search(&n) {
                union(&mut parent, k, m);
            }
        }
    }
    let mut of_root = vec![usize::MAX; sorted.len()];
    let mut label = vec![0usize; sorted.len()];
    let mut out: Vec<[u32; 4]> = Vec::new();
    for k in 0..sorted.len() {
        let (i, j) = sorted[k];
        let root = find(&mut parent, k);
        if of_root[root] == usize::MAX {
            of_root[root] = out.len();
            out.push([i, i, j, j]);
        } else {
            let c = &mut out[of_root[root]];
            *c = [c[0].min(i), c[1].max(i), c[2].min(j), c[3].max(j)];
        }
        label[k] = of_root[root];
    }
    Grouped {
        cells: sorted,
        label,
        bounds: out,
    }
}

/// Distance between the two curves at a parameter pair.
#[inline]
fn gap(a: CubicBez, b: CubicBez, u: f64, v: f64) -> f64 {
    a.eval(u).distance(b.eval(v))
}

/// The parameter on `c` over `[t0, t1]` nearest to `p`, in `c`'s own domain.
///
/// Restricted to the range on purpose: the global search would happily jump to
/// a closer approach somewhere else on the curve and pull the answer into a
/// different intersection.
fn local_nearest(c: CubicBez, p: Point, t0: f64, t1: f64) -> f64 {
    let (s, _) = c.subsegment(t0, t1).nearest(p);
    t0 + (t1 - t0) * s
}

/// The one intersection inside a component, or `None` when the component turns
/// out to hold no crossing after all -- box overlap is conservative, and a
/// component that saturated at a coarse depth can be two arcs that pass near
/// each other without meeting.
///
/// Newton on `a(u) - b(v) = 0` first, bracketed by the component's own cells
/// with a cell of slack, since the boxes were grown by the tolerance and the
/// root can sit just outside the cells that survived. Steps are clamped to the
/// bracket rather than rejected: a root sitting exactly on `t = 0` is
/// approached from one side only, and rejecting the overshoot would leave the
/// seed -- half a cell out -- standing as the answer.
///
/// Where that leaves a residual, the fallback is closest approach: project
/// each curve's point onto the other's arc, both at once. Both projections
/// come from the current pair rather than in sequence, which costs a little
/// convergence and buys exact symmetry under swapping the arguments. It ends
/// at the tangency contact when there is one and at the true separation when
/// there is not, which is what makes the residual test below meaningful.
fn refine(
    a: CubicBez,
    b: CubicBez,
    bounds: [u32; 4],
    seed: (u32, u32),
    depth: u32,
    tol: f64,
) -> Option<(f64, f64)> {
    let h = cell_size(depth);
    let lo_u = (f64::from(bounds[0]) * h - h).max(0.0);
    let hi_u = (f64::from(bounds[1] + 1) * h + h).min(1.0);
    let lo_v = (f64::from(bounds[2]) * h - h).max(0.0);
    let hi_v = (f64::from(bounds[3] + 1) * h + h).min(1.0);

    let (mut u, mut v) = ((f64::from(seed.0) + 0.5) * h, (f64::from(seed.1) + 0.5) * h);
    let mut best = gap(a, b, u, v);
    for _ in 0..MAX_NEWTON {
        let f = a.eval(u) - b.eval(v);
        let (da, db) = (a.deriv_at(u), b.deriv_at(v));
        // Solving [da, -db] . (du, dv) = -f by Cramer's rule. A singular
        // Jacobian -- the tangency -- makes the step infinite or NaN, and both
        // fail the improvement test below without needing a special case.
        let det = da.cross(db);
        let nu = (u + db.cross(f) / det).clamp(lo_u, hi_u);
        let nv = (v + da.cross(f) / det).clamp(lo_v, hi_v);
        let g = gap(a, b, nu, nv);
        // A step that stops improving has converged, and a NaN one -- which is
        // what a singular Jacobian produces -- is not an improvement either.
        if g.is_nan() || g >= best {
            break;
        }
        (u, v, best) = (nu, nv, g);
    }
    if best > tol {
        for _ in 0..MAX_APPROACH {
            let nu = local_nearest(a, b.eval(v), lo_u, hi_u);
            let nv = local_nearest(b, a.eval(u), lo_v, hi_v);
            let g = gap(a, b, nu, nv);
            if g.is_nan() || g >= best {
                break;
            }
            (u, v, best) = (nu, nv, g);
        }
    }
    if best.is_nan() || best > tol {
        return None;
    }

    // Snap onto an endpoint when the residual allows it. `eval` is bit-exact
    // at 0 and 1, so a shared endpoint comes back as exactly (1.0, 0.0) and
    // the two segments -- and whatever P6 stitches out of them -- agree about
    // that vertex to the last bit. Both ends snap together first: at a shared
    // endpoint neither one alone is an improvement, since moving `u` to the
    // end while `v` stays half a cell inside makes the residual worse.
    let end = |t: f64| {
        if t < h {
            0.0
        } else if t > 1.0 - h {
            1.0
        } else {
            t
        }
    };
    let (su, sv) = (end(u), end(v));
    if (su, sv) != (u, v) && gap(a, b, su, sv) <= tol {
        return Some((su, sv));
    }
    // Each single snap is judged against the *unsnapped* other parameter, so
    // that neither one depends on whether the other happened first. Trying
    // them in sequence would make the answer depend on the argument order,
    // which is the one thing this must not do.
    let u2 = if su != u && gap(a, b, su, v) <= tol {
        su
    } else {
        u
    };
    let v2 = if sv != v && gap(a, b, u, sv) <= tol {
        sv
    } else {
        v
    };
    if (u2, v2) != (u, v) && gap(a, b, u2, v2) <= tol {
        return Some((u2, v2));
    }
    Some((u, v))
}

/// The size of the arc of `c` over `[t0, t1]`, as the larger side of its box.
fn arc_size(c: CubicBez, t0: f64, t1: f64) -> f64 {
    let r = c.subsegment(t0, t1).bounding_box();
    r.width().max(r.height())
}

/// True when every sample along `c`'s arc lies on `other`.
fn arc_lies_on(c: CubicBez, t0: f64, t1: f64, other: CubicBez, tol: f64) -> bool {
    (1..OVERLAP_SAMPLES).all(|k| {
        let t = t0 + (t1 - t0) * f64::from(k) / f64::from(OVERLAP_SAMPLES);
        other.nearest(c.eval(t)).1 <= tol
    })
}

/// The shared arc of two curves that lie on top of each other, if there is
/// one.
///
/// A maximal shared arc ends where one of the two curves ends, so its four
/// candidate ends are the four endpoints and one projection each finds them.
/// Each surviving projection contributes a parameter to *both* ranges at once,
/// which is what makes the answer independent of the argument order: swapping
/// the arguments computes the same four projections and exchanges the two
/// lists it files them in.
///
/// Everything after that is confirmation, and it is not optional. Endpoints
/// landing on the other curve is also what two curves that merely cross twice
/// look like; worse, at a large relative tolerance a whole cluster of
/// endpoints can sit inside one tolerance ball, and calling that an overlap
/// invents an arc out of a blob. So the arc has to be substantially bigger
/// than the tolerance on both sides, and samples along both arcs have to land
/// on the other curve.
fn coincident(a: CubicBez, b: CubicBez, tol: f64) -> Option<Intersections> {
    let (mut ta, mut tb) = (Vec::with_capacity(4), Vec::with_capacity(4));
    for (t, p) in [(0.0, a.p0), (1.0, a.p3)] {
        let (s, d) = b.nearest(p);
        if d <= tol {
            ta.push(t);
            tb.push(s);
        }
    }
    for (t, p) in [(0.0, b.p0), (1.0, b.p3)] {
        let (s, d) = a.nearest(p);
        if d <= tol {
            ta.push(s);
            tb.push(t);
        }
    }
    if ta.len() < 2 {
        return None;
    }
    let span = |ts: &[f64]| {
        ts.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |r, &t| {
            (r.0.min(t), r.1.max(t))
        })
    };
    let (a0, a1) = span(&ta);
    let (b0, b1) = span(&tb);
    // A shared arc no bigger than the tolerance is a point, and a degenerate
    // curve -- every control point on one spot -- makes an arc of zero size
    // that coincides with everything passing through it. Both are point
    // intersections, and letting them through here would report an overlap
    // spanning a range that means nothing.
    if arc_size(a, a0, a1) <= 8.0 * tol || arc_size(b, b0, b1) <= 8.0 * tol {
        return None;
    }
    if !arc_lies_on(a, a0, a1, b, tol) || !arc_lies_on(b, b0, b1, a, tol) {
        return None;
    }
    // Which end matches which. Scoring both pairings and taking the better
    // keeps the choice the same when the arguments are swapped; deciding from
    // one end alone would not.
    let (p0, p1) = (a.eval(a0), a.eval(a1));
    let (q0, q1) = (b.eval(b0), b.eval(b1));
    let flip = p0.distance(q1) + p1.distance(q0) < p0.distance(q0) + p1.distance(q1);
    Some(Intersections::Overlap {
        a: (a0, a1),
        b: if flip { (b1, b0) } else { (b0, b1) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QuadBez;
    use crate::fuzz::{Rng, check};

    /// A straight segment as a cubic, with evenly spaced control points so
    /// that `t` is the fraction along it.
    fn line(a: Point, b: Point) -> CubicBez {
        CubicBez::new(a, a.lerp(b, 1.0 / 3.0), a.lerp(b, 2.0 / 3.0), b)
    }

    fn wiggly() -> CubicBez {
        CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 2.0),
            Point::new(3.0, -1.0),
            Point::new(4.0, 1.0),
        )
    }

    fn arch() -> CubicBez {
        CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 3.0),
            Point::new(3.0, 3.0),
            Point::new(4.0, 0.0),
        )
    }

    fn points(a: CubicBez, b: CubicBez) -> Vec<(f64, f64)> {
        match a.intersect(b) {
            Intersections::Points(p) => p,
            other => panic!("expected points, got {other:?}"),
        }
    }

    /// Every reported pair really is one point on both curves.
    fn assert_on_both(a: CubicBez, b: CubicBez, hits: &[(f64, f64)]) {
        let tol = tolerance(a, b);
        for &(u, v) in hits {
            assert!(
                (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v),
                "{u} {v}"
            );
            assert!(
                gap(a, b, u, v) <= 1e3 * tol,
                "u={u} v={v} gap={}",
                gap(a, b, u, v)
            );
        }
    }

    #[test]
    fn crossing_lines_give_exactly_one_point() {
        let a = line(Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        let b = line(Point::new(0.0, 10.0), Point::new(10.0, 0.0));
        let hits = points(a, b);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].0 - 0.5).abs() < 1e-12 && (hits[0].1 - 0.5).abs() < 1e-12);
        assert!(a.eval(hits[0].0).distance(Point::new(5.0, 5.0)) < 1e-12);

        // Off-centre, so the answer is not a symmetry artefact.
        let c = line(Point::new(2.0, 0.0), Point::new(2.0, 10.0));
        let hits = points(a, c);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].0 - 0.2).abs() < 1e-12, "{hits:?}");
        assert!((hits[0].1 - 0.2).abs() < 1e-12, "{hits:?}");
    }

    #[test]
    fn parallel_and_separated_curves_give_none() {
        // Bounding boxes overlap heavily; the curves do not meet.
        let a = wiggly();
        let b = CubicBez::new(
            Point::new(0.0, 5.0),
            Point::new(1.0, 7.0),
            Point::new(3.0, 4.0),
            Point::new(4.0, 6.0),
        );
        assert_eq!(points(a, b), vec![]);

        // An arch and a chord below it: boxes overlap, curves do not.
        let chord = line(Point::new(-1.0, -0.5), Point::new(5.0, -0.5));
        assert_eq!(points(arch(), chord), vec![]);

        // Boxes disjoint, the cheap way out.
        let far = line(Point::new(100.0, 100.0), Point::new(200.0, 200.0));
        assert_eq!(points(a, far), vec![]);
    }

    #[test]
    fn a_chord_across_an_arch_gives_two() {
        let cut = line(Point::new(-1.0, 1.0), Point::new(5.0, 1.0));
        let hits = points(arch(), cut);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].0 < hits[1].0, "sorted by t");
        assert_on_both(arch(), cut, &hits);
        // Symmetric curve, symmetric chord: the two crossings mirror.
        assert!((hits[0].0 + hits[1].0 - 1.0).abs() < 1e-9, "{hits:?}");
    }

    #[test]
    fn a_wave_crossing_a_line_three_times() {
        // A cubic with three crossings of the x axis.
        let wave = CubicBez::new(
            Point::new(0.0, -1.0),
            Point::new(1.0, 6.0),
            Point::new(2.0, -6.0),
            Point::new(3.0, 1.0),
        );
        let axis = line(Point::new(-1.0, 0.0), Point::new(4.0, 0.0));
        let hits = points(wave, axis);
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(hits[0].0 < hits[1].0 && hits[1].0 < hits[2].0);
        assert_on_both(wave, axis, &hits);
        for &(u, _) in &hits {
            assert!(wave.eval(u).y.abs() < 1e-12, "u={u}");
        }
    }

    #[test]
    fn a_tangency_is_found_once() {
        // The arch peaks at y = 2.25; a horizontal line there touches it.
        let peak = arch().eval(0.5).y;
        let tangent = line(Point::new(-1.0, peak), Point::new(5.0, peak));
        let hits = points(arch(), tangent);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!((hits[0].0 - 0.5).abs() < 1e-6, "{hits:?}");
        assert_on_both(arch(), tangent, &hits);

        // Two curves osculating at a point rather than crossing: a circle-like
        // arch and its mirror image touching at the apex.
        let up = arch();
        let down = CubicBez::new(
            Point::new(0.0, 4.5),
            Point::new(1.0, 1.5),
            Point::new(3.0, 1.5),
            Point::new(4.0, 4.5),
        );
        let hits = points(up, down);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!((hits[0].0 - 0.5).abs() < 1e-4, "{hits:?}");
    }

    #[test]
    fn a_shared_endpoint_is_reported_once_and_exactly() {
        let a = wiggly();
        // Starts where `a` ends, heading away.
        let b = CubicBez::new(
            a.p3,
            Point::new(6.0, 3.0),
            Point::new(8.0, 0.0),
            Point::new(9.0, 2.0),
        );
        let hits = points(a, b);
        assert_eq!(hits, vec![(1.0, 0.0)], "{hits:?}");
        assert_eq!(a.eval(hits[0].0), b.eval(hits[0].1));

        // Both ends shared, and the curves apart in between.
        let there = arch();
        let back = CubicBez::new(
            there.p3,
            Point::new(3.0, -3.0),
            Point::new(1.0, -3.0),
            there.p0,
        );
        let hits = points(there, back);
        assert_eq!(hits, vec![(0.0, 1.0), (1.0, 0.0)], "{hits:?}");

        // A tangential meeting at a shared endpoint: `b` leaves along `a`'s
        // outgoing tangent, which is the case that smears over many cells.
        let smooth = CubicBez::new(
            a.p3,
            a.p3 + (a.p3 - a.p2),
            Point::new(8.0, 0.0),
            Point::new(9.0, 3.0),
        );
        let hits = points(a, smooth);
        assert_eq!(hits, vec![(1.0, 0.0)], "{hits:?}");
    }

    #[test]
    fn an_endpoint_landing_mid_curve_is_exact_on_one_side() {
        let a = arch();
        let mid = a.eval(0.5);
        let b = line(mid, Point::new(mid.x + 3.0, mid.y + 3.0));
        let hits = points(a, b);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].1, 0.0, "b's own endpoint must come back exact");
        assert!((hits[0].0 - 0.5).abs() < 1e-9, "{hits:?}");
    }

    #[test]
    fn identical_curves_report_an_overlap() {
        let a = wiggly();
        assert_eq!(
            a.intersect(a),
            Intersections::Overlap {
                a: (0.0, 1.0),
                b: (0.0, 1.0)
            }
        );

        // Reversed: the same arc, traversed the other way.
        let rev = CubicBez::new(a.p3, a.p2, a.p1, a.p0);
        match a.intersect(rev) {
            Intersections::Overlap { a: ra, b: rb } => {
                assert!(
                    (ra.0 - 0.0).abs() < 1e-9 && (ra.1 - 1.0).abs() < 1e-9,
                    "{ra:?}"
                );
                assert!(
                    (rb.0 - 1.0).abs() < 1e-9 && (rb.1 - 0.0).abs() < 1e-9,
                    "{rb:?}"
                );
            }
            other => panic!("expected an overlap, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_overlap_reports_the_shared_range() {
        let c = wiggly();
        // Two halves of one curve that share the middle third.
        let a = c.subsegment(0.0, 0.6);
        let b = c.subsegment(0.4, 1.0);
        match a.intersect(b) {
            Intersections::Overlap { a: ra, b: rb } => {
                // `a` covers [0, 0.6] of c, so c(0.4) sits at 2/3 of `a`.
                assert!((ra.0 - 2.0 / 3.0).abs() < 1e-6, "{ra:?}");
                assert!((ra.1 - 1.0).abs() < 1e-9, "{ra:?}");
                assert!(rb.0.abs() < 1e-9, "{rb:?}");
                // c(0.6) sits at 1/3 of `b`.
                assert!((rb.1 - 1.0 / 3.0).abs() < 1e-6, "{rb:?}");
            }
            other => panic!("expected an overlap, got {other:?}"),
        }

        // One contained wholly in the other.
        let inner = c.subsegment(0.25, 0.75);
        match c.intersect(inner) {
            Intersections::Overlap { a: ra, b: rb } => {
                assert!(
                    (ra.0 - 0.25).abs() < 1e-6 && (ra.1 - 0.75).abs() < 1e-6,
                    "{ra:?}"
                );
                assert!(rb.0.abs() < 1e-9 && (rb.1 - 1.0).abs() < 1e-9, "{rb:?}");
            }
            other => panic!("expected an overlap, got {other:?}"),
        }
    }

    #[test]
    fn touching_but_not_coincident_curves_are_not_an_overlap() {
        // Both endpoints shared, so the endpoint test that opens the overlap
        // search fires -- but the curves bulge apart in between.
        let there = arch();
        let back = CubicBez::new(
            there.p0,
            Point::new(1.0, -3.0),
            Point::new(3.0, -3.0),
            there.p3,
        );
        let hits = points(there, back);
        assert_eq!(hits, vec![(0.0, 0.0), (1.0, 1.0)], "{hits:?}");
    }

    #[test]
    fn collinear_overlapping_segments_are_an_overlap() {
        // The case boolean ops hit constantly: two straight edges along the
        // same line, overlapping in the middle.
        let a = line(Point::new(0.0, 0.0), Point::new(10.0, 0.0));
        let b = line(Point::new(4.0, 0.0), Point::new(14.0, 0.0));
        match a.intersect(b) {
            Intersections::Overlap { a: ra, b: rb } => {
                assert!(
                    (ra.0 - 0.4).abs() < 1e-6 && (ra.1 - 1.0).abs() < 1e-9,
                    "{ra:?}"
                );
                assert!(rb.0.abs() < 1e-9 && (rb.1 - 0.6).abs() < 1e-6, "{rb:?}");
            }
            other => panic!("expected an overlap, got {other:?}"),
        }

        // Collinear but disjoint: no overlap and no points.
        let far = line(Point::new(20.0, 0.0), Point::new(30.0, 0.0));
        assert_eq!(points(a, far), vec![]);

        // Collinear and meeting end to end: one point, not an overlap.
        let next = line(Point::new(10.0, 0.0), Point::new(20.0, 0.0));
        assert_eq!(points(a, next), vec![(1.0, 0.0)]);
    }

    /// The measured near-tangency behaviour: how far apart two crossings have
    /// to be before they are resolved as two rather than one.
    #[test]
    fn near_tangencies_are_neither_missed_nor_duplicated() {
        let a = arch();
        let peak = a.eval(0.5).y;
        // A chord just below the apex crosses twice; just above, not at all.
        // Never more than twice, whatever the offset.
        for k in 0..60 {
            let d = 10.0f64.powi(-k / 4 - 1);
            for &sign in &[-1.0, 1.0] {
                let y = peak + sign * d;
                let chord = line(Point::new(-1.0, y), Point::new(5.0, y));
                let hits = points(a, chord);
                assert!(hits.len() <= 2, "d={d} sign={sign} {hits:?}");
                assert_on_both(a, chord, &hits);
                if sign < 0.0 {
                    // Below the apex there really are two crossings; deep
                    // enough into the tolerance they read as one, and that is
                    // the documented resolution limit, but they never vanish.
                    assert!(!hits.is_empty(), "missed a crossing at d={d}");
                }
            }
        }
        // Where the transition sits, checked rather than assumed: a clearly
        // separated pair resolves as two.
        let y = peak - 1e-4;
        let chord = line(Point::new(-1.0, y), Point::new(5.0, y));
        assert_eq!(points(a, chord).len(), 2);
    }

    #[test]
    fn degenerate_curves_terminate_and_stay_finite() {
        let p = Point::new(2.0, 3.0);
        let dot = CubicBez::new(p, p, p, p);
        // A point on a curve: one intersection, no overlap, no hang.
        let through = line(Point::new(0.0, 3.0), Point::new(4.0, 3.0));
        let hits = points(dot, through);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!((hits[0].1 - 0.5).abs() < 1e-6, "{hits:?}");

        // A point beside a curve: nothing.
        let beside = line(Point::new(0.0, 9.0), Point::new(4.0, 9.0));
        assert_eq!(points(dot, beside), vec![]);

        // Two identical points. Every parameter pair is an intersection here
        // and no single one is the right answer, so the four ends come back
        // rather than an arbitrary pick among them. A zero-length segment is
        // degenerate input; P6 drops them before it gets this far.
        assert_eq!(
            points(dot, dot),
            vec![(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]
        );

        // A cusp against a line through it.
        let cusp = CubicBez::new(
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(-1.0, 1.0),
            Point::new(0.0, 0.0),
        );
        let cut = line(Point::new(-2.0, 0.5), Point::new(2.0, 0.5));
        let hits = points(cusp, cut);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_on_both(cusp, cut, &hits);
    }

    #[test]
    fn scale_does_not_change_the_count() {
        // The tolerance is relative, so a glyph-sized and an artboard-sized
        // copy of one configuration must agree.
        let cut = line(Point::new(-1.0, 1.0), Point::new(5.0, 1.0));
        let base = points(arch(), cut);
        for scale in [1e-6, 1e6] {
            let s = |c: CubicBez| {
                let f = |p: Point| Point::new(p.x * scale, p.y * scale);
                CubicBez::new(f(c.p0), f(c.p1), f(c.p2), f(c.p3))
            };
            let hits = points(s(arch()), s(cut));
            assert_eq!(hits.len(), base.len(), "scale={scale}");
            for (h, w) in hits.iter().zip(&base) {
                assert!((h.0 - w.0).abs() < 1e-9, "scale={scale} {hits:?}");
                assert!((h.1 - w.1).abs() < 1e-9, "scale={scale} {hits:?}");
            }
        }
    }

    #[test]
    fn a_quad_intersects_through_its_cubic() {
        // Quadratics have no `intersect` of their own; `to_cubic` preserves
        // the parameterisation, so the answer applies to the quad directly.
        let q = QuadBez::new(
            Point::new(0.0, 0.0),
            Point::new(2.0, 4.0),
            Point::new(4.0, 0.0),
        );
        let cut = line(Point::new(-1.0, 1.0), Point::new(5.0, 1.0));
        let hits = points(q.to_cubic(), cut);
        assert_eq!(hits.len(), 2, "{hits:?}");
        for &(u, v) in &hits {
            assert!(q.eval(u).distance(cut.eval(v)) < 1e-9, "u={u}");
        }
    }

    #[test]
    fn the_same_input_gives_the_same_output() {
        let cut = line(Point::new(-1.0, 1.0), Point::new(5.0, 1.0));
        let first = arch().intersect(cut);
        for _ in 0..4 {
            assert_eq!(arch().intersect(cut), first);
        }
    }

    /// The largest coordinate in play, which is what a relative comparison has
    /// to be measured against.
    fn magnitude(c: CubicBez, d: CubicBez) -> f64 {
        [c.p0, c.p1, c.p2, c.p3, d.p0, d.p1, d.p2, d.p3]
            .iter()
            .fold(1.0f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
    }

    /// `a.intersect(b)` and `b.intersect(a)` describe the same thing.
    fn symmetric(a: CubicBez, b: CubicBez) -> bool {
        let scale = magnitude(a, b);
        match (a.intersect(b), b.intersect(a)) {
            (Intersections::Points(mut p), Intersections::Points(mut q)) => {
                if p.len() != q.len() {
                    return false;
                }
                // The reverse call is sorted by the parameter on `b`.
                p.sort_by(|x, y| x.0.total_cmp(&y.0));
                q.sort_by(|x, y| x.1.total_cmp(&y.1));
                p.iter().zip(&q).all(|(x, y)| {
                    a.eval(x.0).distance(a.eval(y.1)) <= 1e-9 * scale
                        && b.eval(x.1).distance(b.eval(y.0)) <= 1e-9 * scale
                })
            }
            (Intersections::Overlap { a: pa, b: pb }, Intersections::Overlap { a: qa, b: qb }) => {
                // The same arc: the ends may be listed in either order when
                // the two run opposite ways, so compare as a set of points.
                let ends = |c: CubicBez, r: (f64, f64)| {
                    let (mut lo, mut hi) = (c.eval(r.0), c.eval(r.1));
                    if (lo.x, lo.y) > (hi.x, hi.y) {
                        core::mem::swap(&mut lo, &mut hi);
                    }
                    (lo, hi)
                };
                let (p0, p1) = ends(a, pa);
                let (q0, q1) = ends(a, qb);
                let (r0, r1) = ends(b, pb);
                let (s0, s1) = ends(b, qa);
                p0.distance(q0) <= 1e-9 * scale
                    && p1.distance(q1) <= 1e-9 * scale
                    && r0.distance(s0) <= 1e-9 * scale
                    && r1.distance(s1) <= 1e-9 * scale
            }
            _ => false,
        }
    }

    #[test]
    fn intersections_are_symmetric() {
        check(
            "intersection symmetry",
            2_000,
            |r| (r.cubic(), r.cubic()),
            |&(a, b)| symmetric(a, b),
        );
        // Random pairs rarely meet, so force the cases that matter: curves
        // sharing both endpoints, and curves sharing an arc.
        check(
            "symmetry with shared endpoints",
            2_000,
            |r| {
                let a = r.cubic();
                let b = CubicBez::new(a.p3, r.point(), r.point(), a.p0);
                (a, b)
            },
            |&(a, b)| symmetric(a, b),
        );
        check(
            "symmetry on overlapping arcs",
            500,
            |r| {
                let c = r.cubic();
                let (s, t) = (r.unit(), r.unit());
                (c, c.subsegment(s.min(t), s.max(t)))
            },
            |&(a, b)| symmetric(a, b),
        );
    }

    #[test]
    fn reported_points_really_lie_on_both_curves() {
        check(
            "intersections lie on both curves",
            2_000,
            |r| (r.cubic(), r.cubic()),
            |&(a, b)| {
                let Intersections::Points(hits) = a.intersect(b) else {
                    return true;
                };
                let scale = magnitude(a, b);
                hits.iter().all(|&(u, v)| {
                    (0.0..=1.0).contains(&u)
                        && (0.0..=1.0).contains(&v)
                        && gap(a, b, u, v) <= 1e-6 * scale
                })
            },
        );
    }

    #[test]
    fn results_are_sorted_and_distinct() {
        check(
            "sorted and distinct",
            2_000,
            |r| (r.cubic(), r.cubic()),
            |&(a, b)| {
                let Intersections::Points(hits) = a.intersect(b) else {
                    return true;
                };
                // The dedup rule, mirrored: no two neighbours may be within
                // tolerance of each other on both curves *and* within a few
                // cells on both parameters. The cell size here is the
                // finest one, so this is the weaker claim and never fails
                // for a reason the implementation would disagree with.
                let tol = tolerance(a, b);
                let param_tol = 4.0 * cell_size(MAX_DEPTH);
                hits.windows(2).all(|w| w[0].0 <= w[1].0)
                    && hits.windows(2).all(|w| {
                        a.eval(w[0].0).distance(a.eval(w[1].0)) > tol
                            || b.eval(w[0].1).distance(b.eval(w[1].1)) > tol
                            || (w[0].0 - w[1].0).abs() > param_tol
                            || (w[0].1 - w[1].1).abs() > param_tol
                    })
            },
        );
    }

    #[test]
    fn a_transversal_crossing_is_a_staircase_of_cells() {
        // The eight-connectivity claim, checked directly: four-connectivity
        // would split this into several components and report the crossing
        // several times.
        let cells = [(4, 4), (5, 5), (6, 6), (7, 7)];
        assert_eq!(components(&cells).bounds, vec![[4, 7, 4, 7]]);
        // A gap of one cell really does separate.
        let cells = [(0, 0), (1, 1), (5, 5), (6, 6)];
        assert_eq!(components(&cells).bounds, vec![[0, 1, 0, 1], [5, 6, 5, 6]]);
        // Opposite diagonal, which is what oppositely-oriented curves leave.
        let cells = [(0, 3), (1, 2), (2, 1), (3, 0)];
        assert_eq!(components(&cells).bounds, vec![[0, 3, 0, 3]]);
        assert!(components(&[]).bounds.is_empty());
    }

    #[test]
    fn a_random_seed_reproduces() {
        // The whole pipeline is deterministic, so a fixed seed drives a fixed
        // set of answers -- the property harness depends on it.
        let mut r = Rng::new(12345);
        let pairs: Vec<(CubicBez, CubicBez)> = (0..50).map(|_| (r.cubic(), r.cubic())).collect();
        let first: Vec<Intersections> = pairs.iter().map(|&(a, b)| a.intersect(b)).collect();
        let again: Vec<Intersections> = pairs.iter().map(|&(a, b)| a.intersect(b)).collect();
        assert_eq!(first, again);
    }
}
