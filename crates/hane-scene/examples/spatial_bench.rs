//! Quadtree versus linear scan, on identical data (issue #27).
//!
//! D-001 rules out `criterion`, so this is the hand-rolled replacement:
//! `Instant`, a discarded warmup, and medians rather than means, because one
//! descheduled iteration moves a mean and not a median.
//!
//! Run it with `cargo run --release --example spatial_bench`. A debug build
//! measures nothing useful -- the scan is a tight `overlaps` loop that only
//! exists at `opt-level = 3`. Results live in `BENCHMARKS.md`.

use std::hint::black_box;
use std::time::Instant;

use hane_geom::Rect;
use hane_geom::fuzz::Rng;
use hane_scene::Quadtree;

/// The document grows with the item count so density stays fixed. A viewport
/// is a fixed size in document units, so it returns a roughly constant number
/// of items at every `n` -- which is the case the index exists for, and the
/// one where a scan's `O(n)` is most exposed.
fn doc_side(n: u32) -> f64 {
    40.0 * f64::from(n).sqrt()
}

/// `n` items of 4..40 units, uniform over the document.
fn items(rng: &mut Rng, n: u32, side: f64) -> Vec<(u32, Rect)> {
    (0..n)
        .map(|id| {
            let x = rng.below(side as u64) as f64;
            let y = rng.below(side as u64) as f64;
            let w = rng.below(36) as f64 + 4.0;
            let h = rng.below(36) as f64 + 4.0;
            (id, Rect::new(x, y, x + w, y + h))
        })
        .collect()
}

/// `count` query rectangles of `w` by `h`, uniform over the document.
fn areas(rng: &mut Rng, count: usize, side: f64, w: f64, h: f64) -> Vec<Rect> {
    (0..count)
        .map(|_| {
            let x = rng.below(side as u64) as f64;
            let y = rng.below(side as u64) as f64;
            Rect::new(x, y, x + w, y + h)
        })
        .collect()
}

/// The linear scan being compared against, in its strongest form: one
/// contiguous array indexed by item id, so an update is a single store and a
/// query is a branch-predictable sweep with no pointer chasing.
struct Scan {
    boxes: Vec<Rect>,
}

impl Scan {
    fn new(items: &[(u32, Rect)]) -> Self {
        let mut boxes = vec![Rect::EMPTY; items.len()];
        for &(id, bbox) in items {
            boxes[id as usize] = bbox;
        }
        Self { boxes }
    }

    fn query(&self, area: Rect, out: &mut Vec<u32>) {
        out.extend(
            self.boxes
                .iter()
                .enumerate()
                .filter(|(_, b)| b.overlaps(area))
                .map(|(i, _)| i as u32),
        );
    }
}

