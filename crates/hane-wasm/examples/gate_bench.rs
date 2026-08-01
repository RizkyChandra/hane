//! The P3 gate, measured natively (#31).
//!
//! `cargo run --release --example gate_bench` -- and pass scene sizes to
//! override the default `1000 10000 100000 500000`.
//!
//! Same harness the browser page drives, same scripted camera, same scene
//! seed; only the clock differs (`Instant` here, `performance.now()` there).
//! That is the point: a native run and a browser run of the same size are
//! comparable, so the wasm tax is a number rather than a guess.
//!
//! Percentiles, not medians, unlike the rest of `BENCHMARKS.md`. The gate is
//! stated as a p99 and a p99 is the whole question -- a median frame time
//! hides exactly the frames that drop.

use std::env;
use std::hint::black_box;
use std::time::Instant;

use hane_wasm::bench::{Bench, DOC_SIDE};

/// Frames per run. Long enough for the zoom to reverse twice, so both
/// directions across a tile level boundary are in the sample.
const FRAMES: usize = 300;

/// Frames discarded as warmup, matching the house harnesses' "first eighth".
const WARMUP: usize = FRAMES / 8;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

/// Pan speeds in screen pixels per frame, with the gesture each one is.
///
/// 4 px/frame is `tile_bench`'s script and reproduces its ~98% hit rate. 40
/// px/frame is a flick -- 2400 px/s, which is a normal drag and the speed the
/// cache is least able to help with. A benchmark that only ran the slow one
/// would publish "the frame costs two microseconds", which is true of the
/// gentlest gesture in the product and of nothing else.
const SPEEDS: [(&str, f64); 2] = [("navigate, 4 px/frame", 4.0), ("flick, 40 px/frame", 40.0)];

/// Nearest-rank percentile of a nanosecond series, in milliseconds.
fn pct(samples: &[f64], p: f64) -> f64 {
    let mut s = samples.to_vec();
    s.sort_by(f64::total_cmp);
    let i = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[i.saturating_sub(1).min(s.len() - 1)] / 1e6
}

/// Times one call, keeping the result alive past the clock read so the
/// optimiser cannot delete the work.
fn time<T>(out: &mut Vec<f64>, run: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = run();
    out.push(start.elapsed().as_nanos() as f64);
    black_box(value)
}

fn run(n: u32, label: &str, pan: f64) {
    let build = Instant::now();
    let mut bench = Bench::new(n, u64::from(n), WIDTH, HEIGHT, pan);
    let build_ms = build.elapsed().as_secs_f64() * 1e3;

    let (mut cull, mut encode, mut raster) = (Vec::new(), Vec::new(), Vec::new());
    let (mut misses, mut visible, mut segments) = (0u64, 0u64, 0u64);
    for _ in 0..FRAMES {
        misses += u64::from(time(&mut cull, || bench.cull()));
        segments += u64::from(time(&mut encode, || bench.encode()));
        time(&mut raster, || bench.raster());
        visible += u64::from(bench.visible());
    }
    let (hit_rate, tiles, bytes) = (bench.hit_rate(), bench.tiles(), bench.bytes());
    // Frame 0 misses every tile on screen: a document being opened, or a jump
    // to a zoom level never visited. It is the true worst case and it is the
    // one frame the percentiles below cannot show, so it is reported on its
    // own rather than left to distort them.
    let cold = (cull[0] + encode[0]) / 1e6;
    let cold_raster = raster[0] / 1e6;
    // The cold frames are dropped from the timings but not from the hit rate:
    // a cache that only looks good once it is warm is not a finding.
    let (cull, encode, raster) = (&cull[WARMUP..], &encode[WARMUP..], &raster[WARMUP..]);
    // p99 of a sum is not the sum of the p99s, and the difference is exactly
    // the frames that matter, so the combined series is added per frame.
    let frame: Vec<f64> = cull.iter().zip(encode).map(|(c, e)| c + e).collect();

    println!("### n = {n}, {label}\n");
    println!(
        "Scene build {build_ms:.0} ms. {:.0} shapes in the viewport per frame, \
         {tiles} tiles on screen, {:.2} tile misses per frame, {:.0} segments \
         encoded per frame. Tile-cache hit rate **{:.1}%**, {:.0} MiB resident. \
         Cold first frame: cull + encode {cold:.2} ms, raster {cold_raster:.0} ms.\n",
        visible as f64 / FRAMES as f64,
        misses as f64 / FRAMES as f64,
        segments as f64 / FRAMES as f64,
        hit_rate * 100.0,
        f64::from(bytes) / (1 << 20) as f64,
    );
    println!("| phase | p50 (ms) | p99 (ms) |");
    println!("|---|---:|---:|");
    for (name, series) in [
        ("cull", cull),
        ("encode (bin)", encode),
        ("raster (CPU oracle)", raster),
        ("**cull + encode**", &frame[..]),
    ] {
        println!(
            "| {name} | {:.3} | {:.3} |",
            pct(series, 50.0),
            pct(series, 99.0)
        );
    }
    println!();
}

fn main() {
    let sizes: Vec<u32> = env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let sizes = if sizes.is_empty() {
        vec![1_000, 10_000, 100_000, 500_000]
    } else {
        sizes
    };
    println!("## The P3 gate, native\n");
    println!(
        "{FRAMES} frames of scripted pan and zoom at {WIDTH}x{HEIGHT}, first {WARMUP} discarded. \
         Document {DOC_SIDE:.0} x {DOC_SIDE:.0} at every size.\n"
    );
    for n in sizes {
        for (label, pan) in SPEEDS {
            run(n, label, pan);
        }
    }
}
