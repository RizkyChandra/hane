//! Viewport culling and tile-cache hit rate (issues #28, #30).
//!
//! Same harness as `spatial_bench`, for the same reason: D-001 rules out
//! `criterion`, so this is `Instant`, a discarded warmup and medians rather
//! than means.
//!
//! Run it with `cargo run --release --example tile_bench`. Results live in
//! `BENCHMARKS.md`. Two things are measured:
//!
//! - **Cull.** `View::visible_bounds` plus one `Quadtree::query`, which is the
//!   whole of #28. The number that matters is the one at 100k with a viewport
//!   big enough to hold ~2k items.
//! - **Tile cache.** A scripted pan and zoom over a fixed scene, counting hits
//!   and misses. Nothing is rendered -- a miss allocates a tile-sized buffer
//!   and stores it. That is deliberate: what a render costs belongs to #31, and
//!   mixing it in here would bury the only number this harness is for.

use std::hint::black_box;
use std::time::Instant;

use hane_geom::fuzz::Rng;
use hane_geom::{Point, Rect, Vec2};
use hane_scene::{Quadtree, TileCache, TileKey, View};

/// A 256x256 RGBA8 tile, which is what the cache holds in the browser.
const TILE_BYTES: usize = 256 * 256 * 4;

/// 64 MiB, or 256 resident tiles -- about ten screenfuls at 1280x720.
const BUDGET: usize = 64 << 20;

/// The document grows with the item count so density stays fixed, exactly as
/// in `spatial_bench`; the two harnesses have to describe the same scene for
/// their numbers to sit in the same table.
fn doc_side(n: u32) -> f64 {
    40.0 * f64::from(n).sqrt()
}

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

/// Median nanoseconds per operation over `reps` batches of `batch`, discarding
/// the first eighth as warmup. Lifted from `spatial_bench`, comment and all:
/// `run`'s return value is kept alive past the clock read so the optimiser
/// cannot delete the loop.
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

/// A view centred on the middle of the document at 1:1.
fn centred(side: f64, screen: Rect) -> View {
    let mut view = View::new();
    view.anchor_at(Point::new(side / 2.0, side / 2.0), screen.center());
    view
}

/// One cull measurement: `count` viewports along a diagonal sweep, so the
/// measured frames are not all answering the same query out of cache.
fn cull_row(label: &str, tree: &Quadtree, view: &View, screen: Rect, margin: f64) {
    let mut out = Vec::with_capacity(4096);
    let mut views = Vec::with_capacity(64);
    let mut v = *view;
    for _ in 0..64 {
        views.push(v);
        v.pan_by(Vec2::new(-37.0, -23.0));
    }
    let mut visible = 0u64;
    let ns = measure(65, views.len(), |_| {
        let mut sink = 0u64;
        for v in &views {
            out.clear();
            tree.query(v.visible_bounds(screen, margin), &mut out);
            sink += out.len() as u64;
        }
        visible = sink;
        sink
    });
    println!(
        "| {label} | {} | {:.2} |",
        visible / views.len() as u64,
        ns / 1000.0
    );
}

