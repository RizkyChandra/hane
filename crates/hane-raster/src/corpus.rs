//! The fixture corpus: the drawings both rasterizers are judged on (D-002).
//!
//! # Why this is in the library and not in a test file
//!
//! It was a test file. Then #19 needed to draw the same twenty-eight pictures
//! on the GPU, from inside a browser, where `hane-raster/tests/golden.rs` does
//! not exist. The choices were a second copy of the corpus or one copy
//! somewhere both sides can reach; a second copy that drifts by one coordinate
//! is an oracle that certifies the wrong answer, so the corpus moved here.
//! `hane-text::testfont` is the same trade already taken once.
//!
//! # What the corpus is for
//!
//! These are the cases a *GPU* gets wrong -- winding at tile boundaries,
//! conflation at a shared edge, sub-pixel shapes that vanish, coordinates that
//! stop being exact in `f32` -- not a gallery of shapes that look nice.
//! Anything that renders identically on every plausible implementation is not
//! paying for its 16 KB.

use crate::fill::{BlendMode, Color, FillRule};
use crate::paint::{Paint, Spread, Stop};
use crate::scene::{Draw, Node, Scene};
use hane_geom::{PathEl, Point};

/// One fixture: a name for its golden, and the drawing.
pub struct Fixture {
    /// File stem of the golden, and the name in a failure message.
    pub name: &'static str,
    /// What to draw.
    pub scene: Scene,
}

/// The whole corpus, in the order failures are reported in.
///
/// Rebuilt per call rather than a `const`: a [`Scene`] owns its paths, and a
/// corpus that had to be `const` would be a corpus of function pointers again.
pub fn fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    let mut add = |name, w, h, f: fn(&mut Scene)| {
        let mut scene = Scene::new(w, h);
        f(&mut scene);
        out.push(Fixture { name, scene });
    };

    // -- baseline geometry, where an off-by-one in the span or row loop shows --
    add("rect_pixel_aligned", 64, 64, rect_pixel_aligned);
    add("rect_subpixel_offsets", 64, 64, rect_subpixel_offsets);
    add("triangle", 64, 64, triangle);
    add("circle", 64, 64, circle);
    add("quad_petals", 64, 64, quad_petals);
    // -- fill rule and self-intersection --
    add("pentagram", 64, 64, pentagram);
    add("nested_triangles", 64, 64, nested_triangles);
    add("annulus_reverse_winding", 64, 64, annulus_reverse_winding);
    add("annulus_same_winding", 64, 64, annulus_same_winding);
    add("figure_eight", 64, 64, figure_eight);
    add("cubic_loop", 64, 64, cubic_loop);
    // -- sub-pixel shapes and thin slivers --
    add("subpixel_squares", 64, 24, subpixel_squares);
    add("sliver_rows", 64, 32, sliver_rows);
    add("sliver_diagonal", 64, 64, sliver_diagonal);
    add("taper_wedge", 64, 64, taper_wedge);
    add("hairline_grid", 64, 64, hairline_grid);
    // -- the ones a tiled GPU rasterizer fails and a scanline one does not --
    add("seam_shared_edge", 64, 64, seam_shared_edge);
    add("tile_boundary_rects", 64, 64, tile_boundary_rects);
    add("long_thin_diagonal", 64, 64, long_thin_diagonal);
    add("overlap_translucent", 64, 64, overlap_translucent);
    add("alpha_on_transparent", 64, 64, alpha_on_transparent);
    // -- extreme coordinates --
    add("extreme_coords", 64, 64, extreme_coords);
    add("far_offscreen", 32, 32, far_offscreen);
    // -- empty and degenerate paths --
    add("degenerate_empty", 32, 32, degenerate_empty);
    add("degenerate_zero_area", 32, 32, degenerate_zero_area);
    add("degenerate_nonfinite", 32, 32, degenerate_nonfinite);
    add("open_subpath", 32, 32, open_subpath);
    add("tiny_canvas", 1, 1, tiny_canvas);
    // -- even-odd, the second reading of the same geometry (#12) --
    add("pentagram_evenodd", 64, 64, pentagram_evenodd);
    add("nested_triangles_evenodd", 64, 64, nested_triangles_evenodd);
    add("annulus_same_evenodd", 64, 64, annulus_same_evenodd);
    add("figure_eight_evenodd", 64, 64, figure_eight_evenodd);
    // -- gradients (#14, #23) --
    add("gradient_linear_pad", 64, 64, gradient_linear_pad);
    add("gradient_spread_modes", 64, 64, gradient_spread_modes);
    add("gradient_radial_focus", 64, 64, gradient_radial_focus);
    add(
        "gradient_partial_coverage",
        64,
        64,
        gradient_partial_coverage,
    );
    // -- clipping (#16, #21) --
    add("clip_midpixel_edge", 64, 64, clip_midpixel_edge);
    add("clip_nested_and_empty", 64, 64, clip_nested_and_empty);
    add("clip_curve_over_gradient", 64, 64, clip_curve_over_gradient);
    // -- blend modes (#13, #22) --
    add("blend_separable", 64, 64, blend_separable);
    add("blend_nonseparable", 64, 64, blend_nonseparable);
    add("blend_isolated_group", 64, 64, blend_isolated_group);

    out
}

