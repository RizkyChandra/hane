//! Dash patterns, measured in arc length.
//!
//! # Why arc length and not parameter
//!
//! A Bezier's parameter is not its length: on a curve with a slow end and a
//! fast one, cutting at `t = 1/2` puts the mark nowhere near the middle, and a
//! pattern applied in parameter space would visibly stretch and squash along
//! one curve. P0's `t_at_length` is the inverse that fixes it, and it is the
//! whole reason dashing waited for it.
//!
//! Nothing here decides what a dash *looks* like. Dashing produces a path of
//! open subpaths, and [`Path::stroke`](crate::Path::stroke) strokes it -- so
//! each dash gets caps rather than joins at its ends for free, and a dash of
//! zero length comes out as a lone `MoveTo`, which is exactly the dot a round
//! cap draws there.

use hane_geom::{PathEl, Point};

use super::push_seg;
use crate::{Path, Segment};

impl Path {
    /// This path cut into the "on" spans of `pattern`, starting `offset` into
    /// it.
    ///
    /// Lengths are arc lengths, and the pattern runs continuously across
    /// segment boundaries within a subpath -- a dash that starts on one curve
    /// and ends on the next is one subpath, not two. It restarts at every
    /// subpath, which is what SVG specifies.
    ///
    /// An odd-length pattern repeats doubled, so `[3]` is `[3, 3]` and
    /// `[1, 2, 3]` is `[1, 2, 3, 1, 2, 3]` with the roles alternating. A
    /// pattern that is empty, has a negative or non-finite entry, or sums to
    /// zero is an error under SVG and leaves the path undashed.
    ///
    /// ```
    /// use hane_geom::{PathEl, Point};
    /// use hane_path::Path;
    ///
    /// let line = Path::from(vec![
    ///     PathEl::MoveTo(Point::new(0.0, 0.0)),
    ///     PathEl::LineTo(Point::new(10.0, 0.0)),
    /// ]);
    /// // On for 4, off for 4: three dashes, the last one cut short.
    /// assert_eq!(line.dash(&[4.0], 0.0).subpaths().count(), 2);
    /// ```
    pub fn dash(&self, pattern: &[f64], offset: f64) -> Path {
        let sum: f64 = pattern.iter().sum();
        // Odd patterns repeat doubled, so the period -- what the offset is
        // reduced against -- is twice the sum.
        let period = if pattern.len() % 2 == 1 {
            sum * 2.0
        } else {
            sum
        };
        if pattern.is_empty()
            || !pattern.iter().all(|v| *v >= 0.0)
            || !(period > 0.0 && period.is_finite())
            || !offset.is_finite()
        {
            return self.clone();
        }

        let mut els = Vec::new();
        for sub in self.subpaths() {
            let mut d = Dashes::new(pattern, offset, period);
            let mut started = false;
            for seg in sub.segments() {
                d.cut(seg, &mut started, &mut els);
            }
            // A subpath with no length still has a position, and a dot there
            // is what a round cap draws. Dropping it would lose the dot that
            // survived the walk above for every other zero-length subpath.
            if !started && d.on && sub.segments().next().is_none() {
                els.push(PathEl::MoveTo(sub.start()));
            }
        }
        Path::from(els)
    }
}

/// The walk's position in the pattern, carried from segment to segment.
struct Dashes<'a> {
    pattern: &'a [f64],
    /// Index into the pattern, unreduced: its parity is the on/off state for
    /// an even-length pattern, and `on` tracks it separately for an odd one.
    i: usize,
    /// Length left in the current entry.
    remaining: f64,
    on: bool,
}

impl<'a> Dashes<'a> {
    fn new(pattern: &'a [f64], offset: f64, period: f64) -> Self {
        let mut d = Self {
            pattern,
            i: 0,
            remaining: pattern[0],
            on: true,
        };
        // `rem_euclid` handles a negative offset, which SVG allows and which
        // means the same phase as its positive complement.
        let mut phase = offset.rem_euclid(period);
        // Guarded on `phase > 0` rather than on the entry being consumable, so
        // a zero-length entry at phase zero is *entered* and draws its dot,
        // instead of being stepped over.
        while phase > 0.0 && phase >= d.remaining {
            phase -= d.remaining;
            d.advance();
        }
        d.remaining -= phase;
        d
    }

    fn advance(&mut self) {
        self.i += 1;
        self.on = !self.on;
        self.remaining = self.pattern[self.i % self.pattern.len()];
    }

