//! Golden-image harness and fixture corpus.
//!
//! Every fixture renders to a [`Pixmap`], is compared byte for byte against a
//! committed PNG in `tests/golden/`, and fails the build if it drifts past that
//! fixture's own tolerance. `UPDATE_GOLDEN=1 cargo test -p hane-raster` rewrites
//! the goldens instead of failing, and writes a diff image beside each one it
//! changed so the regeneration is reviewable rather than a wall of new binary.
//! Add `-- --nocapture` to see which fixtures were rewritten as it happens;
//! otherwise the diff images and `git status` are the record.
//!
//! # What this corpus is actually for
//!
//! P2 diffs the GPU rasterizer against this crate (D-002), so the coverage here
//! decides how much of the GPU renderer is genuinely verified. The fixtures are
//! therefore chosen as the cases a *GPU* gets wrong -- winding at tile
//! boundaries, conflation at a shared edge, sub-pixel shapes that vanish,
//! coordinates that stop being exact in `f32` -- not as a gallery of shapes
//! that look nice. Anything that renders identically on every plausible
//! implementation is not paying for its 16 KB.
//!
//! # Failure artefacts
//!
//! On a mismatch the harness writes `<name>.actual.png` and `<name>.diff.png`
//! next to the golden and names them in the failure message. The diff image is
//! the point: a mean of 0.4 tells you a renderer is wrong, and only the picture
//! tells you it is wrong along the left edge of every tile.
//!
//! # Not yet covered
//!
//! The rasterizer today does nonzero fills of a solid colour, so these wait on
//! their own issues rather than being faked here:
//!
//! - **Even-odd (#12).** Four fixtures below (`pentagram`, `annulus_same_winding`,
//!   `nested_triangles`, `figure_eight`) are built so that the two rules
//!   disagree -- when #12 lands, render each a second time as `<name>_evenodd`
//!   and the pair pins the difference down.
//! - **Compositing (#13)** beyond source-over of one colour.
//! - **Gradients (#14).** Wanted: a linear gradient across a large shape (banding
//!   and dither), a radial one with a focal point outside the circle, and a
//!   gradient under partial coverage, where premultiplied interpolation and
//!   straight interpolation visibly differ at the edge.
//! - **Clipping (#16).** Wanted: a clip whose edge falls mid-pixel, a clip that
//!   is entirely outside, and nested clips that intersect to nothing.

use hane_raster::{Color, Pixmap, diff, png};
use std::fs;
use std::path::{Path, PathBuf};

/// One fixture: what to draw, how big, and how far it may drift.
struct Fixture {
    /// File stem of the golden, and the name in a failure message.
    name: &'static str,
    /// Canvas size in pixels.
    size: (u32, u32),
    /// Largest per-channel absolute difference tolerated, in `0..=255`.
    ///
    /// Per fixture, not global: a pixel-aligned rectangle has no arithmetic in
    /// it that could round differently, so it is held to zero, while a fixture
    /// whose coordinates reach 1e9 gets room for the last bit of a `f64` to
    /// land elsewhere on another target.
    max_diff: u8,
    /// Largest mean per-channel absolute difference tolerated, over the whole
    /// image. Catches a small error spread over every pixel, which `max_diff`
    /// alone would wave through.
    mean_diff: f64,
    /// Draws the fixture into a fresh transparent pixmap.
    draw: fn(&mut Pixmap),
}

/// Builds [`FIXTURES`], naming each golden after the function that draws it.
///
/// A macro only so the name cannot drift from the function -- a mistyped string
/// would silently compare one fixture against another's golden -- and so
/// rustfmt leaves one fixture per line, which is what makes the corpus
/// reviewable as a list.
macro_rules! corpus {
    ($($draw:ident $w:literal x $h:literal, max $max:literal, mean $mean:literal;)*) => {
        &[$(Fixture {
            name: stringify!($draw),
            size: ($w, $h),
            max_diff: $max,
            mean_diff: $mean,
            draw: $draw,
        },)*]
    };
}

