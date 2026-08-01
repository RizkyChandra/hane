//! Per-pixel diff of the GPU renderer against the CPU oracle (#20, D-002).
//!
//! # How a GPU pixel gets here
//!
//! Under D-010 the renderer cannot run inside `cargo test`: `hane-gpu` decides
//! what to draw and `hane-wasm` submits it to a `WebGl2RenderingContext`, which
//! only exists in a browser. So the seam is a directory.
//!
//! ```text
//! python3 scripts/gpu-diff.py     # headless browser renders the corpus
//! cargo test -p hane-gpu          # this file compares what it wrote
//! ```
//!
//! `scripts/gpu-diff.py` drives Chromium or Firefox at `web/gpu-diff.html`,
//! which renders every fixture through the real renderer and POSTs the
//! framebuffers back; they land in `tests/gpu-out/<name>.bin` as raw
//! premultiplied RGBA. This test reads them if they are there and reports how
//! many were not -- a run with no artefacts compares nothing and says so, in
//! those words, rather than passing quietly.
//!
//! The alternative -- a software submitter in this file that executed the draw
//! commands natively -- was rejected: it would be a second copy of the shader
//! in Rust, and two implementations that drift is exactly the failure D-002
//! exists to prevent. This way the thing under test is the shipping renderer on
//! a real WebGL2 driver.
//!
//! # Why the oracle is a committed PNG and not a live `hane-raster` call
//!
//! `hane-raster/tests/golden/*.png` *is* the oracle's output, already pinned
//! byte for byte by that crate's own harness. Reading it here keeps one
//! definition of what the oracle says: if `golden_corpus` passes, these PNGs
//! are the CPU rasterizer.
//!
//! The bytes are **premultiplied** RGBA (the [`Pixmap`] contract), so the GPU
//! side has to hand back premultiplied bytes too. `alpha_on_transparent` is the
//! fixture that catches getting this backwards.
//!
//! # Failure artefacts
//!
//! On a mismatch this writes `<name>.actual.png` and `<name>.diff.png` into
//! `crates/hane-gpu/tests/gpu-diff/` -- its own directory, because
//! `hane-raster` writes files of the same names for the CPU comparison and one
//! would silently overwrite the other.
//!
//! [`Pixmap`]: hane_raster::Pixmap

use hane_raster::{diff, png};
use std::fs;
use std::path::{Path, PathBuf};

/// One fixture, and how far the GPU may drift from the oracle on it.
struct Case {
    /// Golden file stem in `hane-raster/tests/golden/`.
    name: &'static str,
    /// Largest per-channel absolute difference tolerated, in `0..=255`.
    max_diff: u8,
    /// Largest mean per-channel absolute difference tolerated.
    mean_diff: f64,
}

/// Builds [`CASES`]. A macro only so rustfmt leaves one fixture per line, which
/// is what makes the tolerance table reviewable as a list.
macro_rules! cases {
    ($($name:ident, max $max:literal, mean $mean:literal;)*) => {
        &[$(Case { name: stringify!($name), max_diff: $max, mean_diff: $mean },)*]
    };
}