    /// Cuts one segment into the pattern, appending the "on" pieces.
    ///
    /// `started` says whether the dash being built already has a `MoveTo`; it
    /// outlives the segment because a dash spans segment boundaries.
    fn cut(&mut self, seg: Segment, started: &mut bool, els: &mut Vec<PathEl>) {
        let total = length(seg);
        let mut pos = 0.0;
        while pos < total {
            let take = self.remaining.min(total - pos);
            if self.on {
                // ponytail: `t_at_length` re-integrates the whole segment on
                // every call, so this is quadratic in the dashes per segment.
                // Build a length table per segment if a pattern ever puts
                // thousands of stops on one curve.
                let t0 = t_at_length(seg, pos);
                let piece = (take > 0.0).then(|| subsegment(seg, t0, t_at_length(seg, pos + take)));
                if !*started {
                    els.push(PathEl::MoveTo(
                        piece.map_or_else(|| point_at(seg, t0), Segment::start),
                    ));
                    *started = true;
                }
                if let Some(piece) = piece {
                    push_seg(els, piece);
                }
            }
            pos += take;
            self.remaining -= take;
            if self.remaining <= 0.0 {
                self.advance();
                // The next "on" span is a new subpath: dashes do not join.
                *started = false;
            }
        }
    }
}

/// The arc length of a whole segment.
fn length(s: Segment) -> f64 {
    match s {
        Segment::Line(p0, p1) => p0.distance(p1),
        Segment::Quad(q) => q.length_at_t(1.0),
        Segment::Cubic(c) => c.length_at_t(1.0),
    }
}