/// The corpus. Order is the order of the failure report, nothing more.
const FIXTURES: &[Fixture] = corpus! {
    // -- baseline geometry, where an off-by-one in the span or row loop shows --
    rect_pixel_aligned      64 x 64, max 0, mean 0.0;
    rect_subpixel_offsets   64 x 64, max 1, mean 0.02;
    triangle                64 x 64, max 1, mean 0.02;
    circle                  64 x 64, max 1, mean 0.02;
    quad_petals             64 x 64, max 1, mean 0.02;
    // -- fill rule and self-intersection --
    pentagram               64 x 64, max 1, mean 0.02;
    nested_triangles        64 x 64, max 1, mean 0.02;
    annulus_reverse_winding 64 x 64, max 1, mean 0.02;
    annulus_same_winding    64 x 64, max 1, mean 0.02;
    figure_eight            64 x 64, max 1, mean 0.02;
    cubic_loop              64 x 64, max 1, mean 0.02;
    // -- sub-pixel shapes and thin slivers --
    subpixel_squares        64 x 24, max 1, mean 0.02;
    sliver_rows             64 x 32, max 1, mean 0.02;
    sliver_diagonal         64 x 64, max 1, mean 0.02;
    taper_wedge             64 x 64, max 1, mean 0.02;
    hairline_grid           64 x 64, max 1, mean 0.02;
    // -- the ones a tiled GPU rasterizer fails and a scanline one does not --
    seam_shared_edge        64 x 64, max 1, mean 0.02;
    tile_boundary_rects     64 x 64, max 1, mean 0.02;
    long_thin_diagonal      64 x 64, max 1, mean 0.02;
    overlap_translucent     64 x 64, max 1, mean 0.02;
    alpha_on_transparent    64 x 64, max 1, mean 0.02;
    // -- extreme coordinates --
    extreme_coords          64 x 64, max 2, mean 0.10;
    far_offscreen           32 x 32, max 0, mean 0.0;
    // -- empty and degenerate paths --
    degenerate_empty        32 x 32, max 0, mean 0.0;
    degenerate_zero_area    32 x 32, max 0, mean 0.0;
    degenerate_nonfinite    32 x 32, max 1, mean 0.02;
    open_subpath            32 x 32, max 1, mean 0.02;
    tiny_canvas              1 x  1, max 1, mean 1.0;
};

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

