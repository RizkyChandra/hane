//! A quadtree over document-space bounding boxes (D-006).
//!
//! Tuned for heavy incremental update. During a drag a few hundred bounding
//! boxes change every frame, so the structure never rebuilds: the path an item
//! takes down the tree is a pure function of its bounding box and the root
//! bounds, which makes removal a descent rather than a search.
//!
//! # Placement rule
//!
//! An item lives at the deepest node whose quadrant *wholly contains* its
//! bounding box, so no item is ever stored in more than one node and a query
//! never has to de-duplicate. An item straddling the midpoint of a node stops
//! there. That is the classic MX-CIF placement, and its cost is that a small
//! box lying across the root's midpoint stays at the root.
//!
//! A node only starts pushing items into children once it holds [`SPLIT`] of
//! them, so a sparse document does not allocate a node per item. Items already
//! resting at a node are never pushed down when it subdivides -- that would
//! make placement depend on insertion history, and removal could then no
//! longer find an item by descending. Instead removal checks every node on the
//! way down, which is cheap because each of those nodes holds at most a
//! handful of items.

use hane_geom::Rect;

/// Depth cap. A dense cluster of coincident items would otherwise subdivide
/// forever, since no amount of subdivision separates boxes that are equal.
/// Twelve levels is a 4096x4096 grid over the root bounds, past which the
/// cells are smaller than anything a document places distinctly.
const MAX_DEPTH: u8 = 12;

/// How many items a node holds before it starts pushing new ones into
/// children.
///
/// Measured: 32 beats 8 and 16 on every query size and every build, because a
/// node's items are one contiguous run while its children are a pointer chase
/// -- scanning 32 boxes is cheaper than the four cache misses that skipping
/// them costs. Past 32 the curve is flat. See `BENCHMARKS.md`.
const SPLIT: usize = 32;

/// Absent-child sentinel, so a node is four `u32`s rather than four `Option`s.
const NO_CHILD: u32 = u32::MAX;

/// One node, holding its items and indices into [`Quadtree::nodes`].
///
/// Children are indices rather than boxes: the whole tree is two allocations
/// plus one `Vec` per node, and a child index survives the parent `Vec`
/// reallocating.
struct Node {
    /// Child indices in quadrant order: `(-x,-y)`, `(+x,-y)`, `(-x,+y)`,
    /// `(+x,+y)`. `NO_CHILD` where the child does not exist yet.
    children: [u32; 4],
    /// The items resting at this node, each with the bounding box it was
    /// inserted with.
    items: Vec<(u32, Rect)>,
}

impl Node {
    const fn new() -> Self {
        Self {
            children: [NO_CHILD; 4],
            items: Vec::new(),
        }
    }
}

/// A spatial index of item ids keyed by document-space bounding box.
///
/// Ids are caller-chosen and opaque to the tree; it never interprets them and
/// assumes they are unique.
pub struct Quadtree {
    /// The root cell. Items outside it are still indexed, just not sorted --
    /// see [`Quadtree::insert`].
    bounds: Rect,
    /// All nodes, with the root at index 0. Never shrinks: a node that empties
    /// stays, because an empty node costs one overlap test and reclaiming it
    /// would invalidate the sibling indices.
    nodes: Vec<Node>,
}