/// The parameter at arc length `len` from the start of `s`.
fn t_at_length(s: Segment, len: f64) -> f64 {
    match s {
        Segment::Line(p0, p1) => {
            let total = p0.distance(p1);
            if total > 0.0 {
                (len / total).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
        Segment::Quad(q) => q.t_at_length(len),
        Segment::Cubic(c) => c.t_at_length(len),
    }
}

/// The piece of `s` over `[t0, t1]`, exact at both ends.
fn subsegment(s: Segment, t0: f64, t1: f64) -> Segment {
    match s {
        Segment::Line(p0, p1) => Segment::Line(p0.lerp(p1, t0), p0.lerp(p1, t1)),
        Segment::Quad(q) => Segment::Quad(q.subsegment(t0, t1)),
        Segment::Cubic(c) => Segment::Cubic(c.subsegment(t0, t1)),
    }
}

/// The point of `s` at parameter `t`.
fn point_at(s: Segment, t: f64) -> Point {
    match s {
        Segment::Line(p0, p1) => p0.lerp(p1, t),
        Segment::Quad(q) => q.eval(t),
        Segment::Cubic(c) => c.eval(t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cap, StrokeStyle};
    use hane_geom::{CubicBez, Point, Rect};

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    /// The arc length of each dashed subpath, in order.
    fn dash_lengths(path: &Path) -> Vec<f64> {
        path.subpaths()
            .map(|sub| sub.segments().map(length).sum())
            .collect()
    }

    fn horizontal(len: f64) -> Path {
        Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(len, 0.0)),
        ])
    }

    /// A cubic whose speed varies by a factor of about nine end to end, so
    /// parameter-space dashing would be visibly wrong on it.
    fn uneven() -> Path {
        Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::CurveTo(p(1.0, 0.0), p(2.0, 0.0), p(12.0, 0.0)),
        ])
    }

    #[test]
    fn dash_lengths_are_arc_length_not_parameter() {
        let path = uneven();
        let total: f64 = path.segments().map(length).sum();
        let dashed = path.dash(&[1.0, 1.0], 0.0);
        let lengths = dash_lengths(&dashed);
        assert_eq!(lengths.len(), (total / 2.0).ceil() as usize);
        // Every dash but the last is exactly one long, wherever it fell on the
        // curve. In parameter space these would run from 0.3 to 4.
        for (i, l) in lengths.iter().enumerate() {
            if i + 1 < lengths.len() {
                assert!((l - 1.0).abs() < 1e-6, "dash {i} is {l} long");
            }
        }
    }

    #[test]
    fn the_offset_shifts_the_pattern() {
        // Pattern [2, 2] on a length of 10, starting 1 in: the first dash is
        // cut short to 1, then the rest fall on the shifted grid.
        let dashed = horizontal(10.0).dash(&[2.0, 2.0], 1.0);
        assert_eq!(dashed.subpaths().next().unwrap().start(), p(0.0, 0.0));
        let lengths = dash_lengths(&dashed);
        assert!((lengths[0] - 1.0).abs() < 1e-12, "{lengths:?}");
        assert!((lengths[1] - 2.0).abs() < 1e-12, "{lengths:?}");
        // A negative offset is the same phase as its positive complement, and
        // a whole period of offset changes nothing.
        assert_eq!(
            horizontal(10.0).dash(&[2.0, 2.0], -3.0),
            horizontal(10.0).dash(&[2.0, 2.0], 1.0)
        );
        assert_eq!(
            horizontal(10.0).dash(&[2.0, 2.0], 4.0),
            horizontal(10.0).dash(&[2.0, 2.0], 0.0)
        );
    }

    #[test]
    fn a_pattern_wraps_continuously_across_segment_boundaries() {
        // Two unit lines meeting at (1, 0), dashed at 1.5: the first dash must
        // cross the join as one subpath, not end at it.
        let path = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(1.0, 0.0)),
            PathEl::LineTo(p(2.0, 0.0)),
        ]);
        let dashed = path.dash(&[1.5, 0.25], 0.0);
        let subs: Vec<_> = dashed.subpaths().collect();
        assert_eq!(subs[0].segments().count(), 2);
        assert!((dash_lengths(&dashed)[0] - 1.5).abs() < 1e-12);
        // And the state carries: the second dash starts 0.25 later.
        assert_eq!(subs[1].start(), p(1.75, 0.0));
    }

    #[test]
    fn an_odd_pattern_repeats_doubled() {
        // [1, 2, 3] is [1 on, 2 off, 3 on, 1 off, 2 on, 3 off], period 12.
        let dashed = horizontal(12.0).dash(&[1.0, 2.0, 3.0], 0.0);
        let lengths = dash_lengths(&dashed);
        assert_eq!(lengths.len(), 3);
        for (got, want) in lengths.iter().zip([1.0, 3.0, 2.0]) {
            assert!((got - want).abs() < 1e-12, "{lengths:?}");
        }
        // Which makes the period twelve, not six.
        assert_eq!(
            horizontal(12.0).dash(&[1.0, 2.0, 3.0], 12.0),
            horizontal(12.0).dash(&[1.0, 2.0, 3.0], 0.0)
        );
        assert_ne!(
            horizontal(12.0).dash(&[1.0, 2.0, 3.0], 6.0),
            horizontal(12.0).dash(&[1.0, 2.0, 3.0], 0.0)
        );
    }

    #[test]
    fn a_zero_length_dash_with_round_caps_renders_a_dot() {
        // The dotted-line pattern: no on-length at all, one dot every 4.
        let dashed = horizontal(10.0).dash(&[0.0, 4.0], 0.0);
        let subs: Vec<_> = dashed.subpaths().collect();
        assert_eq!(subs.len(), 3);
        for (sub, x) in subs.iter().zip([0.0, 4.0, 8.0]) {
            // A lone `MoveTo`: nothing is drawn, but the position survives.
            assert_eq!(sub.segments().count(), 0);
            assert_eq!(sub.start(), p(x, 0.0));
        }
        let dots = dashed.stroke(&StrokeStyle {
            width: 2.0,
            cap: Cap::Round,
            tolerance: 1e-6,
            ..StrokeStyle::default()
        });
        assert_eq!(dots.subpaths().count(), 3);
        assert_eq!(dots.bounding_box(), Rect::new(-1.0, -1.0, 9.0, 1.0));
        // And nothing at all with butt caps, which have no extent.
        assert_eq!(
            dashed
                .stroke(&StrokeStyle {
                    width: 2.0,
                    ..StrokeStyle::default()
                })
                .elements()
                .len(),
            0
        );
    }

    #[test]
    fn an_invalid_pattern_leaves_the_path_alone() {
        let path = uneven();
        for pattern in [&[][..], &[0.0, 0.0][..], &[-1.0, 2.0][..], &[f64::NAN][..]] {
            assert_eq!(path.dash(pattern, 0.0), path, "{pattern:?}");
        }
        assert_eq!(path.dash(&[1.0], f64::NAN), path);
    }

    #[test]
    fn dashing_a_closed_subpath_restarts_the_pattern() {
        let square = Path::from(vec![
            PathEl::MoveTo(p(0.0, 0.0)),
            PathEl::LineTo(p(4.0, 0.0)),
            PathEl::LineTo(p(4.0, 4.0)),
            PathEl::LineTo(p(0.0, 4.0)),
            PathEl::ClosePath,
        ]);
        let dashed = square.dash(&[2.0, 2.0], 0.0);
        // Sixteen units at 2 on, 2 off: four dashes, the first from the seam.
        assert_eq!(dashed.subpaths().count(), 4);
        assert_eq!(dashed.subpaths().next().unwrap().start(), p(0.0, 0.0));
        // ponytail: the dash across the seam is not merged with the first one,
        // so a closed path whose pattern is "on" at the seam shows two butt
        // caps meeting there. Merging means holding the first dash back until
        // the walk ends; worth it when a design tool exposes dash phase.
        assert!(
            dash_lengths(&dashed)
                .iter()
                .all(|l| (l - 2.0).abs() < 1e-12)
        );
    }

    #[test]
    fn dashes_are_cut_from_curves_too() {
        let c = CubicBez::new(p(0.0, 0.0), p(0.0, 4.0), p(6.0, 4.0), p(6.0, 0.0));
        let path = Path::from(vec![
            PathEl::MoveTo(c.p0),
            PathEl::CurveTo(c.p1, c.p2, c.p3),
        ]);
        let dashed = path.dash(&[1.0, 0.5], 0.0);
        // Every dash is on the source curve, and one unit long.
        for sub in dashed.subpaths() {
            for seg in sub.segments() {
                for i in 0..=8 {
                    let q = point_at(seg, f64::from(i) / 8.0);
                    assert!(c.nearest(q).1 < 1e-9, "{q:?} is off the curve");
                }
            }
        }
        let lengths = dash_lengths(&dashed);
        for l in &lengths[..lengths.len() - 1] {
            assert!((l - 1.0).abs() < 1e-6, "{lengths:?}");
        }
    }
}