#[test]
fn golden_corpus() {
    let dir = golden_dir();
    fs::create_dir_all(&dir).expect("create golden dir");
    // Any value but empty or "0", so `UPDATE_GOLDEN=0` reads as "no" rather
    // than as "yes, it is set".
    let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty() && v != "0");

    let mut failures = Vec::new();
    for fixture in FIXTURES {
        let (w, h) = fixture.size;
        let mut pm = Pixmap::new(w, h);
        (fixture.draw)(&mut pm);

        let path = dir.join(format!("{}.png", fixture.name));
        let prev = fs::read(&path).ok().and_then(|b| png::decode(&b));

        let (mismatch, prev_pixels) = match prev {
            Some((pw, ph, pixels)) if (pw, ph) == (w, h) => {
                let (max, mean) = diff::stats(&pixels, pm.data());
                let bad = max > fixture.max_diff || mean > fixture.mean_diff;
                (
                    bad.then(|| {
                        format!(
                            "max per-channel diff {max} (tolerance {}), mean {mean:.4} (tolerance {:.4})",
                            fixture.max_diff, fixture.mean_diff
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
        FIXTURES.len(),
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
    let orphans: Vec<_> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".png") && !n.ends_with(".actual.png") && !n.ends_with(".diff.png"))
        .filter(|n| {
            let stem = n.trim_end_matches(".png");
            !FIXTURES.iter().any(|f| f.name == stem)
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "goldens with no fixture in {}: {orphans:?}",
        dir.display()
    );
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

// ---------------------------------------------------------------------------
// drawing helpers
// ---------------------------------------------------------------------------

use hane_geom::{PathEl, Point};

/// The background every fixture but `alpha_on_transparent` draws onto.
///
/// Opaque, so the premultiplied pixmap bytes and the straight RGBA a viewer
/// expects are the same bytes, and a golden opened in a viewer is exactly what
/// the rasterizer produced.
const PAPER: Color = rgba(255, 255, 255, 255);
const INK: Color = rgba(20, 28, 48, 255);
const RUST: Color = rgba(196, 68, 40, 255);
const SEA: Color = rgba(24, 130, 148, 255);
const HALF_INK: Color = rgba(20, 28, 48, 128);

const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color { r, g, b, a }
}

fn paper(pm: &mut Pixmap) {
    let (w, h) = (f64::from(pm.width()), f64::from(pm.height()));
    pm.fill_path(&rect(0.0, 0.0, w, h), PAPER);
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<PathEl> {
    poly(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)])
}

/// A closed polygon through `pts`, in the order given -- so the caller controls
/// the winding direction, which several fixtures depend on.
fn poly(pts: &[(f64, f64)]) -> Vec<PathEl> {
    let mut els = vec![PathEl::MoveTo(Point::new(pts[0].0, pts[0].1))];
    els.extend(
        pts[1..]
            .iter()
            .map(|&(x, y)| PathEl::LineTo(Point::new(x, y))),
    );
    els.push(PathEl::ClosePath);
    els
}

/// A circle as four cubic arcs, wound one way for `dir = 1.0` and the other for
/// `dir = -1.0`.
///
/// `dir` mirrors the circle in `y`, which reverses the traversal without
/// touching the shape -- the mirrored circle is the same set of points, so the
/// two windings can be compared pixel for pixel.
///
/// The magic number is the standard quarter-arc control offset,
/// `4/3 * (sqrt(2) - 1)`, written as a literal because it is a constant of the
/// construction and not something to recompute per call.
fn circle_path(cx: f64, cy: f64, r: f64, dir: f64) -> Vec<PathEl> {
    const K: f64 = 0.552_284_749_830_793_4;
    let k = K * r;
    let p = |x: f64, y: f64| Point::new(cx + x, cy + dir * y);
    vec![
        PathEl::MoveTo(p(0.0, -r)),
        PathEl::CurveTo(p(k, -r), p(r, -k), p(r, 0.0)),
        PathEl::CurveTo(p(r, k), p(k, r), p(0.0, r)),
        PathEl::CurveTo(p(-k, r), p(-r, k), p(-r, 0.0)),
        PathEl::CurveTo(p(-r, -k), p(-k, -r), p(0.0, -r)),
        PathEl::ClosePath,
    ]
}

// ---------------------------------------------------------------------------
// the fixtures
// ---------------------------------------------------------------------------

/// Edges on exact pixel boundaries: every pixel is fully in or fully out, so
/// any anti-aliasing at all here is a bug. The cheapest possible off-by-one
/// detector, and the reason its tolerance is zero.
fn rect_pixel_aligned(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&rect(8.0, 8.0, 40.0, 24.0), INK);
    pm.fill_path(&rect(48.0, 8.0, 56.0, 56.0), RUST);
}

/// The same rectangle at quarter-pixel offsets. Coverage on the partial edges
/// must be exactly the covered fraction; a renderer that snaps to pixels or
/// rounds coverage the wrong way produces four identical rectangles here.
fn rect_subpixel_offsets(pm: &mut Pixmap) {
    paper(pm);
    for (i, off) in [0.0, 0.25, 0.5, 0.75].into_iter().enumerate() {
        let x = 6.0 + i as f64 * 15.0 + off;
        let y = 10.0 + off;
        pm.fill_path(&rect(x, y, x + 10.5, y + 20.25), INK);
    }
}

/// Slanted edges at three different slopes, including one near-horizontal,
/// where vertical sub-sampling is least accurate.
fn triangle(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&poly(&[(4.0, 58.5), (33.25, 3.5), (60.0, 40.75)]), INK);
}

/// Curve flattening: the chord tolerance shows up as flats on the outline, and
/// the total area pins the flattener's error budget.
fn circle(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&circle_path(32.0, 32.0, 26.5, 1.0), SEA);
}

/// Quadratic segments, which reach the rasterizer through a different arm of
/// the flattener than cubics do.
fn quad_petals(pm: &mut Pixmap) {
    paper(pm);
    let c = Point::new(32.0, 32.0);
    let mut els = vec![PathEl::MoveTo(c)];
    for (cx, cy, ex, ey) in [
        (60.0, 2.0, 32.0, 6.0),
        (4.0, 4.0, 6.0, 32.0),
        (2.0, 60.0, 32.0, 58.0),
        (60.0, 60.0, 32.0, 32.0),
    ] {
        els.push(PathEl::QuadTo(Point::new(cx, cy), Point::new(ex, ey)));
    }
    els.push(PathEl::ClosePath);
    pm.fill_path(&els, RUST);
}

/// A pentagram drawn as five lines through alternate vertices. It crosses
/// itself five times, and the middle pentagon has winding 2: nonzero fills it,
/// even-odd (#12) leaves it empty. The single clearest fill-rule fixture there
/// is, which is why it is first in this group.
fn pentagram(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &poly(&[
            (32.0, 4.0),
            (48.458, 54.6525),
            (5.3704, 23.3475),
            (58.6296, 23.3475),
            (15.542, 54.6525),
        ]),
        INK,
    );
}

