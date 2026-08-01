//! Golden-image harness over the fixture corpus.
//!
//! Every fixture in `hane_raster::corpus` renders to a pixmap, is compared
//! against a committed PNG in `tests/golden/`, and fails the build if it drifts
//! past that fixture's own tolerance. `UPDATE_GOLDEN=1 cargo test -p
//! hane-raster` rewrites the goldens instead of failing, and writes a diff
//! image beside each one it changed so the regeneration is reviewable rather
//! than a wall of new binary. Add `-- --nocapture` to see which fixtures were
//! rewritten as it happens; otherwise the diff images and `git status` are the
//! record.
//!
//! # Why the corpus is not in this file any more
//!
//! It was, until #19 needed to draw the same pictures on the GPU from inside a
//! browser. A test file is not reachable from there, and a second copy of
//! twenty-eight fixtures is an oracle that certifies whatever it drifted into.
//! The drawings moved into the library; what stayed here is the part that is
//! genuinely about *this* crate's goldens -- the per-fixture tolerance and the
//! compare-or-rewrite loop.
//!
//! # Failure artefacts
//!
//! On a mismatch the harness writes `<name>.actual.png` and `<name>.diff.png`
//! next to the golden and names them in the failure message. The diff image is
//! the point: a mean of 0.4 tells you a renderer is wrong, and only the picture
//! tells you it is wrong along the left edge of every tile.

use hane_raster::{corpus, diff, png};
use std::fs;
use std::path::{Path, PathBuf};

/// How far a fixture may drift from its golden, by name.
///
/// Per fixture, not global: a pixel-aligned rectangle has no arithmetic in it
/// that could round differently, so it is held to zero, while a fixture whose
/// coordinates reach 1e9 gets room for the last bit of an `f64` to land
/// elsewhere on another target.
///
/// Anything absent from this table gets [`DEFAULT`], which is what every
/// anti-aliased fixture wants anyway.
const TOLERANCES: &[(&str, u8, f64)] = &[
    ("rect_pixel_aligned", 0, 0.0),
    ("extreme_coords", 2, 0.10),
    ("far_offscreen", 0, 0.0),
    ("degenerate_empty", 0, 0.0),
    ("degenerate_zero_area", 0, 0.0),
    // One pixel of canvas: the mean is over four channels, so it has to be
    // allowed to equal the max or it is the stricter of the two bounds.
    ("tiny_canvas", 1, 1.0),
];

/// The last bit of an `f64`, which is all two runs of the same code can differ
/// by.
const DEFAULT: (u8, f64) = (1, 0.02);

fn tolerance(name: &str) -> (u8, f64) {
    TOLERANCES
        .iter()
        .find(|(n, _, _)| *n == name)
        .map_or(DEFAULT, |&(_, max, mean)| (max, mean))
}

#[test]
fn golden_corpus() {
    let dir = golden_dir();
    fs::create_dir_all(&dir).expect("create golden dir");
    // Any value but empty or "0", so `UPDATE_GOLDEN=0` reads as "no" rather
    // than as "yes, it is set".
    let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0");

    let fixtures = corpus::fixtures();
    let mut failures = Vec::new();
    for fixture in &fixtures {
        let (max_diff, mean_diff) = tolerance(fixture.name);
        let (w, h) = (fixture.scene.width, fixture.scene.height);
        let pm = fixture.scene.render();

        let path = dir.join(format!("{}.png", fixture.name));
        let prev = fs::read(&path).ok().and_then(|b| png::decode(&b));

        let (mismatch, prev_pixels) = match prev {
            Some((pw, ph, pixels)) if (pw, ph) == (w, h) => {
                let (max, mean) = diff::stats(&pixels, pm.data());
                let bad = max > max_diff || mean > mean_diff;
                (
                    bad.then(|| {
                        format!(
                            "max per-channel diff {max} (tolerance {max_diff}), \
                             mean {mean:.4} (tolerance {mean_diff:.4})"
                        )
                    }),
                    Some(pixels),
                )
            }
            Some((pw, ph, _)) => (
                Some(format!("golden is {pw}x{ph}, fixture is {w}x{h}")),
                None,
            ),
            None => (
                Some("no committed golden, or it is not readable by this decoder".to_string()),
                None,
            ),
        };

        let Some(why) = mismatch else { continue };

        // The diff image is what makes either path reviewable: on a failure it
        // shows where the render moved, and on a regeneration it shows what is
        // about to be committed.
        if let Some(pixels) = &prev_pixels {
            write_png(
                &dir.join(format!("{}.diff.png", fixture.name)),
                w,
                h,
                &diff::image(pixels, pm.data()),
            );
        }

        if update {
            write_png(&path, w, h, pm.data());
            println!("UPDATE_GOLDEN: rewrote {}: {why}", fixture.name);
        } else {
            write_png(
                &dir.join(format!("{}.actual.png", fixture.name)),
                w,
                h,
                pm.data(),
            );
            failures.push(format!(
                "  {}: {why}\n    golden {}\n    actual {}\n    diff   {}",
                fixture.name,
                path.display(),
                dir.join(format!("{}.actual.png", fixture.name)).display(),
                match prev_pixels {
                    Some(_) => dir
                        .join(format!("{}.diff.png", fixture.name))
                        .display()
                        .to_string(),
                    None => "(none: nothing to diff against)".to_string(),
                }
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} golden images differ:\n{}\n\nInspect the diff images. If the change is \
         intended, regenerate with:\n  UPDATE_GOLDEN=1 cargo test -p hane-raster",
        failures.len(),
        fixtures.len(),
        failures.join("\n")
    );
}

/// A golden with no fixture is a rename that left its old file behind, and it
/// would sit in the repo forever testing nothing.
#[test]
fn no_orphaned_goldens() {
    let dir = golden_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return; // nothing committed yet; `golden_corpus` is the test that says so
    };
    let fixtures = corpus::fixtures();
    let orphans: Vec<_> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png") && !n.ends_with(".actual.png") && !n.ends_with(".diff.png"))
        .filter(|n| {
            let stem = n.trim_end_matches(".png");
            !fixtures.iter().any(|f| f.name == stem)
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "goldens with no fixture in {}: {orphans:?}",
        dir.display()
    );
}

/// A tolerance for a fixture that no longer exists is a rename nobody followed
/// through, and it silently leaves the renamed fixture on the default.
#[test]
fn every_tolerance_names_a_fixture() {
    let fixtures = corpus::fixtures();
    for (name, _, _) in TOLERANCES {
        assert!(
            fixtures.iter().any(|f| f.name == *name),
            "tolerance for missing fixture {name}"
        );
    }
}

/// `tests/golden`, resolved from the crate rather than the working directory,
/// which differs between `cargo test` at the workspace root and in the crate.
fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn write_png(path: &Path, w: u32, h: u32, pixels: &[u8]) {
    fs::write(path, png::encode(w, h, pixels))
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
