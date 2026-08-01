//! Per-pixel diff of the GPU renderer against the CPU oracle (#20, D-002).
//!
//! # State of this harness
//!
//! **Nothing is compared yet.** There is no GPU renderer (#19), so
//! [`gpu_render`] returns `None` for every fixture and the comparison loop has
//! nothing to run. A green run of `gpu_matches_oracle` today means "no GPU
//! pixel was examined", and the assertion at the end of it says so in those
//! words -- and fails the moment that stops being true, so nobody inherits the
//! sentence without noticing.
//!
//! Everything *around* the seam is finished and tested: the corpus wiring, the
//! verdict, the per-fixture tolerances and the diff image. When #19 lands, the
//! only edit is the body of [`gpu_render`].
//!
//! # Why the oracle is a committed PNG and not a live `hane-raster` call
//!
//! `hane-raster/tests/golden/*.png` *is* the oracle's output, already pinned
//! byte for byte by that crate's own harness. Reading it here gets the corpus
//! without lifting 28 fixture-drawing functions out of a test file into a
//! public API nobody else wants, and keeps one definition of what the oracle
//! says. If `golden_corpus` passes, these PNGs are the CPU rasterizer.
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
/// These are **larger than `hane-raster`'s own tolerances and for a different
/// reason**. There, the two sides are the same code on two machines, so the
/// tolerance covers only the last bit of an `f64`. Here the two sides are
/// different algorithms -- scanline with 16 sub-rows against tiled coverage --
/// arriving through different arithmetic, since D-004 narrows to `f32` at the
/// GPU buffer boundary. `f32` has 24 bits of mantissa against `f64`'s 53, and a
/// coverage value that lands on either side of a `1/255` step moves a byte.
///
/// So the defaults below are: **max 2, mean 0.05** for anything with an
/// anti-aliased edge, and **max 0** for fixtures with no coverage arithmetic in
/// them at all. Two counts out of 255 is under 1% and invisible; every bug this
/// harness exists to catch -- a winding rule counted with a `bool`, an edge
/// binned into the wrong tile, a seam conflated into one coverage buffer --
/// moves whole pixels between 0 and 255, three orders of magnitude clear of it.
///
/// ponytail: these are a defensible starting point, not measured ones -- there
/// is no renderer to measure. #19 must either meet them or argue each one it
/// cannot, in the PR that makes them run.
const CASES: &[Case] = cases! {
    // -- no arithmetic that could round differently: edges on pixel bounds,
    //    or nothing drawn at all. Held to exact equality on purpose. --
    rect_pixel_aligned,      max 0, mean 0.0;
    far_offscreen,           max 0, mean 0.0;
    degenerate_empty,        max 0, mean 0.0;
    degenerate_zero_area,    max 0, mean 0.0;
    // -- anti-aliased edges: the f32-vs-f64 coverage budget above --
    rect_subpixel_offsets,   max 2, mean 0.05;
    triangle,                max 2, mean 0.05;
    circle,                  max 2, mean 0.05;
    quad_petals,             max 2, mean 0.05;
    pentagram,               max 2, mean 0.05;
    nested_triangles,        max 2, mean 0.05;
    annulus_reverse_winding, max 2, mean 0.05;
    annulus_same_winding,    max 2, mean 0.05;
    figure_eight,            max 2, mean 0.05;
    cubic_loop,              max 2, mean 0.05;
    subpixel_squares,        max 2, mean 0.05;
    sliver_diagonal,         max 2, mean 0.05;
    taper_wedge,             max 2, mean 0.05;
    hairline_grid,           max 2, mean 0.05;
    long_thin_diagonal,      max 2, mean 0.05;
    overlap_translucent,     max 2, mean 0.05;
    alpha_on_transparent,    max 2, mean 0.05;
    degenerate_nonfinite,    max 2, mean 0.05;
    open_subpath,            max 2, mean 0.05;
    // Six bars from 1/32 of a pixel tall upward. The oracle samples 16 sub-rows
    // per pixel, so a bar thinner than 1/16 catches a sample line or misses it
    // depending where it sits, and a GPU computing coverage analytically gets
    // the *right* answer -- a different one. Held loose deliberately: tightening
    // this would be demanding the GPU reproduce the oracle's quantisation.
    sliver_rows,             max 24, mean 0.6;
    // The GPU has 16x16 tiles and the oracle has none, so these are the two
    // fixtures where a binning bug shows and nothing else does. Full default
    // tolerance, no slack: a boundary edge binned into both tiles or neither is
    // a whole-pixel error.
    seam_shared_edge,        max 2, mean 0.05;
    tile_boundary_rects,     max 2, mean 0.05;
    // One pixel of canvas: the mean is over four channels, so it has to be
    // allowed to equal the max or it is the stricter of the two bounds.
    tiny_canvas,             max 2, mean 2.0;
    // `extreme_coords` is absent on purpose. Its vertices are at 1e9, where an
    // f32 has a 64-pixel quantum (D-004) -- the GPU cannot land those edges on
    // the oracle's sub-pixel answer and no tolerance short of "anything" would
    // let it. Comparing it here would either fail forever or be a tolerance so
    // wide it tests nothing. #19 owns the question of what the GPU should do
    // with coordinates it cannot represent; when it has an answer, that answer
    // needs its own fixture rendered at a scale f32 can hold.
};

/// Renders `name` at `w` by `h` through the GPU pipeline, as premultiplied
/// RGBA, or `None` if there is no GPU renderer to render it with.
///
/// **This is the seam, and it is empty.** `hane-gpu` has a tile binner (#18)
/// and nothing that produces pixels; #19 is the renderer. Filling this in means
/// building the draw data here and submitting it -- which under D-010 cannot
/// happen in a `cargo test`, since submitting needs a GL context that lives in
/// `hane-wasm`. So #19 has to decide one of two things first:
///
/// - a software reference submitter in this test that executes the draw
///   commands `hane-gpu` emits, which keeps the harness native and in CI but
///   verifies the *commands*, not the driver; or
/// - a browser runner that renders for real and writes the framebuffer back to
///   a file this test reads, which verifies the driver but leaves CI.
///
/// Returning `None` until then is the honest answer. It is not "they matched".
fn gpu_render(name: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    let _ = (name, w, h);
    None
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

    // Says plainly what this run did and did not verify, and deletes itself the
    // moment that stops being true: the first fixture #19 can render fails this
    // line, with the instruction attached.
    assert_eq!(
        pending.len(),
        CASES.len(),
        "{} fixture(s) now render on the GPU and were genuinely diffed. Delete \
         this assertion -- it exists only to record that as of #20 none of them \
         did, so that a green run could not be mistaken for a green comparison.",
        CASES.len() - pending.len()
    );
}

/// The self-check the stubbed seam makes necessary.
///
/// With no GPU output to compare, `gpu_matches_oracle` never executes a single
/// comparison, so nothing else here would notice if [`verdict`] stopped working
/// -- a harness that cannot fail is not a harness. This feeds it a real oracle
/// image and a copy perturbed by a known amount, and pins the boundary.
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