/// Three nested triangles wound the same way, so the innermost region has
/// winding 3. Nonzero fills the lot; even-odd gives a target. Winding counted
/// with a `bool` instead of an integer -- an easy GPU shortcut -- fails this.
fn nested_triangles(pm: &mut Pixmap) {
    paper(pm);
    let mut els = poly(&[(32.0, 4.0), (56.2487, 46.0), (7.7513, 46.0)]);
    els.extend(poly(&[(32.0, 13.0), (48.4545, 41.5), (15.5455, 41.5)]));
    els.extend(poly(&[(32.0, 22.0), (40.6603, 37.0), (23.3397, 37.0)]));
    pm.fill_path(&els, SEA);
}

/// Outer circle one way, inner circle the other: the windings cancel and the
/// hole appears. This is how every real donut is drawn.
fn annulus_reverse_winding(pm: &mut Pixmap) {
    paper(pm);
    let mut els = circle_path(32.0, 32.0, 27.0, 1.0);
    els.extend(circle_path(32.0, 32.0, 13.5, -1.0));
    pm.fill_path(&els, INK);
}

/// The same two circles wound the *same* way. Under nonzero this is a solid
/// disc; under even-odd it is a ring. Identical geometry to the fixture above,
/// opposite result -- so a renderer that ignores winding direction entirely
/// cannot pass both.
fn annulus_same_winding(pm: &mut Pixmap) {
    paper(pm);
    let mut els = circle_path(32.0, 32.0, 27.0, 1.0);
    els.extend(circle_path(32.0, 32.0, 13.5, 1.0));
    pm.fill_path(&els, INK);
}

/// A bowtie: one subpath crossing itself at a point, with the two lobes wound
/// oppositely. The crossing is a single pixel where the winding number changes
/// by two, and it sits at a half-pixel so it cannot be hidden by rounding.
fn figure_eight(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &poly(&[(6.0, 6.0), (58.0, 58.0), (6.0, 58.0), (58.0, 6.0)]),
        RUST,
    );
    // A second, smaller bowtie whose crossing lands exactly on a pixel corner,
    // where a half-open sample rule can drop or double-count the vertex.
    pm.fill_path(
        &poly(&[(20.0, 26.0), (44.0, 38.0), (20.0, 38.0), (44.0, 26.0)]),
        SEA,
    );
}

