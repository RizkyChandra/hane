//! Clip regions, as coverage that multiplies a fill's own (#16).
//!
//! A clip is a per-pixel mask in `[0, 1]`, rasterized by exactly the same
//! scanline code as a fill. Clipping then multiplies the two coverages before
//! anything is composited, which is what makes the clip edge anti-aliased
//! rather than a staircase: coverage `0.5` inside a shape that is itself
//! `0.5` covered contributes a quarter of a pixel, not half or none.
//!
//! The alternative -- render, then mask the result bytes -- loses that. It
//! multiplies an already-rounded 8-bit alpha, so a soft clip over a soft edge
//! quantizes twice, and it cannot express "these two shapes overlap here" at
//! all once the first has been flattened into the buffer.

use crate::fill::{FillRule, build_edges, coverage_row, edge_rows};
use hane_geom::PathEl;

/// A clip region: the coverage of one or more paths, intersected.
///
/// Sized for one pixmap, and only usable with that size. Nesting is
/// [`Clip::intersect_path`] rather than a stack -- a stack would only ever be
/// read as the product of its members, and this stores that product.
#[derive(Clone, Debug)]
pub struct Clip {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Coverage in `[0, 1]`, row-major, one per pixel.
    ///
    /// ponytail: `f64` per pixel, so a 4k mask is 128 MB. A `u8` mask would be
    /// eight times smaller and quantize every clip edge to 1/255 before it is
    /// multiplied; this crate is the oracle P2 is diffed against, so it keeps
    /// the exact number. Narrow it if a real document ever holds many live
    /// clips at once.
    data: Vec<f64>,
    /// The first row that can have any coverage, and one past the last.
    ///
    /// Rows outside it are held at zero, so this is a fast path and not a
    /// separate source of truth.
    pub(crate) y0: u32,
    pub(crate) y1: u32,
}

impl Clip {
    /// The region inside `path` under `rule`, for a `width` by `height` pixmap.
    pub fn from_path(width: u32, height: u32, path: &[PathEl], rule: FillRule) -> Self {
        let len = usize::try_from(u64::from(width) * u64::from(height))
            .expect("clip is larger than this target's address space");
        // Everything, then intersected: one rasterizing path instead of two,
        // and 1.0 is the exact identity of the multiply below.
        let mut clip = Self {
            width,
            height,
            data: vec![1.0; len],
            y0: 0,
            y1: height,
        };
        clip.intersect_path(path, rule);
        clip
    }

    /// Narrows this clip to its intersection with `path` under `rule`.
    ///
    /// Nested clips are exactly this: coverage is a product, so a third clip is
    /// a third factor and the order they are applied in cannot matter.
    pub fn intersect_path(&mut self, path: &[PathEl], rule: FillRule) {
        let width = self.width as usize;
        if width == 0 || self.is_empty() {
            return;
        }
        let edges = build_edges(path);
        // Empty for an empty path, which is how an empty clip arises.
        let (py0, py1) = edge_rows(&edges, self.height);

        let mut acc = vec![0.0; width];
        let mut crossings = Vec::new();
        for y in self.y0..self.y1 {
            let row = &mut self.data[y as usize * width..][..width];
            if y < py0 || y >= py1 {
                // Outside the new path entirely. Zeroed rather than left to the
                // row bounds alone, so the buffer never disagrees with them.
                row.fill(0.0);
                continue;
            }
            coverage_row(&edges, y, rule, &mut acc, &mut crossings);
            for (m, &c) in row.iter_mut().zip(acc.iter()) {
                *m *= c;
            }
        }
        self.y0 = self.y0.max(py0);
        self.y1 = self.y1.min(py1).max(self.y0);
    }

    /// True when nothing can survive this clip.
    pub(crate) fn is_empty(&self) -> bool {
        self.width == 0 || self.y1 <= self.y0
    }

    /// One row of the mask.
    pub(crate) fn row(&self, y: u32) -> &[f64] {
        let width = self.width as usize;
        &self.data[y as usize * width..][..width]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, Pixmap};
    use hane_geom::Point;