// ---------------------------------------------------------------------------
// drawing helpers
// ---------------------------------------------------------------------------

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

fn paper(sc: &mut Scene) {
    let (w, h) = (f64::from(sc.width), f64::from(sc.height));
    sc.fill(rect(0.0, 0.0, w, h), PAPER);
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

/// A fill of `path` with everything spelled out.
fn draw(path: Vec<PathEl>, paint: Paint, rule: FillRule, blend: BlendMode) -> Draw {
    Draw {
        path,
        paint,
        rule,
        blend,
        clip: Vec::new(),
    }
}

fn solid(path: Vec<PathEl>, color: Color, rule: FillRule) -> Draw {
    draw(path, Paint::Solid(color), rule, BlendMode::Normal)
}

fn stops(pairs: &[(f64, Color)]) -> Vec<Stop> {
    pairs
        .iter()
        .map(|&(offset, color)| Stop { offset, color })
        .collect()
}

// ---------------------------------------------------------------------------
// the fixtures
// ---------------------------------------------------------------------------

/// Edges on exact pixel boundaries: every pixel is fully in or fully out, so
/// any anti-aliasing at all here is a bug. The cheapest possible off-by-one
/// detector, and the reason its tolerance is zero.
fn rect_pixel_aligned(sc: &mut Scene) {
    paper(sc);
    sc.fill(rect(8.0, 8.0, 40.0, 24.0), INK);
    sc.fill(rect(48.0, 8.0, 56.0, 56.0), RUST);
}

/// The same rectangle at quarter-pixel offsets. Coverage on the partial edges
/// must be exactly the covered fraction; a renderer that snaps to pixels or
/// rounds coverage the wrong way produces four identical rectangles here.
fn rect_subpixel_offsets(sc: &mut Scene) {
    paper(sc);
    for (i, off) in [0.0, 0.25, 0.5, 0.75].into_iter().enumerate() {
        let x = 6.0 + i as f64 * 15.0 + off;
        let y = 10.0 + off;
        sc.fill(rect(x, y, x + 10.5, y + 20.25), INK);
    }
}

/// Slanted edges at three different slopes, including one near-horizontal,
/// where vertical sub-sampling is least accurate.
fn triangle(sc: &mut Scene) {
    paper(sc);
    sc.fill(poly(&[(4.0, 58.5), (33.25, 3.5), (60.0, 40.75)]), INK);
}

/// Curve flattening: the chord tolerance shows up as flats on the outline, and
/// the total area pins the flattener's error budget.
fn circle(sc: &mut Scene) {
    paper(sc);
    sc.fill(circle_path(32.0, 32.0, 26.5, 1.0), SEA);
}

/// Quadratic segments, which reach the rasterizer through a different arm of
/// the flattener than cubics do.
fn quad_petals(sc: &mut Scene) {
    paper(sc);
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
    sc.fill(els, RUST);
}

/// The pentagram path: five lines through alternate vertices. It crosses itself
/// five times, and the middle pentagon has winding 2 -- nonzero fills it,
/// even-odd leaves it empty.
fn pentagram_path() -> Vec<PathEl> {
    poly(&[
        (32.0, 4.0),
        (48.458, 54.6525),
        (5.3704, 23.3475),
        (58.6296, 23.3475),
        (15.542, 54.6525),
    ])
}

/// The single clearest fill-rule fixture there is, which is why it is first in
/// this group.
fn pentagram(sc: &mut Scene) {
    paper(sc);
    sc.fill(pentagram_path(), INK);
}

/// Three nested triangles wound the same way, so the innermost region has
/// winding 3. Nonzero fills the lot; even-odd gives a target. Winding counted
/// with a `bool` instead of an integer -- an easy GPU shortcut -- fails this.
fn nested_triangles_path() -> Vec<PathEl> {
    let mut els = poly(&[(32.0, 4.0), (56.2487, 46.0), (7.7513, 46.0)]);
    els.extend(poly(&[(32.0, 13.0), (48.4545, 41.5), (15.5455, 41.5)]));
    els.extend(poly(&[(32.0, 22.0), (40.6603, 37.0), (23.3397, 37.0)]));
    els
}

fn nested_triangles(sc: &mut Scene) {
    paper(sc);
    sc.fill(nested_triangles_path(), SEA);
}

/// Outer circle one way, inner circle the other: the windings cancel and the
/// hole appears. This is how every real donut is drawn.
fn annulus_reverse_winding(sc: &mut Scene) {
    paper(sc);
    let mut els = circle_path(32.0, 32.0, 27.0, 1.0);
    els.extend(circle_path(32.0, 32.0, 13.5, -1.0));
    sc.fill(els, INK);
}

/// The same two circles wound the *same* way. Under nonzero this is a solid
/// disc; under even-odd it is a ring. Identical geometry to the fixture above,
/// opposite result -- so a renderer that ignores winding direction entirely
/// cannot pass both.
fn annulus_same_path() -> Vec<PathEl> {
    let mut els = circle_path(32.0, 32.0, 27.0, 1.0);
    els.extend(circle_path(32.0, 32.0, 13.5, 1.0));
    els
}

fn annulus_same_winding(sc: &mut Scene) {
    paper(sc);
    sc.fill(annulus_same_path(), INK);
}

/// A bowtie: one subpath crossing itself at a point, with the two lobes wound
/// oppositely. The crossing is a single pixel where the winding number changes
/// by two, and it sits at a half-pixel so it cannot be hidden by rounding.
fn figure_eight(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        poly(&[(6.0, 6.0), (58.0, 58.0), (6.0, 58.0), (58.0, 6.0)]),
        RUST,
    );
    // A second, smaller bowtie whose crossing lands exactly on a pixel corner,
    // where a half-open sample rule can drop or double-count the vertex.
    sc.fill(
        poly(&[(20.0, 26.0), (44.0, 38.0), (20.0, 38.0), (44.0, 26.0)]),
        SEA,
    );
}