/// A single cubic with a loop in it, so the self-intersection is inside one
/// segment rather than between two. A flattener that subdivides by chord error
/// alone still has to produce edges that close the loop exactly.
fn cubic_loop(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &[
            PathEl::MoveTo(Point::new(16.0, 44.0)),
            PathEl::CurveTo(
                Point::new(84.0, 2.0),
                Point::new(-20.0, 2.0),
                Point::new(48.0, 44.0),
            ),
            PathEl::ClosePath,
        ],
        INK,
    );
}

/// Squares from a tenth of a pixel to nine tenths, on a common baseline.
/// Coverage must be proportional to area: these are the shapes a renderer
/// that snaps to the pixel grid drops entirely or promotes to whole pixels.
fn subpixel_squares(pm: &mut Pixmap) {
    paper(pm);
    for i in 1..=9 {
        let s = f64::from(i) / 10.0;
        let x = 4.0 + f64::from(i) * 6.0;
        pm.fill_path(&rect(x, 8.0, x + s, 8.0 + s), INK);
        // The same square shifted by half a pixel, so it straddles a boundary
        // and its coverage has to split across two pixels.
        pm.fill_path(&rect(x + 0.5, 15.5, x + 0.5 + s, 15.5 + s), RUST);
    }
}

/// Horizontal bars far thinner than the vertical sampling grid. With 16 sub-rows
/// a bar 1/32 of a pixel tall may catch one sample line or none depending where
/// it sits, so this fixture is the direct read-out of the vertical quantisation
/// -- and the place to look when a GPU with a different sample count disagrees.
fn sliver_rows(pm: &mut Pixmap) {
    paper(pm);
    for (i, t) in [0.03125, 0.0625, 0.125, 0.25, 0.5, 0.75]
        .into_iter()
        .enumerate()
    {
        let y = 3.0 + i as f64 * 5.0;
        pm.fill_path(&rect(4.0, y, 30.0, y + t), INK);
        // Offset by a third of a pixel: the same thickness, a different phase
        // against the sample lines.
        pm.fill_path(&rect(34.0, y + 1.0 / 3.0, 60.0, y + 1.0 / 3.0 + t), RUST);
    }
}

/// A sliver thinner than a pixel running diagonally, so on every scanline the
/// span is a fraction of a pixel wide *and* moves. Nothing exercises the
/// interaction of horizontal analytic coverage with vertical sampling harder.
fn sliver_diagonal(pm: &mut Pixmap) {
    paper(pm);
    for (i, w) in [0.15, 0.4, 0.9].into_iter().enumerate() {
        let x = 6.0 + i as f64 * 18.0;
        pm.fill_path(
            &poly(&[
                (x, 4.0),
                (x + w, 4.0),
                (x + w + 12.0, 60.0),
                (x + 12.0, 60.0),
            ]),
            INK,
        );
    }
}

/// A wedge that narrows from eight pixels to nothing. The last few rows are
/// sub-pixel triangles, and the tip is the classic place for a renderer to
/// either stop early or leave a stray pixel past the end.
fn taper_wedge(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&poly(&[(4.0, 6.0), (12.0, 6.0), (60.0, 30.5)]), SEA);
    pm.fill_path(&poly(&[(60.0, 34.0), (4.0, 56.0), (4.0, 60.0)]), RUST);
}

/// One-pixel bars on the integer grid and on the half-pixel grid. The integer
/// ones must be solid with no fringe; the half-pixel ones must be two rows at
/// half coverage. Getting the pixel centre convention backwards swaps them.
fn hairline_grid(pm: &mut Pixmap) {
    paper(pm);
    for i in 0..4 {
        let a = 6.0 + f64::from(i) * 14.0;
        pm.fill_path(&rect(a, 4.0, a + 1.0, 28.0), INK);
        pm.fill_path(&rect(4.0, a, 28.0, a + 1.0), INK);
        pm.fill_path(&rect(a + 0.5, 34.0, a + 1.5, 58.0), RUST);
        pm.fill_path(&rect(34.0, a + 0.5, 58.0, a + 1.5), RUST);
    }
}

