//! Laid-out text as ordinary geometry.
//!
//! A run goes in as glyph ids with pen positions and comes out as [`PathEl`]s
//! in layout units, indistinguishable from a path drawn by hand. That is the
//! whole value: once text is paths, stroking, boolean ops, node editing and
//! SVG export need no text-shaped special case.
//!
//! # The transform, and why its order matters
//!
//! Outlines arrive in font units with y up, as `outline.rs` returns them. Layout
//! works in the engine's user space, where y grows downward and the pen sits
//! on the baseline. So each glyph is scaled by `size / units_per_em` with y
//! negated, and *then* translated to its pen position. Composing the two the
//! other way round scales the pen position as well, which is invisible at
//! `size == units_per_em` -- 2048 in half the fonts on this machine -- and
//! wrong at every other size.

use crate::opentype::Font;
use hane_geom::{Affine, PathEl, Point};

/// One glyph of a laid-out run: which glyph, and where its pen sits.
///
/// Deliberately not tied to any particular shaper's output. Anything that can
/// name a glyph and a baseline origin converts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    /// Index of the glyph in the font.
    pub glyph: u16,
    /// The glyph's origin on the baseline, in layout units.
    pub position: Point,
}

impl Font<'_> {
    /// The whole run as one path in layout units, at `size` units per em.
    ///
    /// Glyphs with no contours -- a space -- and ids the font does not have
    /// contribute nothing. A run is not worth failing over one missing glyph,
    /// and the caller has no better recovery than dropping it either.
    pub fn run_to_path(&self, run: &[PositionedGlyph], size: f64) -> Vec<PathEl> {
        let scale = size / f64::from(self.units_per_em());
        run.iter()
            .flat_map(|g| {
                let t = Affine::translate(g.position.to_vec2())
                    * Affine::scale_non_uniform(scale, -scale);
                self.glyph_outline(g.glyph)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |el| el.transform(t))
            })
            .collect()
    }
}