/// The corpus, with a GPU tolerance per fixture.
///
/// # These are measured, not guessed
///
/// #20 set them at max 2 / mean 0.05 with nothing to measure and asked #19 to
/// meet them or argue. The renderer met them with room to spare, so they are
/// now the numbers two real drivers actually produce:
///
/// | | fixtures | worst max | worst mean |
/// |---|---|---|---|
/// | Chromium 150, SwiftShader | 41 | 1 | 0.0060 |
/// | Firefox 153, software WebGL | 41 | 1 | 0.0114 |
///
/// Thirty-one of the forty-one are **bit-exact** in Chromium and thirty in
/// Firefox. That is not luck: the fragment shader runs the oracle's own
/// algorithm -- sixteen sample lines, sorted crossings, analytic horizontal
/// spans -- so the only thing left to differ is `f32` against `f64` (D-004),
/// and a coverage value has to land within an ulp of a `1/255` step for that to
/// move a byte.
///
/// So the table below is **max 1, mean 0.02** for anything with an
/// anti-aliased edge, and **max 0** for fixtures with no coverage arithmetic in
/// them at all. One count out of 255 is the smallest difference a byte can
/// hold; every bug this harness exists to catch -- a winding rule counted with
/// a `bool`, an edge binned into the wrong tile, a seam conflated into one
/// coverage buffer -- moves whole pixels between 0 and 255.
///
/// The mean bound is the loose one on purpose: 0.02 is nearly twice Firefox's
/// worst, which leaves room for a third driver to round a few more edge pixels
/// the other way without leaving room for a wrong picture.
const CASES: &[Case] = cases! {
    // -- no arithmetic that could round differently: edges on pixel bounds,
    //    or nothing drawn at all. Held to exact equality on purpose. --
    rect_pixel_aligned,      max 0, mean 0.0;
    far_offscreen,           max 0, mean 0.0;
    degenerate_empty,        max 0, mean 0.0;
    degenerate_zero_area,    max 0, mean 0.0;
    // -- anti-aliased edges: the f32-vs-f64 coverage budget above --
    rect_subpixel_offsets,   max 1, mean 0.02;
    triangle,                max 1, mean 0.02;
    circle,                  max 1, mean 0.02;
    quad_petals,             max 1, mean 0.02;
    pentagram,               max 1, mean 0.02;
    nested_triangles,        max 1, mean 0.02;
    annulus_reverse_winding, max 1, mean 0.02;
    annulus_same_winding,    max 1, mean 0.02;
    figure_eight,            max 1, mean 0.02;
    cubic_loop,              max 1, mean 0.02;
    subpixel_squares,        max 1, mean 0.02;
    sliver_diagonal,         max 1, mean 0.02;
    taper_wedge,             max 1, mean 0.02;
    hairline_grid,           max 1, mean 0.02;
    long_thin_diagonal,      max 1, mean 0.02;
    overlap_translucent,     max 1, mean 0.02;
    alpha_on_transparent,    max 1, mean 0.02;
    degenerate_nonfinite,    max 1, mean 0.02;
    open_subpath,            max 1, mean 0.02;
    // Six bars from 1/32 of a pixel tall upward. #20 gave this max 24 on the
    // reasoning that a GPU computing coverage *analytically* would get the
    // right answer where the oracle's sixteen sample lines get a quantised one,
    // and that demanding agreement would be demanding the GPU reproduce the
    // quantisation. That reasoning was sound and the premise turned out to be
    // false: this renderer samples the same sixteen lines, so it reproduces the
    // quantisation exactly and the measured difference is zero in both
    // browsers. Held to the default -- the slack was for a renderer that was
    // never built.
    sliver_rows,             max 1, mean 0.02;
    // The GPU has 16x16 tiles and the oracle has none, so these are the two
    // fixtures where a binning bug shows and nothing else does. Full default
    // tolerance, no slack: a boundary edge binned into both tiles or neither is
    // a whole-pixel error. Both are bit-exact.
    seam_shared_edge,        max 1, mean 0.02;
    tile_boundary_rects,     max 1, mean 0.02;
    // One pixel of canvas: the mean is over four channels, so it has to be
    // allowed to equal the max or it is the stricter of the two bounds.
    tiny_canvas,             max 1, mean 1.0;
    // -- even-odd, the same geometry read for parity (#12) --
    pentagram_evenodd,       max 1, mean 0.02;
    nested_triangles_evenodd, max 1, mean 0.02;
    annulus_same_evenodd,    max 1, mean 0.02;
    figure_eight_evenodd,    max 1, mean 0.02;
    // -- gradients (#23). The ramp is sampled and dithered by the same
    //    formulas, in f32 rather than f64; a stop lerp near a 1/255 step is
    //    where the two part company. Only the radial one actually does.
    gradient_linear_pad,     max 1, mean 0.02;
    gradient_spread_modes,   max 1, mean 0.02;
    gradient_radial_focus,   max 1, mean 0.02;
    gradient_partial_coverage, max 1, mean 0.02;
    // -- clipping (#21). The mask is a half-float texture and the oracle's is
    //    f64, so a clip edge carries 10 bits of mantissa instead of 53 -- worth
    //    at most the one count these show.
    clip_midpixel_edge,      max 1, mean 0.02;
    clip_nested_and_empty,   max 1, mean 0.02;
    clip_curve_over_gradient, max 1, mean 0.02;
    // -- blend modes (#22). The non-separable set divides by a luminosity and
    //    could amplify an f32 rounding difference; measured, it does not, and
    //    both blend fixtures are bit-exact. The group one is not, by one count
    //    on the pixels where a layer alpha lands on a rounding boundary.
    blend_separable,         max 1, mean 0.02;
    blend_nonseparable,      max 1, mean 0.02;
    blend_isolated_group,    max 1, mean 0.02;
    // `extreme_coords` is absent on purpose, and now with a number rather than
    // an argument: rendered and compared, it comes back at max 215, mean 38.5.
    // Its vertices are at 1e9, where an f32 has a 64-pixel quantum (D-004), so
    // the GPU cannot land those edges on the oracle's sub-pixel answer and no
    // tolerance short of "anything" would let it. What the renderer should do
    // with coordinates it cannot represent is a real question -- transform into
    // a local frame before narrowing -- and it belongs to whoever adds the view
    // transform, with its own fixture at a scale f32 can hold.
};