/// Two shapes meeting exactly along a shared edge, filled in separate calls.
///
/// On the integer boundary the two half-covered edges must add to a solid seam.
/// On the half-pixel boundary they cannot: each contributes 50% coverage and
/// source-over of two 50% fills leaves 75%, a visible light line. That line is
/// the *conflation artifact*, it is correct for independent draws, and a GPU
/// that accumulates both shapes into one coverage buffer will not reproduce it
/// -- which is exactly the disagreement P2 needs to find deliberately rather
/// than discover in a screenshot.
fn seam_shared_edge(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&rect(4.0, 4.0, 32.0, 28.0), INK);
    pm.fill_path(&rect(32.0, 4.0, 60.0, 28.0), INK);
    pm.fill_path(&rect(4.0, 34.0, 32.5, 58.0), RUST);
    pm.fill_path(&rect(32.5, 34.0, 60.0, 58.0), RUST);
}

/// Rectangles landing exactly on a 16-pixel tile grid, one straddling it by
/// half a pixel, and one that fills a single tile completely.
///
/// A tiled renderer bins edges per tile and clips spans to tile bounds; an edge
/// that falls precisely on the boundary is the case that gets binned into both
/// tiles or neither. The scanline oracle has no tiles at all, so it renders the
/// obvious answer and the GPU has to match it.
fn tile_boundary_rects(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&rect(16.0, 0.0, 32.0, 16.0), INK); // exactly one tile
    pm.fill_path(&rect(31.5, 16.0, 48.5, 32.0), RUST); // straddles a boundary
    pm.fill_path(&rect(0.0, 32.0, 64.0, 33.0), SEA); // crosses every tile column
    pm.fill_path(&rect(47.0, 33.0, 48.0, 64.0), SEA); // and every tile row
    pm.fill_path(&rect(16.0, 48.0, 16.25, 64.0), INK); // a quarter-pixel on the line
}

/// A one-pixel-wide diagonal from corner to corner: it clips every tile of any
/// plausible tiling, with a different sub-pixel phase in each.
fn long_thin_diagonal(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &poly(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (64.0, 63.0),
            (64.0, 64.0),
            (63.0, 64.0),
            (0.0, 1.0),
        ]),
        INK,
    );
}

/// Translucent shapes over each other, three deep in the middle. Source-over is
/// not commutative in the presence of rounding, so the order of these calls is
/// part of the fixture.
fn overlap_translucent(pm: &mut Pixmap) {
    paper(pm);
    for (cx, cy, c) in [
        (26.0, 24.0, rgba(196, 68, 40, 140)),
        (40.0, 26.0, rgba(24, 130, 148, 140)),
        (32.0, 40.0, rgba(20, 28, 48, 140)),
    ] {
        pm.fill_path(&circle_path(cx, cy, 16.0, 1.0), c);
    }
}

/// The one fixture with no background, so its golden holds *premultiplied*
/// bytes -- which is what the pixmap contract says and what P2 diffs against.
/// It will look darker than it composites when opened in a viewer; that is
/// correct, and the alternative (un-premultiplying) is lossy at low alpha and
/// would stop the golden round-tripping bit-exactly.
fn alpha_on_transparent(pm: &mut Pixmap) {
    pm.fill_path(&circle_path(24.0, 24.0, 18.0, 1.0), HALF_INK);
    pm.fill_path(&rect(28.5, 28.5, 58.0, 58.0), rgba(196, 68, 40, 64));
    pm.fill_path(&rect(4.0, 44.0, 24.0, 60.0), rgba(24, 130, 148, 255));
}