/// A single cubic with a loop in it, so the self-intersection is inside one
/// segment rather than between two. A flattener that subdivides by chord error
/// alone still has to produce edges that close the loop exactly.
fn cubic_loop(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        vec![
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
fn subpixel_squares(sc: &mut Scene) {
    paper(sc);
    for i in 1..=9 {
        let s = f64::from(i) / 10.0;
        let x = 4.0 + f64::from(i) * 6.0;
        sc.fill(rect(x, 8.0, x + s, 8.0 + s), INK);
        // The same square shifted by half a pixel, so it straddles a boundary
        // and its coverage has to split across two pixels.
        sc.fill(rect(x + 0.5, 15.5, x + 0.5 + s, 15.5 + s), RUST);
    }
}

/// Horizontal bars far thinner than the vertical sampling grid. With 16 sub-rows
/// a bar 1/32 of a pixel tall may catch one sample line or none depending where
/// it sits, so this fixture is the direct read-out of the vertical quantisation
/// -- and the place to look when a GPU with a different sample count disagrees.
fn sliver_rows(sc: &mut Scene) {
    paper(sc);
    for (i, t) in [0.03125, 0.0625, 0.125, 0.25, 0.5, 0.75]
        .into_iter()
        .enumerate()
    {
        let y = 3.0 + i as f64 * 5.0;
        sc.fill(rect(4.0, y, 30.0, y + t), INK);
        // Offset by a third of a pixel: the same thickness, a different phase
        // against the sample lines.
        sc.fill(rect(34.0, y + 1.0 / 3.0, 60.0, y + 1.0 / 3.0 + t), RUST);
    }
}

/// A sliver thinner than a pixel running diagonally, so on every scanline the
/// span is a fraction of a pixel wide *and* moves. Nothing exercises the
/// interaction of horizontal analytic coverage with vertical sampling harder.
fn sliver_diagonal(sc: &mut Scene) {
    paper(sc);
    for (i, w) in [0.15, 0.4, 0.9].into_iter().enumerate() {
        let x = 6.0 + i as f64 * 18.0;
        sc.fill(
            poly(&[
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
fn taper_wedge(sc: &mut Scene) {
    paper(sc);
    sc.fill(poly(&[(4.0, 6.0), (12.0, 6.0), (60.0, 30.5)]), SEA);
    sc.fill(poly(&[(60.0, 34.0), (4.0, 56.0), (4.0, 60.0)]), RUST);
}

/// One-pixel bars on the integer grid and on the half-pixel grid. The integer
/// ones must be solid with no fringe; the half-pixel ones must be two rows at
/// half coverage. Getting the pixel centre convention backwards swaps them.
fn hairline_grid(sc: &mut Scene) {
    paper(sc);
    for i in 0..4 {
        let a = 6.0 + f64::from(i) * 14.0;
        sc.fill(rect(a, 4.0, a + 1.0, 28.0), INK);
        sc.fill(rect(4.0, a, 28.0, a + 1.0), INK);
        sc.fill(rect(a + 0.5, 34.0, a + 1.5, 58.0), RUST);
        sc.fill(rect(34.0, a + 0.5, 58.0, a + 1.5), RUST);
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
fn seam_shared_edge(sc: &mut Scene) {
    paper(sc);
    sc.fill(rect(4.0, 4.0, 32.0, 28.0), INK);
    sc.fill(rect(32.0, 4.0, 60.0, 28.0), INK);
    sc.fill(rect(4.0, 34.0, 32.5, 58.0), RUST);
    sc.fill(rect(32.5, 34.0, 60.0, 58.0), RUST);
}

/// Rectangles landing exactly on a 16-pixel tile grid, one straddling it by
/// half a pixel, and one that fills a single tile completely.
///
/// A tiled renderer bins edges per tile and clips spans to tile bounds; an edge
/// that falls precisely on the boundary is the case that gets binned into both
/// tiles or neither. The scanline oracle has no tiles at all, so it renders the
/// obvious answer and the GPU has to match it.
fn tile_boundary_rects(sc: &mut Scene) {
    paper(sc);
    sc.fill(rect(16.0, 0.0, 32.0, 16.0), INK); // exactly one tile
    sc.fill(rect(31.5, 16.0, 48.5, 32.0), RUST); // straddles a boundary
    sc.fill(rect(0.0, 32.0, 64.0, 33.0), SEA); // crosses every tile column
    sc.fill(rect(47.0, 33.0, 48.0, 64.0), SEA); // and every tile row
    sc.fill(rect(16.0, 48.0, 16.25, 64.0), INK); // a quarter-pixel on the line
}

/// A one-pixel-wide diagonal from corner to corner: it clips every tile of any
/// plausible tiling, with a different sub-pixel phase in each.
fn long_thin_diagonal(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        poly(&[
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
fn overlap_translucent(sc: &mut Scene) {
    paper(sc);
    for (cx, cy, c) in [
        (26.0, 24.0, rgba(196, 68, 40, 140)),
        (40.0, 26.0, rgba(24, 130, 148, 140)),
        (32.0, 40.0, rgba(20, 28, 48, 140)),
    ] {
        sc.fill(circle_path(cx, cy, 16.0, 1.0), c);
    }
}

/// The one fixture with no background, so its golden holds *premultiplied*
/// bytes -- which is what the pixmap contract says and what P2 diffs against.
/// It will look darker than it composites when opened in a viewer; that is
/// correct, and the alternative (un-premultiplying) is lossy at low alpha and
/// would stop the golden round-tripping bit-exactly.
fn alpha_on_transparent(sc: &mut Scene) {
    sc.fill(circle_path(24.0, 24.0, 18.0, 1.0), HALF_INK);
    sc.fill(rect(28.5, 28.5, 58.0, 58.0), rgba(196, 68, 40, 64));
    sc.fill(rect(4.0, 44.0, 24.0, 60.0), rgba(24, 130, 148, 255));
}

/// Vertices a billion pixels away, with only a sliver of the shape on screen.
///
/// Two things are under test: that the row range clamps instead of trying to
/// iterate a billion scanlines, and that interpolating `x` along an edge whose
/// endpoints differ by 1e9 still lands within a fraction of a pixel on screen.
/// This is also the fixture that will disagree first on the GPU, where D-004
/// narrows to `f32` and 1e9 has a 64-pixel quantum.
fn extreme_coords(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        poly(&[(-1.0e9, 12.0), (1.0e9, 20.0), (1.0e9, 26.0), (-1.0e9, 18.0)]),
        INK,
    );
    // A wedge whose apex is far off-screen, so the on-screen part is a pair of
    // nearly parallel edges arriving from a great distance.
    sc.fill(poly(&[(1.0e9, -1.0e9), (10.0, 60.0), (54.0, 60.0)]), RUST);
}

/// Entirely off-screen, ten orders of magnitude out. Must render nothing, and
/// must not spend a scanline doing it.
fn far_offscreen(sc: &mut Scene) {
    paper(sc);
    sc.fill(rect(1.0e12, 1.0e12, 1.0e12 + 40.0, 1.0e12 + 40.0), INK);
    sc.fill(rect(-500.0, -500.0, -100.0, -100.0), RUST);
}

/// No path elements at all. The background is the whole fixture.
fn degenerate_empty(sc: &mut Scene) {
    paper(sc);
    sc.fill(Vec::new(), INK);
}

/// Four ways to enclose no area: a single point, a horizontal line, a segment
/// walked out and back, and a `ClosePath` with nothing before it. Each must
/// render nothing rather than a stray pixel or a panic.
fn degenerate_zero_area(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        vec![PathEl::MoveTo(Point::new(8.0, 8.0)), PathEl::ClosePath],
        INK,
    );
    sc.fill(
        vec![
            PathEl::MoveTo(Point::new(4.0, 16.0)),
            PathEl::LineTo(Point::new(28.0, 16.0)),
            PathEl::ClosePath,
        ],
        INK,
    );
    sc.fill(
        vec![
            PathEl::MoveTo(Point::new(4.0, 24.0)),
            PathEl::LineTo(Point::new(28.0, 28.0)),
            PathEl::LineTo(Point::new(4.0, 24.0)),
            PathEl::ClosePath,
        ],
        INK,
    );
    sc.fill(vec![PathEl::ClosePath], INK);
    // A curve with all four control points equal: the flattener must terminate.
    let p = Point::new(16.0, 16.0);
    sc.fill(vec![PathEl::MoveTo(p), PathEl::CurveTo(p, p, p)], INK);
}

/// A subpath full of NaN and infinite coordinates, and a valid triangle, in the
/// *same* fill.
///
/// The rasterizer drops non-finite edges and keeps the rest, so a malformed
/// subpath loses itself rather than poisoning the shape next to it -- a
/// non-finite crossing in the sort would otherwise fill half a scanline.
/// Pinning it here makes the behaviour a decision rather than an accident; an
/// SVG importer will eventually hand this in.
fn degenerate_nonfinite(sc: &mut Scene) {
    paper(sc);
    let mut els = poly(&[
        (4.0, 4.0),
        (f64::NAN, 12.0),
        (20.0, f64::INFINITY),
        (f64::NEG_INFINITY, 20.0),
        (24.0, 4.0),
    ]);
    els.extend(poly(&[(6.0, 18.0), (26.0, 22.5), (10.0, 29.0)]));
    sc.fill(els, INK);
}

/// A subpath with no `ClosePath`. Fills close implicitly, so this must be
/// identical to the same path with one -- which the second shape asserts by
/// drawing exactly that, in a colour that would show through if they differed.
fn open_subpath(sc: &mut Scene) {
    paper(sc);
    sc.fill(
        vec![
            PathEl::MoveTo(Point::new(4.0, 4.0)),
            PathEl::LineTo(Point::new(26.0, 9.0)),
            PathEl::LineTo(Point::new(12.0, 14.0)),
        ],
        INK,
    );
    sc.fill(poly(&[(4.0, 18.0), (26.0, 23.0), (12.0, 28.0)]), INK);
}

/// One pixel of canvas, partly covered. Every loop bound in the rasterizer is
/// degenerate here at once.
fn tiny_canvas(sc: &mut Scene) {
    paper(sc);
    sc.fill(poly(&[(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)]), INK);
}

// -- even-odd: the same four paths, read for parity ---------------------------
//
// Each of these has a nonzero twin above with identical geometry. The pair is
// the fixture: a renderer that ignores the rule, or counts winding with a
// `bool`, cannot produce both pictures.

fn evenodd(sc: &mut Scene, path: Vec<PathEl>, color: Color) {
    paper(sc);
    sc.push(solid(path, color, FillRule::EvenOdd));
}

/// The middle pentagon has winding 2, so even-odd punches it out.
fn pentagram_evenodd(sc: &mut Scene) {
    evenodd(sc, pentagram_path(), INK);
}

/// Windings 1, 2 and 3 nested: even-odd fills the outer band and the innermost
/// triangle and leaves the middle band empty.
fn nested_triangles_evenodd(sc: &mut Scene) {
    evenodd(sc, nested_triangles_path(), SEA);
}

/// Two circles wound alike: a solid disc under nonzero, a ring under even-odd.
fn annulus_same_evenodd(sc: &mut Scene) {
    evenodd(sc, annulus_same_path(), INK);
}

/// The two lobes of a bowtie are wound oppositely, so parity and nonzero agree
/// on them -- the difference is where the lobes overlap the second bowtie.
fn figure_eight_evenodd(sc: &mut Scene) {
    paper(sc);
    let mut path = poly(&[(6.0, 6.0), (58.0, 58.0), (6.0, 58.0), (58.0, 6.0)]);
    path.extend(poly(&[
        (20.0, 26.0),
        (44.0, 38.0),
        (20.0, 38.0),
        (44.0, 26.0),
    ]));
    sc.push(solid(path, RUST, FillRule::EvenOdd));
}

// -- gradients ----------------------------------------------------------------

/// A ramp across the whole canvas between two close colours, so the CPU's
/// ordered dither is the only thing standing between it and visible banding.
/// The GPU has to reproduce the dither, not merely be smooth.
fn gradient_linear_pad(sc: &mut Scene) {
    paper(sc);
    sc.push(draw(
        rect(2.0, 2.0, 62.0, 62.0),
        Paint::Linear {
            start: Point::new(2.0, 2.0),
            end: Point::new(62.0, 62.0),
            stops: stops(&[(0.0, rgba(30, 40, 70, 255)), (1.0, rgba(60, 70, 100, 255))]),
            spread: Spread::Pad,
        },
        FillRule::NonZero,
        BlendMode::Normal,
    ));
}

/// The three spread modes on one axis short enough that all three run off both
/// ends of it. Pad, repeat and reflect differ everywhere outside `[0, 1]` and
/// nowhere inside, so this is the fixture that tells them apart.
fn gradient_spread_modes(sc: &mut Scene) {
    paper(sc);
    for (i, spread) in [Spread::Pad, Spread::Repeat, Spread::Reflect]
        .into_iter()
        .enumerate()
    {
        let y = 4.0 + i as f64 * 20.0;
        sc.push(draw(
            rect(2.0, y, 62.0, y + 16.0),
            Paint::Linear {
                start: Point::new(20.0, 0.0),
                end: Point::new(32.0, 0.0),
                stops: stops(&[(0.0, RUST), (0.45, rgba(240, 200, 60, 255)), (1.0, SEA)]),
                spread,
            },
            FillRule::NonZero,
            BlendMode::Normal,
        ));
    }
}

/// A radial gradient whose focus is well off the centre, which is the case the
/// quadratic in `radial_t` exists for -- and one whose focus is pushed outside
/// the circle, where SVG says to pull it back onto the rim.
fn gradient_radial_focus(sc: &mut Scene) {
    paper(sc);
    let ramp = stops(&[
        (0.0, rgba(255, 240, 200, 255)),
        (0.6, RUST),
        (1.0, rgba(30, 10, 10, 255)),
    ]);
    sc.push(draw(
        circle_path(20.0, 20.0, 17.0, 1.0),
        Paint::Radial {
            center: Point::new(20.0, 20.0),
            radius: 17.0,
            focus: Point::new(13.0, 13.0),
            stops: ramp.clone(),
            spread: Spread::Pad,
        },
        FillRule::NonZero,
        BlendMode::Normal,
    ));
    sc.push(draw(
        rect(30.0, 34.0, 62.0, 62.0),
        Paint::Radial {
            center: Point::new(46.0, 48.0),
            radius: 14.0,
            // Outside the circle: pulled onto it, per SVG.
            focus: Point::new(70.0, 48.0),
            stops: ramp,
            spread: Spread::Reflect,
        },
        FillRule::NonZero,
        BlendMode::Normal,
    ));
}

/// A gradient with a transparent stop under an anti-aliased curve, so coverage
/// and paint alpha multiply at the edge. Premultiplied and straight
/// interpolation differ visibly here, which is the whole reason the ramp is
/// sampled premultiplied.
fn gradient_partial_coverage(sc: &mut Scene) {
    paper(sc);
    sc.push(draw(
        circle_path(32.0, 32.0, 27.5, 1.0),
        Paint::Linear {
            start: Point::new(6.0, 0.0),
            end: Point::new(58.0, 0.0),
            stops: stops(&[
                (0.0, rgba(24, 130, 148, 255)),
                (0.5, rgba(240, 240, 240, 16)),
                (1.0, rgba(196, 68, 40, 255)),
            ]),
            spread: Spread::Pad,
        },
        FillRule::NonZero,
        BlendMode::Normal,
    ));
}

// -- clipping -----------------------------------------------------------------

fn clipped(path: Vec<PathEl>, paint: Paint, clip: Vec<(Vec<PathEl>, FillRule)>) -> Draw {
    Draw {
        path,
        paint,
        rule: FillRule::NonZero,
        blend: BlendMode::Normal,
        clip,
    }
}

/// A clip whose every edge falls mid-pixel, over a shape whose edges do too.
/// The clip must be anti-aliased and must multiply the fill's own coverage --
/// a hard cut shows up immediately as a staircase against the soft fill edge.
fn clip_midpixel_edge(sc: &mut Scene) {
    paper(sc);
    sc.push(clipped(
        rect(4.0, 4.0, 60.0, 60.0),
        Paint::Solid(INK),
        vec![(
            poly(&[(8.5, 6.25), (55.75, 20.5), (30.5, 57.25)]),
            FillRule::NonZero,
        )],
    ));
    // The same clip over a shape that only partly covers its own pixels, so the
    // two coverages multiply rather than one of them being 0 or 1.
    sc.push(clipped(
        circle_path(32.0, 32.0, 22.5, 1.0),
        Paint::Solid(rgba(240, 200, 60, 200)),
        vec![(rect(2.5, 24.5, 61.5, 40.5), FillRule::NonZero)],
    ));
}

/// Three clips that cannot all be satisfied, and one entirely off the canvas.
/// Both must render nothing at all rather than a stray edge pixel, and the
/// pair either side of them must be unaffected.
fn clip_nested_and_empty(sc: &mut Scene) {
    paper(sc);
    sc.push(clipped(
        rect(0.0, 0.0, 64.0, 64.0),
        Paint::Solid(SEA),
        vec![
            (rect(4.0, 4.0, 40.5, 30.0), FillRule::NonZero),
            (rect(20.25, 10.0, 60.0, 46.0), FillRule::NonZero),
            (circle_path(30.0, 20.0, 14.0, 1.0), FillRule::NonZero),
        ],
    ));
    // Intersects to nothing: two rectangles that do not meet.
    sc.push(clipped(
        rect(0.0, 0.0, 64.0, 64.0),
        Paint::Solid(INK),
        vec![
            (rect(0.0, 34.0, 64.0, 40.0), FillRule::NonZero),
            (rect(0.0, 48.0, 64.0, 56.0), FillRule::NonZero),
        ],
    ));
    // Entirely outside the canvas.
    sc.push(clipped(
        rect(0.0, 0.0, 64.0, 64.0),
        Paint::Solid(RUST),
        vec![(rect(-40.0, -40.0, -8.0, -8.0), FillRule::NonZero)],
    ));
    sc.push(clipped(
        rect(0.0, 0.0, 64.0, 64.0),
        Paint::Solid(RUST),
        vec![(circle_path(20.0, 52.0, 9.5, 1.0), FillRule::NonZero)],
    ));
}

/// A curved clip with an even-odd hole in it, over a gradient. The clip carries
/// its own fill rule, which is a different rule from the fill it restricts.
fn clip_curve_over_gradient(sc: &mut Scene) {
    paper(sc);
    let mut ring = circle_path(32.0, 32.0, 28.0, 1.0);
    ring.extend(circle_path(32.0, 32.0, 11.5, 1.0));
    sc.push(clipped(
        rect(0.0, 0.0, 64.0, 64.0),
        Paint::Linear {
            start: Point::new(0.0, 4.0),
            end: Point::new(0.0, 60.0),
            stops: stops(&[
                (0.0, rgba(24, 130, 148, 255)),
                (1.0, rgba(240, 200, 60, 255)),
            ]),
            spread: Spread::Pad,
        },
        vec![
            (ring, FillRule::EvenOdd),
            (rect(6.25, 0.0, 57.75, 64.0), FillRule::NonZero),
        ],
    ));
}

// -- blend modes --------------------------------------------------------------

/// The backdrop every blend fixture works against: three overlapping bands, so
/// each mode is exercised over a dark, a light and a saturated colour.
fn blend_backdrop(sc: &mut Scene) {
    paper(sc);
    sc.fill(rect(0.0, 0.0, 64.0, 22.0), rgba(40, 50, 90, 255));
    sc.fill(rect(0.0, 22.0, 64.0, 43.0), rgba(220, 210, 180, 255));
    sc.fill(rect(0.0, 43.0, 64.0, 64.0), rgba(196, 68, 40, 255));
}

/// A twelve-cell grid, one separable mode per cell, each a translucent square
/// over the same three-band backdrop.
fn blend_separable(sc: &mut Scene) {
    blend_backdrop(sc);
    const MODES: [BlendMode; 12] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::HardLight,
        BlendMode::SoftLight,
        BlendMode::Difference,
        BlendMode::Exclusion,
    ];
    for (i, mode) in MODES.into_iter().enumerate() {
        let x = 1.5 + (i % 4) as f64 * 15.5;
        let y = 2.5 + (i / 4) as f64 * 20.0;
        sc.push(draw(
            rect(x, y, x + 13.5, y + 17.0),
            Paint::Solid(rgba(120, 190, 90, 210)),
            FillRule::NonZero,
            mode,
        ));
    }
}

/// The four non-separable modes, each over the same backdrop and each with a
/// gradient source, so hue and luminosity vary across the cell rather than
/// being one value the formula could accidentally get right.
fn blend_nonseparable(sc: &mut Scene) {
    blend_backdrop(sc);
    const MODES: [BlendMode; 4] = [
        BlendMode::Hue,
        BlendMode::Saturation,
        BlendMode::Color,
        BlendMode::Luminosity,
    ];
    for (i, mode) in MODES.into_iter().enumerate() {
        let x = 2.0 + (i % 2) as f64 * 31.0;
        let y = 3.0 + (i / 2) as f64 * 29.5;
        sc.push(draw(
            rect(x, y, x + 29.0, y + 27.5),
            Paint::Linear {
                start: Point::new(x, y),
                end: Point::new(x + 29.0, y + 27.5),
                stops: stops(&[
                    (0.0, rgba(230, 40, 40, 255)),
                    (0.5, rgba(40, 230, 90, 255)),
                    (1.0, rgba(40, 60, 230, 255)),
                ]),
                spread: Spread::Pad,
            },
            FillRule::NonZero,
            mode,
        ));
    }
}

/// Isolation, stated as a difference.
///
/// The two halves draw the same two shapes with the same `Multiply`. On the
/// left they are a group, so the multiply sees only the group's own contents
/// and the backdrop shows through untouched where the group is transparent. On
/// the right they are drawn straight onto the page and the multiply reaches
/// the backdrop. If the two halves come out the same, isolation is not
/// implemented.
fn blend_isolated_group(sc: &mut Scene) {
    blend_backdrop(sc);
    let members = |x: f64| {
        vec![
            Node::Fill(draw(
                circle_path(x + 11.0, 24.0, 10.0, 1.0),
                Paint::Solid(rgba(250, 240, 120, 230)),
                FillRule::NonZero,
                BlendMode::Normal,
            )),
            Node::Fill(draw(
                circle_path(x + 18.0, 36.0, 10.0, 1.0),
                Paint::Solid(rgba(90, 170, 250, 230)),
                FillRule::NonZero,
                BlendMode::Multiply,
            )),
        ]
    };
    sc.push_group(members(2.0), BlendMode::Normal, 0.85);
    for node in members(34.0) {
        match node {
            Node::Fill(mut d) => {
                // The same two fills, but the group's own 0.85 applied to each
                // member instead of to the finished layer -- which is the
                // difference isolation makes, made visible.
                if let Paint::Solid(c) = &mut d.paint {
                    c.a = (f64::from(c.a) * 0.85).round() as u8;
                }
                sc.push(d);
            }
            Node::Group { .. } => unreachable!("members are fills"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names are file stems and are compared against a directory listing, so a
    /// duplicate would silently make one fixture overwrite another's golden.
    #[test]
    fn fixture_names_are_unique_and_sizes_are_sane() {
        let all = fixtures();
        let mut names: Vec<_> = all.iter().map(|f| f.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate fixture name");
        for f in &all {
            assert!(f.scene.width > 0 && f.scene.height > 0, "{}", f.name);
            assert!(!f.scene.nodes.is_empty(), "{} draws nothing", f.name);
        }
    }

    /// The pairs that exist only to disagree. If a rule change ever made these
    /// identical, the fill-rule fixtures would stop testing anything and no
    /// golden would move to say so.
    #[test]
    fn the_two_fill_rules_disagree_where_they_are_meant_to() {
        let all = fixtures();
        let of = |name: &str| {
            all.iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("no fixture {name}"))
                .scene
                .render()
        };
        for (nonzero, evenodd) in [
            ("pentagram", "pentagram_evenodd"),
            ("nested_triangles", "nested_triangles_evenodd"),
            ("annulus_same_winding", "annulus_same_evenodd"),
            ("figure_eight", "figure_eight_evenodd"),
        ] {
            assert_ne!(of(nonzero).data(), of(evenodd).data(), "{nonzero}");
        }
    }
}