/// Median nanoseconds per operation over `reps` batches of `batch`
/// operations, discarding the first eighth of the batches as warmup.
///
/// `run` returns a value derived from the work so the optimiser cannot delete
/// the loop it was asked to time. Whatever it returns is dropped after the
/// clock is read, which keeps freeing a tree's several thousand allocations
/// out of that tree's build time.
fn measure<T>(reps: usize, batch: usize, mut run: impl FnMut(usize) -> T) -> f64 {
    let mut samples = Vec::with_capacity(reps);
    for rep in 0..reps {
        let start = Instant::now();
        let sink = run(rep);
        let ns = start.elapsed().as_nanos() as f64 / batch as f64;
        black_box(sink);
        if rep >= reps / 8 {
            samples.push(ns);
        }
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// One `n`, printed as a markdown table row per measurement.
fn run_size(n: u32) {
    let side = doc_side(n);
    let mut rng = Rng::new(u64::from(n));
    let items = items(&mut rng, n, side);
    // A screen-sized viewport, and a quarter-document rectangle for the case
    // where the answer is a large fraction of the document.
    let viewports = areas(&mut rng, 64, side, 1280.0, 720.0);
    let wide = areas(&mut rng, 64, side, side / 4.0, side / 4.0);
    // The drag: 500 items move by one pixel per frame.
    let moving: Vec<u32> = (0..500).map(|_| rng.below(u64::from(n)) as u32).collect();

    let tree = Quadtree::bulk_load(Rect::new(0.0, 0.0, side, side), &items);
    let scan = Scan::new(&items);
    let mut out = Vec::with_capacity(n as usize);

    // Build.
    let bulk = measure(33, 1, |_| {
        Quadtree::bulk_load(Rect::new(0.0, 0.0, side, side), &items)
    });
    let repeated = measure(33, 1, |_| {
        let mut t = Quadtree::new(Rect::new(0.0, 0.0, side, side));
        for &(id, bbox) in &items {
            t.insert(id, bbox);
        }
        t
    });
    let scan_build = measure(33, 1, |_| Scan::new(&items));

    // Query.
    let mut hits = 0u64;
    let tree_view = measure(65, viewports.len(), |_| {
        let mut sink = 0;
        for &area in &viewports {
            out.clear();
            tree.query(area, &mut out);
            sink += out.len() as u64;
        }
        hits = sink;
        sink
    });
    let scan_view = measure(65, viewports.len(), |_| {
        let mut sink = 0;
        for &area in &viewports {
            out.clear();
            scan.query(area, &mut out);
            sink += out.len() as u64;
        }
        sink
    });
    let mut wide_hits = 0u64;
    let tree_wide = measure(65, wide.len(), |_| {
        let mut sink = 0;
        for &area in &wide {
            out.clear();
            tree.query(area, &mut out);
            sink += out.len() as u64;
        }
        wide_hits = sink;
        sink
    });
    let scan_wide = measure(65, wide.len(), |_| {
        let mut sink = 0;
        for &area in &wide {
            out.clear();
            scan.query(area, &mut out);
            sink += out.len() as u64;
        }
        sink
    });

    // Incremental update. The delta alternates sign with the repetition, so
    // the items oscillate around their starting boxes instead of drifting off
    // the document over the course of the run.
    let mut tree = tree;
    let mut current: Vec<Rect> = vec![Rect::EMPTY; n as usize];
    for &(id, bbox) in &items {
        current[id as usize] = bbox;
    }
    let tree_update = measure(65, moving.len(), |rep| {
        let d = if rep % 2 == 0 { 1.0 } else { -1.0 };
        for &id in &moving {
            let old = current[id as usize];
            let new = Rect::new(old.x0 + d, old.y0 + d, old.x1 + d, old.y1 + d);
            tree.update(id, old, new);
            current[id as usize] = new;
        }
        current[0].x0.to_bits()
    });
    let mut scan = scan;
    let scan_update = measure(65, moving.len(), |rep| {
        let d = if rep % 2 == 0 { 1.0 } else { -1.0 };
        for &id in &moving {
            let old = scan.boxes[id as usize];
            scan.boxes[id as usize] = Rect::new(old.x0 + d, old.y0 + d, old.x1 + d, old.y1 + d);
        }
        scan.boxes[0].x0.to_bits()
    });

    // The tree must still agree with the scan after all that, or the numbers
    // above are timings of the wrong answer.
    let mut a = Vec::new();
    let mut b = Vec::new();
    tree.query(viewports[0], &mut a);
    scan.query(viewports[0], &mut b);
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "quadtree and scan disagree at n = {n}");

    let per_query = |ns: f64| ns / 1000.0;
    println!("### n = {n}\n");
    println!(
        "Document {side:.0} x {side:.0}. Viewport query returns {} items, \
         quarter-document query returns {}.\n",
        hits / viewports.len() as u64,
        wide_hits / wide.len() as u64
    );
    println!("| measurement | quadtree | linear scan | ratio |");
    println!("|---|---:|---:|---:|");
    println!(
        "| build (us) | {:.1} bulk / {:.1} repeated insert | {:.1} | {:.2}x |",
        per_query(bulk),
        per_query(repeated),
        per_query(scan_build),
        scan_build / bulk
    );
    println!(
        "| viewport query (us) | {:.2} | {:.2} | {:.2}x |",
        per_query(tree_view),
        per_query(scan_view),
        scan_view / tree_view
    );
    println!(
        "| quarter-document query (us) | {:.2} | {:.2} | {:.2}x |",
        per_query(tree_wide),
        per_query(scan_wide),
        scan_wide / tree_wide
    );
    println!(
        "| move one item (ns) | {:.1} | {:.1} | {:.2}x |",
        tree_update,
        scan_update,
        scan_update / tree_update
    );
    // The workload D-006 is tuned for: 500 boxes move, then the viewport is
    // culled once. Derived from the medians above rather than measured on its
    // own, since it is exactly their sum.
    println!(
        "| drag frame: 500 moves + 1 viewport query (us) | {:.1} | {:.1} | {:.2}x |\n",
        (500.0 * tree_update + tree_view) / 1000.0,
        (500.0 * scan_update + scan_view) / 1000.0,
        (500.0 * scan_update + scan_view) / (500.0 * tree_update + tree_view)
    );
}

fn main() {
    println!("Ratios are scan/quadtree: above 1.00 the quadtree wins.\n");
    // 30k is not one of the sizes issue #27 asks for; it is there to pin
    // down where the two implementations cross over.
    for n in [1_000, 10_000, 30_000, 100_000] {
        run_size(n);
    }
}