impl Quadtree {
    /// An empty tree covering `bounds`.
    ///
    /// `bounds` should enclose the document. Items outside it are handled
    /// correctly but not accelerated, and a non-finite `bounds` degrades the
    /// whole tree to a linear scan rather than misplacing anything.
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            nodes: vec![Node::new()],
        }
    }

    /// Builds a tree from all items at once, faster than inserting them one by
    /// one.
    ///
    /// Repeated insertion walks from the root for every item and grows one
    /// node's `Vec` at a time. This partitions the whole set by quadrant in a
    /// single pass per level instead, touching each item once per level of
    /// depth over contiguous memory.
    pub fn bulk_load(bounds: Rect, items: &[(u32, Rect)]) -> Self {
        let mut tree = Self::new(bounds);
        let mut inside = Vec::with_capacity(items.len());
        for &(id, bbox) in items {
            if placeable(bounds, bbox) {
                inside.push((id, bbox));
            } else {
                tree.nodes[0].items.push((id, bbox));
            }
        }
        let mut scratch = vec![(0u32, Rect::ZERO); inside.len()];
        // One node per few items, so the node array does not spend the build
        // doubling itself.
        tree.nodes.reserve(1 + inside.len() / (SPLIT / 2));
        tree.build(0, bounds, 0, &mut inside, &mut scratch);
        tree
    }

    /// Recursively partitions `src` (all placeable within `rect`) into the
    /// subtree rooted at `ni`. `dst` is scratch of the same length.
    ///
    /// The two buffers swap roles on the way down rather than copying the
    /// partition back, so a level costs one pass over the items and not two.
    fn build(
        &mut self,
        ni: usize,
        rect: Rect,
        depth: u8,
        src: &mut [(u32, Rect)],
        dst: &mut [(u32, Rect)],
    ) {
        if src.len() <= SPLIT || depth == MAX_DEPTH {
            self.nodes[ni].items.extend_from_slice(src);
            return;
        }
        // Key 4 is "straddles the midpoint", i.e. stays at this node.
        let mut counts = [0usize; 5];
        for &(_, bbox) in src.iter() {
            counts[quadrant(rect, bbox).unwrap_or(4)] += 1;
        }
        let mut starts = [0usize; 5];
        let mut acc = 0;
        for (start, count) in starts.iter_mut().zip(counts) {
            *start = acc;
            acc += count;
        }
        let mut cursor = starts;
        for &item in src.iter() {
            let k = quadrant(rect, item.1).unwrap_or(4);
            dst[cursor[k]] = item;
            cursor[k] += 1;
        }
        self.nodes[ni].items.extend_from_slice(&dst[starts[4]..]);
        for q in 0..4 {
            if counts[q] == 0 {
                continue;
            }
            let child = self.nodes.len() as u32;
            self.nodes.push(Node::new());
            self.nodes[ni].children[q] = child;
            let (lo, hi) = (starts[q], starts[q] + counts[q]);
            self.build(
                child as usize,
                child_rect(rect, q),
                depth + 1,
                &mut dst[lo..hi],
                &mut src[lo..hi],
            );
        }
    }

    /// Indexes `id` under `bbox`.
    ///
    /// A box that cannot be placed -- one outside [`Quadtree::new`]'s bounds,
    /// or a backwards one such as [`Rect::EMPTY`] -- is kept at the root,
    /// where every query sees it. Nothing is dropped, not even a zero-area
    /// box: [`Rect::overlaps`] compares the two rectangles edge to edge and
    /// happily reports an overlap for a box with no area, so dropping one
    /// would make the index disagree with a plain scan.
    pub fn insert(&mut self, id: u32, bbox: Rect) {
        let mut ni = 0usize;
        if placeable(self.bounds, bbox) {
            let mut rect = self.bounds;
            for _ in 0..MAX_DEPTH {
                let Some(q) = quadrant(rect, bbox) else { break };
                let mut child = self.nodes[ni].children[q];
                if child == NO_CHILD {
                    // Subdivide only once the node has earned it, so a sparse
                    // document does not pay a node per item.
                    if self.nodes[ni].items.len() < SPLIT {
                        break;
                    }
                    child = self.nodes.len() as u32;
                    self.nodes.push(Node::new());
                    self.nodes[ni].children[q] = child;
                }
                rect = child_rect(rect, q);
                ni = child as usize;
            }
        }
        self.nodes[ni].items.push((id, bbox));
    }

    /// Removes `id`, whose bounding box must be the one it was inserted with.
    /// Returns whether it was found.
    ///
    /// The old box is required because it, not a side table, is what locates
    /// the item: removal is a descent, not a search.
    pub fn remove(&mut self, id: u32, bbox: Rect) -> bool {
        let descends = placeable(self.bounds, bbox);
        let mut ni = 0usize;
        let mut rect = self.bounds;
        for depth in 0..=MAX_DEPTH {
            // Insertion stops at the first node that is not yet full, so the
            // item may rest at any node on this path, not only the last.
            let items = &mut self.nodes[ni].items;
            if let Some(k) = items.iter().position(|&(other, _)| other == id) {
                items.swap_remove(k);
                return true;
            }
            if !descends || depth == MAX_DEPTH {
                return false;
            }
            let Some(q) = quadrant(rect, bbox) else {
                return false;
            };
            let child = self.nodes[ni].children[q];
            if child == NO_CHILD {
                return false;
            }
            rect = child_rect(rect, q);
            ni = child as usize;
        }
        false
    }

    /// Moves `id` from `old` to `new` without rebuilding the tree.
    ///
    /// This is the drag path: a few hundred calls per frame.
    ///
    /// ponytail: two independent descents, one to remove and one to insert.
    /// A move of one pixel almost always lands in the node it left, so a fused
    /// descent that walks the prefix `old` and `new` share would save close to
    /// half of it -- worth doing if profiling ever shows drag frames dominated
    /// by this rather than by the raster.
    pub fn update(&mut self, id: u32, old: Rect, new: Rect) {
        self.remove(id, old);
        self.insert(id, new);
    }

    /// Appends the ids of every item whose bounding box overlaps `area`.
    ///
    /// `out` is not cleared, so a caller can accumulate several queries; it is
    /// an argument rather than a return value so a per-frame cull can reuse
    /// one allocation.
    pub fn query(&self, area: Rect, out: &mut Vec<u32>) {
        // The root is visited unconditionally: it holds the unplaceable
        // items, which by definition are not inside `bounds`.
        self.query_node(0, self.bounds, area, out);
    }

    fn query_node(&self, ni: usize, rect: Rect, area: Rect, out: &mut Vec<u32>) {
        let node = &self.nodes[ni];
        out.extend(
            node.items
                .iter()
                .filter(|(_, bbox)| bbox.overlaps(area))
                .map(|&(id, _)| id),
        );
        for (q, &child) in node.children.iter().enumerate() {
            if child == NO_CHILD {
                continue;
            }
            let cell = child_rect(rect, q);
            if cell.overlaps(area) {
                self.query_node(child as usize, cell, area, out);
            }
        }
    }
}