/// The GPU render of `name`, or `None` if the browser has not been run.
///
/// See the module docs: `scripts/gpu-diff.py` writes these. Nothing here draws
/// anything -- it reads what a real WebGL2 driver produced.
fn gpu_render(name: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let bytes = fs::read(gpu_out_dir().join(format!("{name}.bin"))).ok()?;
    // A short or long file is a bug worth reporting through `verdict`, which
    // says how many bytes it got, rather than being silently skipped as
    // "pending" alongside the fixtures nobody rendered.
    let _ = (w, h);
    Some(bytes)
}

/// Where `scripts/gpu-diff.py` puts the framebuffers it collected.
fn gpu_out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gpu-out")
}

/// The comparison itself: `None` if the GPU output is within this case's
/// tolerance, otherwise why it is not.
fn verdict(case: &Case, oracle: &[u8], actual: &[u8]) -> Option<String> {
    if oracle.len() != actual.len() {
        return Some(format!(
            "GPU returned {} bytes, oracle has {}",
            actual.len(),
            oracle.len()
        ));
    }
    let (max, mean) = diff::stats(oracle, actual);
    (max > case.max_diff || mean > case.mean_diff).then(|| {
        format!(
            "max per-channel diff {max} (tolerance {}), mean {mean:.4} (tolerance {:.4})",
            case.max_diff, case.mean_diff
        )
    })
}

/// Diffs every fixture the GPU can render against the oracle, and says out loud
/// how many it could not.
#[test]
fn gpu_matches_oracle() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gpu-diff");
    let mut failures = Vec::new();
    let mut pending = Vec::new();

    for case in CASES {
        let (w, h, oracle) = load_oracle(case.name);

        let Some(actual) = gpu_render(case.name, w, h) else {
            pending.push(case.name);
            continue;
        };

        let Some(why) = verdict(case, &oracle, &actual) else {
            continue;
        };

        fs::create_dir_all(&out_dir).expect("create gpu-diff dir");
        write_png(
            &out_dir.join(format!("{}.actual.png", case.name)),
            w,
            h,
            &actual,
        );
        // Only meaningful when the sizes agree; a size mismatch has no per-pixel
        // story to tell and `diff::image` would panic trying to tell it.
        if actual.len() == oracle.len() {
            write_png(
                &out_dir.join(format!("{}.diff.png", case.name)),
                w,
                h,
                &diff::image(&oracle, &actual),
            );
        }
        failures.push(format!("  {}: {why}", case.name));
    }

    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ from the CPU oracle:\n{}\n\nDiff images in {}",
        failures.len(),
        CASES.len() - pending.len(),
        failures.join("\n"),
        out_dir.display()
    );

    // A run with no artefacts has compared nothing, and must not read as a
    // green comparison. It says so; `GPU_DIFF_REQUIRE=1` turns it into a
    // failure, which is what `scripts/gpu-diff.py` tells you to run next.
    if !pending.is_empty() {
        let required = std::env::var("GPU_DIFF_REQUIRE").is_ok_and(|v| !v.is_empty() && v != "0");
        let msg = format!(
            "{} of {} fixtures were not rendered on the GPU and were NOT compared: {}.\n\
             Run `python3 scripts/gpu-diff.py` to produce them; see the module docs.",
            pending.len(),
            CASES.len(),
            pending.join(", ")
        );
        assert!(!required, "{msg}");
        println!("{msg}");
    }
}