/// The scripted pan and zoom #31 will run: a steady drag with a slow zoom
/// through it, which is what a user does when they navigate rather than what a
/// synthetic benchmark does when it wants a good number.
///
/// `edits_per_frame` objects move each frame, and every move invalidates both
/// the box it left and the box it arrived at. `spread` is the fraction of the
/// viewport those edits are scattered over: 1.0 is the pathological case where
/// every edit lands on a different tile, and 0.05 is a selection being dragged,
/// which is what editing actually looks like.
fn tile_run(
    side: f64,
    screen: Rect,
    frames: usize,
    edits_per_frame: usize,
    spread: f64,
) -> (f64, usize, usize) {
    let mut rng = Rng::new(99);
    let mut cache = TileCache::new(BUDGET);
    let mut view = centred(side, screen);
    let mut keys = Vec::new();
    let mut renders = 0usize;
    for frame in 0..frames {
        // A tenth of a screen per second at 60fps, and a zoom that crosses a
        // power-of-two level boundary every ~90 frames.
        view.pan_by(Vec2::new(-4.0, -2.5));
        view.zoom_about(screen.center(), 1.0078);
        // Edits land inside the visible region. An edit off screen dirties
        // nothing resident and would flatter the hit rate for free, which is
        // the easy way to publish a number that means nothing.
        let doc = view.visible_bounds(screen, 0.0);
        let extent = doc.width() * spread;
        let ax = doc.x0 + rng.unit() * (doc.width() - extent);
        let ay = doc.y0 + rng.unit() * (doc.height() - extent).max(0.0);
        for _ in 0..edits_per_frame {
            let x = ax + rng.unit() * extent;
            let y = ay + rng.unit() * extent;
            let size = 20.0 / view.zoom();
            let old = Rect::new(x, y, x + size, y + size);
            let new = old.translate(Vec2::new(size, 0.0));
            cache.invalidate(old, new);
        }
        keys.clear();
        TileKey::visible(&view, screen, 128.0, &mut keys);
        for &key in &keys {
            if cache.get(key).is_none() {
                cache.insert(key, vec![frame as u8; TILE_BYTES]);
                renders += 1;
            }
        }
    }
    (cache.hit_rate(), renders, cache.bytes())
}

fn run_size(n: u32) {
    let side = doc_side(n);
    let mut rng = Rng::new(u64::from(n));
    let items = items(&mut rng, n, side);
    let tree = Quadtree::bulk_load(Rect::new(0.0, 0.0, side, side), &items);
    let screen = Rect::new(0.0, 0.0, 1280.0, 720.0);
    let view = centred(side, screen);
    let mut turned = view;
    turned.rotate_about(screen.center(), core::f64::consts::FRAC_PI_6);

    println!("### n = {n}\n");
    println!("Document {side:.0} x {side:.0}.\n");
    println!("| cull | items returned | time (us) |");
    println!("|---|---:|---:|");
    cull_row("1280x720, no margin", &tree, &view, screen, 0.0);
    cull_row("1280x720, 256px overdraw", &tree, &view, screen, 256.0);
    cull_row("1280x720, rotated 30 degrees", &tree, &turned, screen, 0.0);
    // Four times the area, which is what it takes to hold ~2k items at this
    // density -- the case #28 names.
    cull_row(
        "2560x1440, no margin",
        &tree,
        &centred(side, Rect::new(0.0, 0.0, 2560.0, 1440.0)),
        Rect::new(0.0, 0.0, 2560.0, 1440.0),
        0.0,
    );
    println!();
}

fn main() {
    println!("## Culling\n");
    for n in [1_000, 10_000, 100_000, 500_000] {
        run_size(n);
    }

    // The cache does not care how many objects the document has -- it is keyed
    // by geometry, not by content -- so the hit rate is measured once, over
    // the scripted navigation, with and without a stream of edits under it.
    println!("## Tile cache\n");
    println!(
        "300 frames of scripted pan and zoom, 1280x720, 128px overdraw, {BUDGET} byte budget.\n"
    );
    println!("| script | hit rate | tiles rendered | resident bytes |");
    println!("|---|---:|---:|---:|");
    let screen = Rect::new(0.0, 0.0, 1280.0, 720.0);
    for (label, edits, spread) in [
        ("pan + zoom", 0, 0.0),
        ("pan + zoom, 20 edits/frame in one selection", 20, 0.05),
        ("pan + zoom, 20 edits/frame scattered", 20, 1.0),
        ("pan + zoom, 200 edits/frame scattered", 200, 1.0),
    ] {
        let (rate, renders, bytes) = tile_run(doc_side(100_000), screen, 300, edits, spread);
        println!("| {label} | {:.1}% | {renders} | {bytes} |", rate * 100.0);
    }
}
