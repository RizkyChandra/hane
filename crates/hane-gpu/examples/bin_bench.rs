//! What tile size should `hane_gpu::TILE_SIZE` be (issue #18)?
//!
//! D-001 rules out `criterion`, so this is the same hand-rolled shape as
//! `hane-scene`'s `spatial_bench`: `Instant`, a discarded warmup, and medians.
//!
//! Run it with `cargo run --release --example bin_bench`. A debug build
//! measures the optimiser. Results live in `BENCHMARKS.md`.
//!
//! Two costs pull in opposite directions and only one of them is measurable
//! without a GPU:
//!
//! - **Binning, measured here.** Smaller tiles mean more `(tile, segment)`
//!   entries per segment and a bigger grid to sweep, so the CPU cost falls
//!   monotonically as tiles grow.
//! - **Fill, modelled here.** WebGL2 has no compute shaders (D-003), so a tile
//!   is drawn by running its whole segment list over its whole pixel area.
//!   That is `entries * size^2` segment-pixels, and it rises with the tile,
//!   because a bigger tile pulls in segments that miss most of its pixels.
//!
//! The second number is a model, not a measurement, and it is labelled as one.
//! It is also the only reason not to pick the largest tile on the table.

use std::hint::black_box;
use std::time::Instant;

use hane_geom::fuzz::Rng;
use hane_geom::{CubicBez, Point, QuadBez};
use hane_gpu::{TILE_SIZE, TileBinner};
use hane_path::Segment;

/// A 1080p artboard, the size the frame budget in `PLAN.md` is quoted at.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// Half a device pixel, the flattening tolerance a rasterizer would use.
const TOL: f64 = 0.5;

/// A scene of roughly `n` segments as closed blobs of 8 segments each.
///
/// Blobs are 8 to 200 pixels across and scattered over the artboard, which is
/// what a real illustration looks like once culling has run: many small shapes,
/// not a few screen-spanning ones. A screen-spanning path is the case where the
/// tile size stops mattering, since it lands in every tile whatever the size.
///
/// The octagon offsets are a fixed table rather than trigonometry, so the scene
/// is bit-identical on every machine that runs this -- the same reason
/// `hane_geom::fuzz` bans libm.
fn scene(rng: &mut Rng, n: usize) -> Vec<Segment> {
    const D: f64 = std::f64::consts::FRAC_1_SQRT_2;
    const RING: [(f64, f64); 8] = [
        (1.0, 0.0),
        (D, D),
        (0.0, 1.0),
        (-D, D),
        (-1.0, 0.0),
        (-D, -D),
        (0.0, -1.0),
        (D, -D),
    ];
    let mut segs = Vec::with_capacity(n);
    while segs.len() < n {
        let cx = rng.below(u64::from(WIDTH)) as f64;
        let cy = rng.below(u64::from(HEIGHT)) as f64;
        let r = 4.0 + rng.below(96) as f64;
        // Per-vertex jitter, so no two blobs are the same shape.
        let at = |k: usize, rng: &mut Rng| {
            let (dx, dy) = RING[k % 8];
            let j = 0.6 + 0.8 * rng.unit();
            Point::new(cx + dx * r * j, cy + dy * r * j)
        };
        let mut prev = at(0, rng);
        for k in 1..=8 {
            let next = at(k, rng);
            // A third lines, a third quadratics, a third cubics: an SVG import
            // is mostly cubics, a font is mostly quadratics, a UI mock is
            // mostly lines.
            segs.push(match k % 3 {
                0 => Segment::Line(prev, next),
                1 => Segment::Quad(QuadBez::new(prev, at(k + 4, rng), next)),
                _ => Segment::Cubic(CubicBez::new(prev, at(k + 3, rng), at(k + 5, rng), next)),
            });
            prev = next;
        }
    }
    segs
}

/// Median nanoseconds per operation over `reps` batches of `batch`, discarding
/// the first eighth as warmup. Lifted from `spatial_bench`, same reasoning: one
/// descheduled iteration moves a mean and not a median.
fn measure<T>(reps: usize, batch: usize, mut run: impl FnMut() -> T) -> f64 {
    let mut samples = Vec::with_capacity(reps);
    for rep in 0..reps {
        let start = Instant::now();
        let sink = run();
        let ns = start.elapsed().as_nanos() as f64 / batch as f64;
        black_box(sink);
        if rep >= reps / 8 {
            samples.push(ns);
        }
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn run_size(n: usize) {
    let mut rng = Rng::new(n as u64);
    let segs = scene(&mut rng, n);

    println!("### {} segments\n", segs.len());
    println!(
        "| tile | bin (us) | entries | entries/seg | non-empty tiles | segs/tile | \
         segment-pixels (model) |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|");

    let mut rows = Vec::new();
    for size in [4u32, 8, 16, 32, 64, 128] {
        let mut binner = TileBinner::with_tile_size(WIDTH, HEIGHT, size);
        let us = measure(65, 1, || {
            binner.bin(&segs, TOL);
            binner.tiles().count()
        }) / 1000.0;

        binner.bin(&segs, TOL);
        let occupied = binner.tiles().count();
        let entries: usize = binner.tiles().map(|(_, s)| s.len()).sum();
        // Every segment that survives the cull must be somewhere, or the
        // timings above are timings of the wrong answer.
        assert!(entries >= occupied && occupied > 0);
        let fill = entries as f64 * f64::from(size) * f64::from(size);
        println!(
            "| {size} | {us:.1} | {entries} | {:.2} | {occupied} | {:.2} | {:.1}M |",
            entries as f64 / segs.len() as f64,
            entries as f64 / occupied as f64,
            fill / 1e6,
        );
        rows.push((size, us, fill));
    }

    // Flattening is common to every tile size, so it is the floor the bin
    // column cannot go below however big the tiles get. Worth separating: it is
    // work the rasterizer has to do anyway, and it is not the binner's to save.
    let mut flat = Vec::new();
    let mut points = 0usize;
    let flatten_us = measure(65, 1, || {
        points = 0;
        for seg in &segs {
            flat.clear();
            match *seg {
                Segment::Line(a, b) => {
                    flat.push(a);
                    flat.push(b);
                }
                Segment::Quad(q) => q.flatten(TOL, &mut flat),
                Segment::Cubic(c) => c.flatten(TOL, &mut flat),
            }
            points += flat.len();
        }
        points
    }) / 1000.0;
    println!("\nFlattening alone: {flatten_us:.1} us for {points} polyline points.");

    // Normalised against the shipped tile so the two columns are comparable at
    // a glance: above 1.00 is worse than TILE_SIZE.
    let base = rows
        .iter()
        .find(|r| r.0 == TILE_SIZE)
        .expect("TILE_SIZE not on the table");
    println!("\nRelative to the shipped {TILE_SIZE} px tile (lower is better):\n");
    println!("| tile | bin cost | fill cost (model) | sum |");
    println!("|---:|---:|---:|---:|");
    for &(size, us, fill) in &rows {
        let (b, f) = (us / base.1, fill / base.2);
        println!("| {size} | {b:.2}x | {f:.2}x | {:.2}x |", b + f);
    }
    println!();
}

fn main() {
    println!(
        "Artboard {WIDTH}x{HEIGHT}, flattening tolerance {TOL} px. \
         Shipped tile size: {TILE_SIZE}.\n"
    );
    for n in [1_000, 10_000, 50_000] {
        run_size(n);
    }
}