/// True when `bbox` has a meaningful cell in a tree rooted at `bounds`.
///
/// Both halves matter. A backwards box has no position -- `Rect::EMPTY`'s
/// bounds are inverted infinities, and `contains_rect` accepts it against any
/// root -- and quadrant descent would send it somewhere that does not enclose
/// it, where a query stepping over that cell would then miss it. Out-of-bounds
/// boxes fail for the same reason. Both end up at the root instead, which is
/// correct and merely unaccelerated.
///
/// A zero-area box passes: it is a point, cells enclose points fine, and a
/// query can legitimately return it.
#[inline]
fn placeable(bounds: Rect, bbox: Rect) -> bool {
    bbox.x0 <= bbox.x1 && bbox.y0 <= bbox.y1 && bounds.contains_rect(bbox)
}

/// The quadrant of `node` wholly containing `bbox`, or `None` when `bbox`
/// straddles a midpoint.
///
/// Returns `None` for any non-finite midpoint too, since the comparisons all
/// fail against NaN -- which is what keeps a degenerate root harmless.
#[inline]
fn quadrant(node: Rect, bbox: Rect) -> Option<usize> {
    let mx = 0.5 * (node.x0 + node.x1);
    let my = 0.5 * (node.y0 + node.y1);
    // Written as four comparisons combined arithmetically rather than as
    // nested `if`s. Which side of a midpoint a box falls is unpredictable, and
    // four mispredictable branches per level -- taken once per item per level
    // of every insert and every build -- cost more than the whole rest of the
    // descent. This form compiles to `setcc`, with one branch at the end.
    let left = bbox.x1 <= mx;
    let right = bbox.x0 >= mx;
    let above = bbox.y1 <= my;
    let below = bbox.y0 >= my;
    if (left | right) & (above | below) {
        Some(usize::from(right) | (usize::from(below) << 1))
    } else {
        None
    }
}