/// Vertices a billion pixels away, with only a sliver of the shape on screen.
///
/// Two things are under test: that the row range clamps instead of trying to
/// iterate a billion scanlines, and that interpolating `x` along an edge whose
/// endpoints differ by 1e9 still lands within a fraction of a pixel on screen.
/// This is also the fixture that will disagree first on the GPU, where D-004
/// narrows to `f32` and 1e9 has a 64-pixel quantum.
fn extreme_coords(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &poly(&[(-1.0e9, 12.0), (1.0e9, 20.0), (1.0e9, 26.0), (-1.0e9, 18.0)]),
        INK,
    );
    // A wedge whose apex is far off-screen, so the on-screen part is a pair of
    // nearly parallel edges arriving from a great distance.
    pm.fill_path(&poly(&[(1.0e9, -1.0e9), (10.0, 60.0), (54.0, 60.0)]), RUST);
}

/// Entirely off-screen, ten orders of magnitude out. Must render nothing, and
/// must not spend a scanline doing it.
fn far_offscreen(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&rect(1.0e12, 1.0e12, 1.0e12 + 40.0, 1.0e12 + 40.0), INK);
    pm.fill_path(&rect(-500.0, -500.0, -100.0, -100.0), RUST);
}

/// No path elements at all. The background is the whole fixture.
fn degenerate_empty(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&[], INK);
}

/// Four ways to enclose no area: a single point, a horizontal line, a segment
/// walked out and back, and a `ClosePath` with nothing before it. Each must
/// render nothing rather than a stray pixel or a panic.
fn degenerate_zero_area(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &[PathEl::MoveTo(Point::new(8.0, 8.0)), PathEl::ClosePath],
        INK,
    );
    pm.fill_path(
        &[
            PathEl::MoveTo(Point::new(4.0, 16.0)),
            PathEl::LineTo(Point::new(28.0, 16.0)),
            PathEl::ClosePath,
        ],
        INK,
    );
    pm.fill_path(
        &[
            PathEl::MoveTo(Point::new(4.0, 24.0)),
            PathEl::LineTo(Point::new(28.0, 28.0)),
            PathEl::LineTo(Point::new(4.0, 24.0)),
            PathEl::ClosePath,
        ],
        INK,
    );
    pm.fill_path(&[PathEl::ClosePath], INK);
    // A curve with all four control points equal: the flattener must terminate.
    let p = Point::new(16.0, 16.0);
    pm.fill_path(&[PathEl::MoveTo(p), PathEl::CurveTo(p, p, p)], INK);
}

/// A subpath full of NaN and infinite coordinates, and a valid triangle, in the
/// *same* fill.
///
/// The rasterizer drops non-finite edges and keeps the rest, so a malformed
/// subpath loses itself rather than poisoning the shape next to it -- a
/// non-finite crossing in the sort would otherwise fill half a scanline.
/// Pinning it here makes the behaviour a decision rather than an accident; an
/// SVG importer will eventually hand this in.
fn degenerate_nonfinite(pm: &mut Pixmap) {
    paper(pm);
    let mut els = poly(&[
        (4.0, 4.0),
        (f64::NAN, 12.0),
        (20.0, f64::INFINITY),
        (f64::NEG_INFINITY, 20.0),
        (24.0, 4.0),
    ]);
    els.extend(poly(&[(6.0, 18.0), (26.0, 22.5), (10.0, 29.0)]));
    pm.fill_path(&els, INK);
}

/// A subpath with no `ClosePath`. Fills close implicitly, so this must be
/// identical to the same path with one -- which the second shape asserts by
/// drawing exactly that, in a colour that would show through if they differed.
fn open_subpath(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(
        &[
            PathEl::MoveTo(Point::new(4.0, 4.0)),
            PathEl::LineTo(Point::new(26.0, 9.0)),
            PathEl::LineTo(Point::new(12.0, 14.0)),
        ],
        INK,
    );
    pm.fill_path(&poly(&[(4.0, 18.0), (26.0, 23.0), (12.0, 28.0)]), INK);
}

/// One pixel of canvas, partly covered. Every loop bound in the rasterizer is
/// degenerate here at once.
fn tiny_canvas(pm: &mut Pixmap) {
    paper(pm);
    pm.fill_path(&poly(&[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)]), INK);
}