    const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    fn poly(points: &[(f64, f64)]) -> Vec<PathEl> {
        let mut els = vec![PathEl::MoveTo(Point::new(points[0].0, points[0].1))];
        els.extend(
            points[1..]
                .iter()
                .map(|&(x, y)| PathEl::LineTo(Point::new(x, y))),
        );
        els.push(PathEl::ClosePath);
        els
    }

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<PathEl> {
        poly(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)])
    }

    fn clipped(w: u32, h: u32, path: &[PathEl], clip: Option<&Clip>) -> Pixmap {
        let mut pm = Pixmap::new(w, h);
        pm.fill_path_with(path, &crate::Paint::Solid(WHITE), FillRule::NonZero, clip);
        pm
    }

    fn cov(pm: &Pixmap, x: u32, y: u32) -> u8 {
        pm.data()[((y * pm.width() + x) * 4 + 3) as usize]
    }

    #[test]
    fn clipping_to_a_pixel_rect_matches_a_cropped_render() {
        // The strongest form of the criterion: byte-for-byte, not close.
        let shape = poly(&[(2.0, 1.5), (28.0, 4.0), (20.0, 26.0), (3.0, 18.0)]);
        let clip = Clip::from_path(32, 32, &rect(8.0, 8.0, 20.0, 24.0), FillRule::NonZero);
        let got = clipped(32, 32, &shape, Some(&clip));

        // The manual crop: the unclipped render, zeroed outside the rect.
        let unclipped = clipped(32, 32, &shape, None);
        let mut want = unclipped.data().to_vec();
        for y in 0..32u32 {
            for x in 0..32u32 {
                if !(8..20).contains(&x) || !(8..24).contains(&y) {
                    want[((y * 32 + x) * 4) as usize..][..4].fill(0);
                }
            }
        }
        assert_eq!(got.data(), &want[..]);
        // Not vacuous: the clip actually removed something.
        assert!(unclipped.data() != &want[..]);
    }

    #[test]
    fn a_clip_edge_is_anti_aliased_not_a_hard_cut() {
        // A diagonal clip across a solid square. The pixels on the diagonal
        // must be partial, and exactly as partial as filling the diagonal
        // shape itself would make them.
        let tri = poly(&[(0.0, 0.0), (16.0, 16.0), (0.0, 16.0)]);
        let clip = Clip::from_path(16, 16, &tri, FillRule::NonZero);
        let got = clipped(16, 16, &rect(0.0, 0.0, 16.0, 16.0), Some(&clip));
        let want = clipped(16, 16, &tri, None);
        assert_eq!(got.data(), want.data());
        // Not vacuous: the diagonal really is a ramp.
        assert_eq!(cov(&got, 5, 5), 128);
    }

    #[test]
    fn a_partial_clip_scales_a_partial_coverage() {
        // A fully covered pixel under a half clip is a half, and the AA of the
        // clip edge survives into the result rather than being rounded to in
        // or out.
        let clip = Clip::from_path(4, 4, &rect(0.0, 0.0, 1.5, 4.0), FillRule::NonZero);
        let got = clipped(4, 4, &rect(1.0, 0.0, 4.0, 4.0), Some(&clip));
        assert_eq!(cov(&got, 0, 0), 0, "outside the fill");
        assert_eq!(cov(&got, 1, 0), 128, "half a clip over a whole pixel");
        assert_eq!(cov(&got, 2, 0), 0, "outside the clip");

        // ponytail: coverage is multiplied, which assumes the two coverages
        // are independent inside the pixel. Where they are not -- a clip edge
        // lying on the fill edge -- the product is the conflation artifact
        // below: 1/4 where the true intersection is 1/2. Fixing it needs the
        // clip and the fill rasterized into one span list, which is a much
        // larger change than #16 asks for and which P2 cannot follow anyway.
        let same = clipped(4, 4, &rect(0.0, 0.0, 1.5, 4.0), Some(&clip));
        assert_eq!(cov(&same, 1, 0), 64);
    }

    #[test]
    fn nested_clips_intersect() {
        let mut clip = Clip::from_path(16, 16, &rect(2.0, 2.0, 12.0, 12.0), FillRule::NonZero);
        clip.intersect_path(&rect(6.0, 0.0, 16.0, 8.0), FillRule::NonZero);
        let got = clipped(16, 16, &rect(0.0, 0.0, 16.0, 16.0), Some(&clip));
        for y in 0..16u32 {
            for x in 0..16u32 {
                let inside = (6..12).contains(&x) && (2..8).contains(&y);
                assert_eq!(cov(&got, x, y), if inside { 255 } else { 0 }, "({x}, {y})");
            }
        }
        // Order cannot matter, and neither can doing it in one path.
        let mut other = Clip::from_path(16, 16, &rect(6.0, 0.0, 16.0, 8.0), FillRule::NonZero);
        other.intersect_path(&rect(2.0, 2.0, 12.0, 12.0), FillRule::NonZero);
        assert_eq!(
            clipped(16, 16, &rect(0.0, 0.0, 16.0, 16.0), Some(&other)).data(),
            got.data()
        );
    }

    #[test]
    fn an_empty_clip_renders_nothing() {
        let cases = [
            // No path at all.
            Clip::from_path(16, 16, &[], FillRule::NonZero),
            // A path with no area.
            Clip::from_path(
                16,
                16,
                &poly(&[(1.0, 1.0), (5.0, 1.0), (3.0, 1.0)]),
                FillRule::NonZero,
            ),
            // Entirely off the pixmap.
            Clip::from_path(16, 16, &rect(-8.0, -8.0, -2.0, -2.0), FillRule::NonZero),
            // And two clips that do not meet.
            {
                let mut c = Clip::from_path(16, 16, &rect(0.0, 0.0, 16.0, 4.0), FillRule::NonZero);
                c.intersect_path(&rect(0.0, 10.0, 16.0, 16.0), FillRule::NonZero);
                c
            },
        ];
        for clip in &cases {
            assert!(clip.is_empty());
            let got = clipped(16, 16, &rect(0.0, 0.0, 16.0, 16.0), Some(clip));
            assert!(got.data().iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn a_clip_honours_its_own_fill_rule() {
        // Two squares wound alike: even-odd punches the overlap out of the
        // clip, so the fill through it has a hole in the same place.
        let mut path = rect(0.0, 0.0, 8.0, 8.0);
        path.extend(rect(4.0, 4.0, 12.0, 12.0));
        let solid = Clip::from_path(16, 16, &path, FillRule::NonZero);
        let holed = Clip::from_path(16, 16, &path, FillRule::EvenOdd);
        let whole = rect(0.0, 0.0, 16.0, 16.0);
        assert_eq!(cov(&clipped(16, 16, &whole, Some(&solid)), 6, 6), 255);
        assert_eq!(cov(&clipped(16, 16, &whole, Some(&holed)), 6, 6), 0);
        assert_eq!(cov(&clipped(16, 16, &whole, Some(&holed)), 2, 2), 255);
    }

    #[test]
    #[should_panic(expected = "clip is 8x8 but the pixmap is 16x16")]
    fn a_mismatched_clip_is_rejected() {
        let clip = Clip::from_path(8, 8, &rect(0.0, 0.0, 8.0, 8.0), FillRule::NonZero);
        clipped(16, 16, &rect(0.0, 0.0, 16.0, 16.0), Some(&clip));
    }

    #[test]
    fn a_zero_sized_clip_is_harmless() {
        let mut clip = Clip::from_path(0, 0, &rect(0.0, 0.0, 4.0, 4.0), FillRule::NonZero);
        clip.intersect_path(&rect(0.0, 0.0, 2.0, 2.0), FillRule::NonZero);
        let pm = clipped(0, 0, &rect(0.0, 0.0, 4.0, 4.0), Some(&clip));
        assert!(pm.data().is_empty());
    }
}