/// The cell of quadrant `q` of `node`.
#[inline]
fn child_rect(node: Rect, q: usize) -> Rect {
    let mx = 0.5 * (node.x0 + node.x1);
    let my = 0.5 * (node.y0 + node.y1);
    let (x0, x1) = if q & 1 == 0 {
        (node.x0, mx)
    } else {
        (mx, node.x1)
    };
    let (y0, y1) = if q & 2 == 0 {
        (node.y0, my)
    } else {
        (my, node.y1)
    };
    Rect::new(x0, y0, x1, y1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::Rng;

    const DOC: Rect = Rect::new(0.0, 0.0, 1024.0, 1024.0);

    /// A box in `0..1024` squared, sized 0..64, plus degenerate draws.
    fn item(rng: &mut Rng) -> Rect {
        let x = rng.below(1024) as f64;
        let y = rng.below(1024) as f64;
        match rng.below(8) {
            0 => Rect::new(x, y, x, y),                         // zero area
            1 => Rect::new(-10.0, -10.0, 2000.0, 2000.0),       // spans everything
            2 => Rect::new(512.0, 512.0, 513.0, 513.0),         // coincident cluster
            3 => Rect::new(x - 4000.0, y, x - 3900.0, y + 1.0), // outside bounds
            _ => {
                let (w, h) = (rng.below(64) as f64 + 1.0, rng.below(64) as f64 + 1.0);
                Rect::new(x, y, x + w, y + h)
            }
        }
    }

    fn brute(items: &[(u32, Rect)], area: Rect) -> Vec<u32> {
        let mut v: Vec<u32> = items
            .iter()
            .filter(|(_, b)| b.overlaps(area))
            .map(|&(id, _)| id)
            .collect();
        v.sort_unstable();
        v
    }

    fn sorted(tree: &Quadtree, area: Rect) -> Vec<u32> {
        let mut v = Vec::new();
        tree.query(area, &mut v);
        v.sort_unstable();
        v
    }

    #[test]
    fn query_matches_a_linear_scan() {
        // The whole contract in one property: whatever the tree returns must
        // equal what a scan over the same items returns, for every query.
        for seed in 0..64u64 {
            let mut rng = Rng::new(seed);
            let items: Vec<(u32, Rect)> = (0..200).map(|i| (i, item(&mut rng))).collect();
            let mut tree = Quadtree::new(DOC);
            for &(id, bbox) in &items {
                tree.insert(id, bbox);
            }
            let bulk = Quadtree::bulk_load(DOC, &items);
            for _ in 0..20 {
                let area = item(&mut rng);
                let want = brute(&items, area);
                assert_eq!(sorted(&tree, area), want, "seed {seed}, area {area:?}");
                assert_eq!(
                    sorted(&bulk, area),
                    want,
                    "bulk, seed {seed}, area {area:?}"
                );
            }
        }
    }

    #[test]
    fn update_does_not_rebuild_and_stays_exact() {
        let mut rng = Rng::new(7);
        let mut items: Vec<(u32, Rect)> = (0..300).map(|i| (i, item(&mut rng))).collect();
        let mut tree = Quadtree::bulk_load(DOC, &items);
        let nodes_after_load = tree.nodes.len();
        for _ in 0..500 {
            let k = rng.below(items.len() as u64) as usize;
            let (id, old) = items[k];
            let new = item(&mut rng);
            tree.update(id, old, new);
            items[k].1 = new;
        }
        // One insert can create at most one node -- the child it descends
        // into starts empty and so cannot subdivide again. A move therefore
        // costs a cell at worst, never a rebuild.
        assert!(tree.nodes.len() <= nodes_after_load + 500);
        for _ in 0..50 {
            let area = item(&mut rng);
            assert_eq!(sorted(&tree, area), brute(&items, area));
        }
    }

    #[test]
    fn removing_everything_empties_the_tree() {
        let mut rng = Rng::new(11);
        let items: Vec<(u32, Rect)> = (0..300).map(|i| (i, item(&mut rng))).collect();
        let mut tree = Quadtree::new(DOC);
        for &(id, bbox) in &items {
            tree.insert(id, bbox);
        }
        for &(id, bbox) in &items {
            assert!(tree.remove(id, bbox), "id {id} not found");
            assert!(!tree.remove(id, bbox), "id {id} removed twice");
        }
        assert!(sorted(&tree, DOC).is_empty());
    }

    #[test]
    fn coincident_items_do_not_recurse_without_bound() {
        // 10k identical boxes: no subdivision can separate them, so only the
        // depth cap stops the descent.
        let bbox = Rect::new(1.0, 1.0, 2.0, 2.0);
        let mut tree = Quadtree::new(DOC);
        for id in 0..10_000 {
            tree.insert(id, bbox);
        }
        assert!(
            tree.nodes.len() <= MAX_DEPTH as usize + 1,
            "{} nodes for one point",
            tree.nodes.len()
        );
        assert_eq!(sorted(&tree, bbox).len(), 10_000);
        assert!(sorted(&tree, Rect::new(500.0, 500.0, 600.0, 600.0)).is_empty());
    }

    #[test]
    fn degenerate_bounds_still_answer_correctly() {
        // Rect::EMPTY as the root: every midpoint is NaN, so nothing descends
        // and the tree is a linear scan. Slow, but not wrong.
        let items = [
            (0u32, Rect::new(0.0, 0.0, 1.0, 1.0)),
            (1, Rect::new(10.0, 10.0, 11.0, 11.0)),
            (2, Rect::EMPTY),
        ];
        for tree in [
            Quadtree::bulk_load(Rect::EMPTY, &items),
            Quadtree::bulk_load(Rect::ZERO, &items),
        ] {
            assert_eq!(sorted(&tree, Rect::new(-1.0, -1.0, 5.0, 5.0)), vec![0]);
            assert_eq!(sorted(&tree, Rect::new(0.0, 0.0, 100.0, 100.0)), vec![0, 1]);
            // A query with no area matches nothing, mirroring `overlaps`.
            assert!(sorted(&tree, Rect::EMPTY).is_empty());
        }
    }

    #[test]
    fn bulk_load_and_insertion_agree_on_every_query() {
        let mut rng = Rng::new(3);
        let items: Vec<(u32, Rect)> = (0..5000).map(|i| (i, item(&mut rng))).collect();
        let mut inserted = Quadtree::new(DOC);
        for &(id, bbox) in &items {
            inserted.insert(id, bbox);
        }
        let bulk = Quadtree::bulk_load(DOC, &items);
        for _ in 0..100 {
            let area = item(&mut rng);
            assert_eq!(sorted(&bulk, area), sorted(&inserted, area));
        }
        // Removal must work against a bulk-loaded shape too, which is the
        // real risk: placement differs from what insertion would have built.
        let mut bulk = bulk;
        for &(id, bbox) in &items {
            assert!(bulk.remove(id, bbox), "id {id} not found");
        }
        assert!(sorted(&bulk, DOC).is_empty());
    }
}