/// The glyphs of one laid-out [`Line`](crate::layout::Line) as [`PositionedGlyph`]s.
///
/// This bridge exists because the two sides disagree about which way `y` points
/// and nothing else would catch it. `Line::baseline` is measured **downward**
/// from the top of the paragraph, while `PlacedGlyph::y_offset` comes from GPOS
/// and is measured **upward** from the baseline -- so the offset is *subtracted*,
/// not added. Adding it puts every accent on the wrong side of its letter, which
/// looks like a font bug rather than a sign error and is correspondingly nasty to
/// find.
///
/// `size` is only needed by [`run_to_path`](Font::run_to_path); the positions
/// here are already in layout units.
pub fn line_to_glyphs(line: &crate::layout::Line) -> Vec<PositionedGlyph> {
    line.glyphs
        .iter()
        .map(|g| PositionedGlyph {
            glyph: g.id,
            position: Point::new(line.x + g.x + g.x_offset, line.baseline - g.y_offset),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{simple, truetype_font};
    use hane_geom::fuzz::check;

    /// The one glyph every test below uses: a unit square, one em wide at the
    /// synthetic font's 2048 units per em, with its top-left corner first so
    /// the y flip is visible in the very first element.
    fn square_font() -> Vec<u8> {
        truetype_font(&[simple(&[&[
            (0, 0, true),
            (2048, 0, true),
            (2048, 2048, true),
            (0, 2048, true),
        ]])])
    }

    fn at(x: f64, y: f64) -> PositionedGlyph {
        PositionedGlyph {
            glyph: 0,
            position: Point::new(x, y),
        }
    }

    #[test]
    fn scale_applies_before_the_pen_translation() {
        let data = square_font();
        let font = Font::parse(&data).unwrap();
        // 16 units per em over 2048 units per em: an exact power of two, so
        // the expected coordinates are exact and == is the right comparison.
        let path = font.run_to_path(&[at(100.0, 50.0)], 16.0);
        assert_eq!(
            path,
            vec![
                PathEl::MoveTo(Point::new(100.0, 50.0)),
                PathEl::LineTo(Point::new(116.0, 50.0)),
                // y up in the font is y down in layout: the top of the square
                // sits *above* the baseline, at a smaller y.
                PathEl::LineTo(Point::new(116.0, 34.0)),
                PathEl::LineTo(Point::new(100.0, 34.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn the_pen_position_does_not_scale_with_size() {
        // The trap the composition order sets: at size == units_per_em the
        // scale is 1 and both orders agree, so a wrong one passes any test
        // written at a single size.
        let data = square_font();
        let font = Font::parse(&data).unwrap();
        let pen = Point::new(300.0, 700.0);
        for size in [2048.0, 32.0, 1.0] {
            let path = font.run_to_path(&[at(pen.x, pen.y)], size);
            assert_eq!(path[0], PathEl::MoveTo(pen), "size {size}");
        }
    }

    #[test]
    fn each_glyph_lands_at_its_own_pen() {
        let data = square_font();
        let font = Font::parse(&data).unwrap();
        let run = [at(0.0, 0.0), at(64.0, 0.0), at(0.0, 80.0)];
        let path = font.run_to_path(&run, 64.0);
        let starts: Vec<_> = path
            .iter()
            .filter_map(|el| match el {
                PathEl::MoveTo(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(starts, run.iter().map(|g| g.position).collect::<Vec<_>>());
        // One contour each, nothing dropped or duplicated.
        assert_eq!(path.len(), run.len() * 5);
    }

    #[test]
    fn contourless_and_missing_glyphs_are_skipped() {
        let data = truetype_font(&[Vec::new()]);
        let font = Font::parse(&data).unwrap();
        let run = [
            PositionedGlyph {
                glyph: 0, // present, but zero-length: a space
                position: Point::new(10.0, 10.0),
            },
            PositionedGlyph {
                glyph: 9999, // past the end of the font
                position: Point::new(20.0, 10.0),
            },
        ];
        assert!(font.run_to_path(&run, 16.0).is_empty());
    }

    #[test]
    fn every_point_is_the_pen_plus_the_scaled_outline() {
        let data = square_font();
        let font = Font::parse(&data).unwrap();
        let upem = f64::from(font.units_per_em());
        // The square's corners, in font units, in outline order.
        let corners = [(0.0, 0.0), (2048.0, 0.0), (2048.0, 2048.0), (0.0, 2048.0)];
        check(
            "run_to_path places every point at pen + scaled outline",
            500,
            |r| (r.point(), r.coord()),
            |&(pen, size)| {
                let path = font.run_to_path(&[at(pen.x, pen.y)], size);
                let scale = size / upem;
                path.iter().zip(corners).all(|(el, (fx, fy))| {
                    let want = Point::new(pen.x + fx * scale, pen.y - fy * scale);
                    // Exact: the transform is a diagonal matrix, so each
                    // coordinate is one multiply and one add, the same two
                    // operations the expectation performs.
                    matches!(el, PathEl::MoveTo(p) | PathEl::LineTo(p) if *p == want)
                })
            },
        );
    }

    #[test]
    fn an_empty_run_is_an_empty_path() {
        let data = square_font();
        let font = Font::parse(&data).unwrap();
        assert!(font.run_to_path(&[], 16.0).is_empty());
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use crate::layout::{Line, PlacedGlyph};

    fn placed(id: u16, x: f64, x_offset: f64, y_offset: f64) -> PlacedGlyph {
        PlacedGlyph {
            id,
            cluster: 0,
            x,
            advance: 10.0,
            x_offset,
            y_offset,
        }
    }

    #[test]
    fn gpos_y_offset_is_subtracted_because_the_axes_disagree() {
        // baseline grows downward; y_offset is "above the baseline". A mark
        // raised 3 units must land 3 units *nearer the top* of the page.
        let line = Line {
            range: 0..1,
            glyphs: vec![placed(1, 0.0, 0.0, 3.0)],
            baseline: 100.0,
            x: 0.0,
            width: 10.0,
        };
        let g = line_to_glyphs(&line);
        assert_eq!(
            g[0].position.y, 97.0,
            "adding instead of subtracting gives 103"
        );
    }

    #[test]
    fn line_x_and_glyph_offsets_all_land_on_the_pen() {
        let line = Line {
            range: 0..2,
            glyphs: vec![placed(1, 0.0, 2.0, 0.0), placed(2, 10.0, -1.0, 0.0)],
            baseline: 50.0,
            x: 7.0,
            width: 20.0,
        };
        let g = line_to_glyphs(&line);
        assert_eq!(g[0].position, Point::new(9.0, 50.0));
        assert_eq!(g[1].position, Point::new(16.0, 50.0));
    }
}