/// The self-check that keeps the harness honest without a browser.
///
/// On a machine that has not run `scripts/gpu-diff.py`, `gpu_matches_oracle`
/// executes no comparison at all, so nothing else here would notice if
/// [`verdict`] stopped working -- a harness that cannot fail is not a harness.
/// This feeds it a real oracle image and a copy perturbed by a known amount,
/// and pins the boundary.
#[test]
fn verdict_accepts_identity_and_rejects_regression() {
    let case = &Case {
        name: "circle",
        max_diff: 2,
        mean_diff: 0.05,
    };
    let (w, h, oracle) = load_oracle(case.name);

    assert!(
        verdict(case, &oracle, &oracle).is_none(),
        "identical buffers must pass"
    );

    // Darken rather than brighten: this fixture is drawn on opaque white, and
    // adding would wrap 255 round to a difference of 254 instead of the small
    // one the test means to make.
    let darken =
        |buf: &[u8], by: u8| -> Vec<u8> { buf.iter().map(|b| b.saturating_sub(by)).collect() };

    // Exactly at the tolerance, on one pixel: within max, and far too few
    // pixels to move the mean. Must pass, or the bound is off by one.
    let mut edge = oracle.clone();
    edge[0] = edge[0].saturating_sub(2);
    assert_eq!(diff::stats(&oracle, &edge).0, 2, "the perturbation landed");
    assert!(
        verdict(case, &oracle, &edge).is_none(),
        "max 2 means 2 passes"
    );

    // One count past it, on one channel out of 16384. The mean is 0.0002,
    // nowhere near its own bound -- so only `max_diff` can catch this, which is
    // the whole reason both statistics exist.
    let mut spike = oracle.clone();
    spike[0] = spike[0].saturating_sub(3);
    assert!(
        verdict(case, &oracle, &spike).is_some(),
        "one channel 3 counts out must fail on max_diff"
    );

    // Every channel out by one: under `max_diff`, mean exactly 1.0, twenty
    // times its bound. The error a max-only harness waves through.
    let smear = darken(&oracle, 1);
    assert_eq!(diff::stats(&oracle, &smear), (1, 1.0), "a uniform -1");
    let why = verdict(case, &oracle, &smear).expect("a uniform -1 must fail on mean_diff");
    assert!(why.contains("mean 1.0000"), "reported: {why}");

    // A short buffer is a bug in the renderer, not a pixel difference, and must
    // not reach `diff::stats` -- which asserts on it.
    assert!(verdict(case, &oracle, &oracle[..oracle.len() - 4]).is_some());

    let img = diff::image(&oracle, &spike);
    assert_eq!(img.len(), oracle.len());
    assert_eq!(
        &img[..4],
        &[255, 255 - 24, 0, 255],
        "the changed pixel is marked at 8x gain"
    );
    assert_eq!((w, h), (64, 64), "fixture size came from the PNG header");
}

/// Every case must name a golden that exists, decodes, and is the size its own
/// header claims. This is the wiring across the two crates -- a fixture renamed
/// in `hane-raster` has to fail here rather than silently stop being compared.
#[test]
fn every_case_has_an_oracle() {
    for case in CASES {
        let (w, h, pixels) = load_oracle(case.name);
        assert_eq!(
            pixels.len(),
            (w as usize) * (h as usize) * 4,
            "{}: decoded {} bytes for {w}x{h}",
            case.name,
            pixels.len()
        );
    }
}

/// A golden with no case here is a fixture that stopped being diffed against
/// the GPU without anyone deciding it should. `extreme_coords` is the one
/// deliberate exclusion; see [`CASES`].
#[test]
fn no_fixture_is_silently_skipped() {
    const EXCLUDED: &[&str] = &["extreme_coords"];

    let missing: Vec<_> = fs::read_dir(oracle_dir())
        .expect("read golden dir")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png") && !n.ends_with(".actual.png") && !n.ends_with(".diff.png"))
        .map(|n| n.trim_end_matches(".png").to_string())
        .filter(|stem| !EXCLUDED.contains(&stem.as_str()) && !CASES.iter().any(|c| c.name == stem))
        .collect();

    assert!(
        missing.is_empty(),
        "goldens with no GPU case: {missing:?}. Add each to CASES with a \
         tolerance, or to EXCLUDED with a reason."
    );
}

/// `hane-raster/tests/golden`, reached from this crate's manifest so it
/// resolves the same from the workspace root and from inside the crate.
fn oracle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../hane-raster/tests/golden")
}

/// Decodes a committed golden into `(width, height, premultiplied RGBA)`.
fn load_oracle(name: &str) -> (u32, u32, Vec<u8>) {
    let path = oracle_dir().join(format!("{name}.png"));
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    png::decode(&bytes).unwrap_or_else(|| panic!("decode {}", path.display()))
}

fn write_png(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    fs::write(path, png::encode(w, h, pixels))
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
