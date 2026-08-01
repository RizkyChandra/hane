//! What a fill is painted with: a solid colour, or a gradient evaluated per
//! pixel and interpolated in premultiplied space.
//!
//! # Premultiplied interpolation
//!
//! A gradient from opaque red to transparent has, at its midpoint, half the
//! red and half the alpha. Interpolating the *straight* colour instead ramps
//! the red channel down towards whatever the transparent stop happens to name
//! -- usually black -- and the result is a grey fringe across the middle of
//! every fade. Premultiplied is the only form in which "half of red, half of
//! nothing" is still red. `sable`'s D-030 reached the same conclusion.
//!
//! # Ordered dithering
//!
//! Eight bits per channel is about 0.4% per step, and a gradient spread over a
//! few hundred pixels crosses one step every few dozen of them. The eye finds
//! those straight, parallel edges immediately -- Mach banding makes them look
//! like ridges rather than steps. A Bayer matrix offsets the rounding by a
//! fixed pattern instead, so a value 0.3 of the way between two bytes lands on
//! the higher one in 30% of a tile rather than in none of it. The mean is
//! preserved and the error moves to a frequency the eye ignores.

use crate::fill::Color;
use hane_geom::Point;

/// One colour stop of a gradient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stop {
    /// Position along the gradient: 0 at its start, 1 at its end.
    ///
    /// Offsets must be given in non-decreasing order, as SVG requires. Two
    /// equal offsets are a hard edge between the colours.
    pub offset: f64,
    /// The straight (non-premultiplied) colour at that offset.
    pub color: Color,
}

/// What a gradient paints outside `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spread {
    /// The end stops extend outwards forever.
    Pad,
    /// The ramp tiles: 1.25 paints what 0.25 paints.
    Repeat,
    /// The ramp tiles mirrored: 1.25 paints what 0.75 paints.
    Reflect,
}

/// What a fill is painted with.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    /// A single straight colour.
    Solid(Color),
    /// A ramp along the axis from `start` to `end`, constant across it.
    Linear {
        /// Where offset 0 sits.
        start: Point,
        /// Where offset 1 sits.
        end: Point,
        /// The ramp, in non-decreasing offset order.
        stops: Vec<Stop>,
        /// What happens off the ends of the axis.
        spread: Spread,
    },
    /// A ramp from `focus` outwards to the circle `center`/`radius`.
    Radial {
        /// The centre of the end circle.
        center: Point,
        /// The radius of the end circle. Offset 1 lies on it.
        radius: f64,
        /// Where offset 0 sits. Pass `center` for the concentric case; SVG's
        /// `fx`/`fy` default to `cx`/`cy` for exactly this reason.
        focus: Point,
        /// The ramp, in non-decreasing offset order.
        stops: Vec<Stop>,
        /// What happens outside the circle.
        spread: Spread,
    },
}

impl Paint {
    /// The premultiplied colour this paint has at a point, on the 0..=255
    /// scale the destination bytes use.
    pub(crate) fn premul_at(&self, p: Point) -> [f64; 4] {
        match self {
            Paint::Solid(c) => premul(*c),
            Paint::Linear {
                start,
                end,
                stops,
                spread,
            } => {
                let axis = *end - *start;
                let len2 = axis.length_squared();
                // A zero-length axis paints the last stop, per SVG. Projecting
                // onto it would be 0/0.
                let t = if len2 > 0.0 {
                    (p - *start).dot(axis) / len2
                } else {
                    1.0
                };
                sample(stops, spread.map(t))
            }
            Paint::Radial {
                center,
                radius,
                focus,
                stops,
                spread,
            } => sample(stops, spread.map(radial_t(*center, *radius, *focus, p))),
        }
    }
}

impl Spread {
    /// Folds a raw gradient parameter into `[0, 1]`.
    fn map(self, t: f64) -> f64 {
        // A degenerate geometry can hand over a non-finite parameter, and every
        // arm below would turn it into a NaN that reaches the byte cast. NaN is
        // not greater than zero, so it lands on the start stop.
        if !t.is_finite() {
            return if t > 0.0 { 1.0 } else { 0.0 };
        }
        match self {
            Spread::Pad => t.clamp(0.0, 1.0),
            Spread::Repeat => t - t.floor(),
            Spread::Reflect => {
                // Fold modulo two, then mirror the far half. Written on the
                // halved parameter so that t = 1 gives exactly 1.0 and not
                // 1 - eps, which would sample the wrong side of the last stop.
                let h = t * 0.5;
                let r = (h - h.floor()) * 2.0;
                if r > 1.0 { 2.0 - r } else { r }
            }
        }
    }
}

/// The gradient parameter of `p` for an SVG radial gradient.
///
/// The ramp runs along every ray out of the focus, reaching 1 where that ray
/// meets the circle. With `focus == center` that is just distance over radius;
/// off-centre it is the ratio the quadratic below solves for.
fn radial_t(center: Point, radius: f64, focus: Point, p: Point) -> f64 {
    // Per SVG, a zero radius paints the last stop. NaN lands here too: every
    // comparison below would be false and the ray solve would return NaN.
    if radius.is_nan() || radius <= 0.0 {
        return 1.0;
    }
    // Per SVG, a focus outside the circle is pulled onto it. Just inside
    // rather than exactly on: on the boundary every ray reaches 1 at the focus
    // itself and the whole gradient collapses to a point.
    let mut f = focus;
    let off = focus - center;
    let dist = off.length();
    if dist > radius * 0.999 {
        f = center + off * (radius * 0.999 / dist);
    }
    let u = p - f;
    let a = u.length_squared();
    if a == 0.0 {
        return 0.0;
    }
    // |f + k u - center|^2 = radius^2, with b the half-coefficient. c < 0
    // because f is strictly inside, so the discriminant is positive and the
    // larger root is the one in front of the focus.
    let e = f - center;
    let b = e.dot(u);
    let c = e.length_squared() - radius * radius;
    let k = (-b + (b * b - a * c).sqrt()) / a;
    // p sits at k = 1 along its own ray, so the fraction of the way to the
    // circle is 1/k.
    1.0 / k
}

/// The premultiplied colour of a ramp at `t`, which must be in `[0, 1]`.
fn sample(stops: &[Stop], t: f64) -> [f64; 4] {
    let Some(first) = stops.first() else {
        return [0.0; 4];
    };
    let last = stops[stops.len() - 1];
    // Also the whole of the single-stop case, and what makes Pad exact at the
    // ends rather than a lerp with a zero weight.
    if t <= first.offset {
        return premul(first.color);
    }
    if t >= last.offset {
        return premul(last.color);
    }
    for w in stops.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t < b.offset {
            // Sorted input puts `t` in `[a.offset, b.offset)` here, so the
            // divisor is positive. Unsorted input is a caller error, but it
            // must not produce a NaN that propagates into the buffer.
            if b.offset <= a.offset {
                return premul(b.color);
            }
            let u = (t - a.offset) / (b.offset - a.offset);
            let (pa, pb) = (premul(a.color), premul(b.color));
            // The symmetric lerp: weights exactly 1 and 0 at both ends, so a
            // stop's own offset reproduces that stop's colour bit for bit.
            return [0, 1, 2, 3].map(|i| (1.0 - u) * pa[i] + u * pb[i]);
        }
    }
    premul(last.color)
}

/// A straight colour as premultiplied f64 on the 0..=255 scale.
pub(crate) fn premul(c: Color) -> [f64; 4] {
    let a = f64::from(c.a);
    // Multiplied before dividing: the product of two bytes is exact in f64 and
    // `r * 255 / 255` is then exactly `r`, which is what makes an opaque fill
    // land on its own colour rather than one below it.
    [
        f64::from(c.r) * a / 255.0,
        f64::from(c.g) * a / 255.0,
        f64::from(c.b) * a / 255.0,
        a,
    ]
}

/// The 8x8 ordered dither matrix, in the recursive Bayer order.
const BAYER: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// The dither offset for a pixel, in `(-0.5, 0.5)`, added before the byte is
/// rounded.
///
/// The 64 offsets of a tile are spread evenly over that interval, so a value
/// with fractional part `f` rounds up in `64 f` of its 64 pixels -- the mean
/// is the exact value, to within one part in 64 of a byte.
pub(crate) fn dither(x: u32, y: u32) -> f64 {
    (f64::from(BAYER[(y % 8) as usize][(x % 8) as usize]) + 0.5) / 64.0 - 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FillRule, Pixmap};
    use hane_geom::PathEl;

    const RED: Color = Color {
        r: 255,
        g: 0,
        b: 0,
        a: 255,
    };
    const BLUE: Color = Color {
        r: 0,
        g: 0,
        b: 255,
        a: 255,
    };
    const CLEAR: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    fn stops(pairs: &[(f64, Color)]) -> Vec<Stop> {
        pairs
            .iter()
            .map(|&(offset, color)| Stop { offset, color })
            .collect()
    }

    /// The whole of a `w` by `h` pixmap, as a path.
    fn canvas(w: u32, h: u32) -> Vec<PathEl> {
        let (w, h) = (f64::from(w), f64::from(h));
        vec![
            PathEl::MoveTo(Point::new(0.0, 0.0)),
            PathEl::LineTo(Point::new(w, 0.0)),
            PathEl::LineTo(Point::new(w, h)),
            PathEl::LineTo(Point::new(0.0, h)),
            PathEl::ClosePath,
        ]
    }

    fn painted(w: u32, h: u32, paint: &Paint) -> Pixmap {
        let mut pm = Pixmap::new(w, h);
        pm.fill_path_with(&canvas(w, h), paint, FillRule::NonZero, None);
        pm
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pm.width() + x) * 4) as usize;
        pm.data()[i..i + 4].try_into().unwrap()
    }

    fn linear(x0: f64, x1: f64, stops: Vec<Stop>, spread: Spread) -> Paint {
        Paint::Linear {
            start: Point::new(x0, 0.0),
            end: Point::new(x1, 0.0),
            stops,
            spread,
        }
    }

    // ----------------------------------------------------------- the matrix

    #[test]
    fn the_dither_matrix_is_a_permutation() {
        // A single transposed digit would bias one pixel of every tile, which
        // is invisible in an image and obvious here.
        let mut seen = [false; 64];
        for row in BAYER {
            for v in row {
                assert!(!seen[v as usize], "{v} appears twice");
                seen[v as usize] = true;
            }
        }
        assert!(seen.iter().all(|&s| s));
        // And the offsets are symmetric about zero, so dithering does not
        // brighten or darken a flat area.
        let sum: f64 = (0..8).flat_map(|y| (0..8).map(move |x| dither(x, y))).sum();
        assert!(sum.abs() < 1e-12, "{sum}");
    }

    // ---------------------------------------------------------------- linear

    #[test]
    fn a_linear_gradient_ramps_along_its_axis_and_is_flat_across_it() {
        // The paint itself, not the render: dithering deliberately makes the
        // *bytes* vary down a column, and this is the property it varies about.
        let paint = linear(0.0, 64.0, stops(&[(0.0, RED), (1.0, BLUE)]), Spread::Pad);
        for x in 0..64 {
            let p = paint.premul_at(Point::new(f64::from(x) + 0.5, 0.5));
            for y in 1..8 {
                let q = paint.premul_at(Point::new(f64::from(x) + 0.5, f64::from(y) * 9.5));
                assert_eq!(p, q, "column {x} varies");
            }
            let t = (f64::from(x) + 0.5) / 64.0;
            assert!((p[0] - 255.0 * (1.0 - t)).abs() < 1e-12, "x={x}: {p:?}");
            assert!((p[2] - 255.0 * t).abs() < 1e-12, "x={x}: {p:?}");
        }
        // A skew axis is only flat across itself to within the arithmetic, so
        // it gets a tolerance rather than equality.
        let skew = Paint::Linear {
            start: Point::new(1.0, 2.0),
            end: Point::new(9.0, 6.0),
            stops: stops(&[(0.0, RED), (1.0, BLUE)]),
            spread: Spread::Pad,
        };
        let base = skew.premul_at(Point::new(5.0, 4.0));
        // Along the perpendicular (-4, 8) direction, scaled down to stay near.
        for k in [-1.0, -0.25, 0.25, 1.0] {
            let q = skew.premul_at(Point::new(5.0 - 4.0 * k, 4.0 + 8.0 * k));
            for i in 0..4 {
                assert!((base[i] - q[i]).abs() < 1e-9, "k={k}: {base:?} vs {q:?}");
            }
        }

        // And the render does follow the ramp, to within the dither.
        let pm = painted(64, 8, &paint);
        for x in 0..64 {
            let want = 255.0 * (1.0 - (f64::from(x) + 0.5) / 64.0);
            let got = f64::from(px(&pm, x, 3)[0]);
            assert!((got - want).abs() <= 1.0, "x={x}: {got} vs {want}");
            assert_eq!(px(&pm, x, 3)[3], 255, "an opaque ramp stays opaque");
        }
    }

    #[test]
    fn a_linear_gradient_is_exact_at_its_stops() {
        // Three stops at thirds, sampled at pixel centres that land exactly on
        // them: the middle stop's colour must survive with no interpolation.
        let mid = Color {
            r: 10,
            g: 200,
            b: 30,
            a: 255,
        };
        // The axis is offset by half a pixel so that pixel centres land
        // exactly on t = 0, 1/2 and 1 rather than near them.
        let paint = linear(
            0.5,
            30.5,
            stops(&[(0.0, RED), (0.5, mid), (1.0, BLUE)]),
            Spread::Pad,
        );
        let pm = painted(32, 1, &paint);
        assert_eq!(px(&pm, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&pm, 15, 0), [10, 200, 30, 255]);
        assert_eq!(px(&pm, 30, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn a_degenerate_axis_paints_the_last_stop() {
        let paint = linear(10.0, 10.0, stops(&[(0.0, RED), (1.0, BLUE)]), Spread::Pad);
        let pm = painted(4, 1, &paint);
        assert_eq!(px(&pm, 0, 0), [0, 0, 255, 255]);
    }

    // --------------------------------------------------------------- spread

    #[test]
    fn the_three_spread_modes_behave() {
        // The ramp occupies x in [8, 16), so x = 0 is t = -0.5 and x = 20 is
        // t = 1.5. Sampled at centres 8.5.. so the values below are exact
        // eighths of the way along.
        let make = |spread| linear(8.0, 16.0, stops(&[(0.0, RED), (1.0, BLUE)]), spread);
        let (pad, repeat, reflect) = (
            make(Spread::Pad),
            make(Spread::Repeat),
            make(Spread::Reflect),
        );
        let at = |p: &Paint, x: u32| p.premul_at(Point::new(f64::from(x) + 0.5, 0.0));

        // Inside the ramp all three agree.
        for x in 8..16 {
            assert_eq!(at(&pad, x), at(&repeat, x), "x={x}");
            assert_eq!(at(&pad, x), at(&reflect, x), "x={x}");
        }
        // Pad holds the end colours.
        assert_eq!(at(&pad, 0), [255.0, 0.0, 0.0, 255.0]);
        assert_eq!(at(&pad, 23), [0.0, 0.0, 255.0, 255.0]);
        // Repeat tiles: x and x + 8 match.
        for x in 8..16 {
            assert_eq!(at(&repeat, x), at(&repeat, x + 8), "x={x}");
            assert_eq!(at(&repeat, x), at(&repeat, x - 8), "x={x}");
        }
        // Reflect mirrors about the ends: 16 + k mirrors 15 - k.
        for k in 0..8 {
            assert_eq!(at(&reflect, 16 + k), at(&reflect, 15 - k), "k={k}");
            assert_eq!(at(&reflect, 7 - k), at(&reflect, 8 + k), "k={k}");
        }
    }

    // --------------------------------------------------------------- radial

    #[test]
    fn a_concentric_radial_gradient_is_circular() {
        let paint = Paint::Radial {
            center: Point::new(32.0, 32.0),
            radius: 30.0,
            focus: Point::new(32.0, 32.0),
            stops: stops(&[(0.0, RED), (1.0, BLUE)]),
            spread: Spread::Pad,
        };
        // Equal distance, equal colour, in every direction -- which is the
        // whole of "circular". Twelve directions at four radii.
        for &r in &[3.0, 11.0, 20.0, 29.5] {
            let want = paint.premul_at(Point::new(32.0 + r, 32.0));
            for k in 1..12 {
                let a = std::f64::consts::TAU * f64::from(k) / 12.0;
                let p = Point::new(32.0 + r * a.cos(), 32.0 + r * a.sin());
                let got = paint.premul_at(p);
                for i in 0..4 {
                    assert!((got[i] - want[i]).abs() < 1e-9, "r={r} k={k}: {got:?}");
                }
            }
            // And the parameter is distance over radius: at r the colour is
            // r/30 of the way from red to blue.
            let t = r / 30.0;
            assert!((want[0] - 255.0 * (1.0 - t)).abs() < 1e-9, "{want:?}");
        }
        assert_eq!(
            paint.premul_at(Point::new(32.0, 32.0)),
            [255.0, 0.0, 0.0, 255.0]
        );

        // Padded past the circle, and the render agrees.
        let pm = painted(64, 64, &paint);
        assert_eq!(px(&pm, 63, 63), [0, 0, 255, 255]);
        assert_eq!(px(&pm, 32, 32)[0], 248); // half a pixel off centre, dithered
    }

    #[test]
    fn an_offset_focus_pushes_the_ramp_towards_it() {
        let paint = Paint::Radial {
            center: Point::new(32.0, 32.0),
            radius: 30.0,
            focus: Point::new(16.0, 32.0),
            stops: stops(&[(0.0, RED), (1.0, BLUE)]),
            spread: Spread::Pad,
        };
        // Offset 0 sits at the focus, not the centre.
        assert_eq!(
            paint.premul_at(Point::new(16.0, 32.0)),
            [255.0, 0.0, 0.0, 255.0]
        );
        assert!(
            paint.premul_at(Point::new(32.0, 32.0))[0] < 200.0,
            "the centre is no longer the start"
        );
        // The circle itself is still exactly where offset 1 lands, all the way
        // round -- the focus moves the ramp, not the end.
        for k in 0..12 {
            let a = std::f64::consts::TAU * f64::from(k) / 12.0;
            let p = Point::new(32.0 + 30.0 * a.cos(), 32.0 + 30.0 * a.sin());
            let got = paint.premul_at(p);
            assert!((got[2] - 255.0).abs() < 1e-9, "k={k}: {got:?}");
        }
        // Rays out of the focus are where it is monotone, so walk one.
        let mut prev = -1.0;
        for k in 0..40 {
            let t = paint.premul_at(Point::new(16.0 + f64::from(k), 32.0))[2];
            assert!(t >= prev, "k={k}: {t} < {prev}");
            prev = t;
        }
    }

    #[test]
    fn a_focus_outside_the_circle_is_pulled_onto_it() {
        // Per SVG. The only requirement is that it stays a gradient rather
        // than dividing by zero or painting one flat colour.
        let paint = Paint::Radial {
            center: Point::new(32.0, 32.0),
            radius: 10.0,
            focus: Point::new(200.0, 32.0),
            stops: stops(&[(0.0, RED), (1.0, BLUE)]),
            spread: Spread::Pad,
        };
        let pm = painted(64, 64, &paint);
        assert!(px(&pm, 41, 32)[0] > 100, "near the pulled-in focus");
        assert_eq!(px(&pm, 0, 0), [0, 0, 255, 255], "outside the circle");
    }

    #[test]
    fn a_zero_radius_paints_the_last_stop() {
        let paint = Paint::Radial {
            center: Point::new(2.0, 2.0),
            radius: 0.0,
            focus: Point::new(2.0, 2.0),
            stops: stops(&[(0.0, RED), (1.0, BLUE)]),
            spread: Spread::Pad,
        };
        let pm = painted(4, 4, &paint);
        assert_eq!(px(&pm, 0, 0), [0, 0, 255, 255]);
    }

    // -------------------------------------------------------- premultiplied

    #[test]
    fn a_red_to_transparent_ramp_has_no_dark_fringe() {
        // The test the whole premultiplied convention exists for. Every pixel
        // must still be pure red once divided back out, which in the stored
        // form means red == alpha exactly.
        let paint = linear(0.0, 64.0, stops(&[(0.0, RED), (1.0, CLEAR)]), Spread::Pad);
        let pm = painted(64, 1, &paint);
        for x in 0..64 {
            let p = px(&pm, x, 0);
            assert_eq!(p[0], p[3], "x={x}: {p:?} is not pure red");
            assert_eq!([p[1], p[2]], [0, 0], "x={x}: {p:?} gained a channel");
        }
        // And it really does fade: opaque at one end, gone at the other.
        assert_eq!(px(&pm, 0, 0)[3], 253);
        assert_eq!(px(&pm, 63, 0)[3], 2);
    }

    #[test]
    fn a_transparent_stop_carries_no_colour_of_its_own() {
        // Premultiplied, "transparent black" and "transparent red" are the same
        // colour, so both must give the same ramp. Straight interpolation would
        // give one a dark half and the other not.
        let a = painted(
            32,
            1,
            &linear(0.0, 32.0, stops(&[(0.0, RED), (1.0, CLEAR)]), Spread::Pad),
        );
        let clear_red = Color {
            r: 255,
            g: 0,
            b: 0,
            a: 0,
        };
        let b = painted(
            32,
            1,
            &linear(
                0.0,
                32.0,
                stops(&[(0.0, RED), (1.0, clear_red)]),
                Spread::Pad,
            ),
        );
        assert_eq!(a.data(), b.data());
    }

    // ---------------------------------------------------------------- dither

    #[test]
    fn ordered_dithering_removes_the_banding_from_a_wide_dark_gradient() {
        // Eight bytes of range over 256 pixels: undithered this is eight hard
        // steps 32 pixels wide, the textbook banding case.
        let dark = Color {
            r: 8,
            g: 8,
            b: 8,
            a: 255,
        };
        let paint = linear(
            0.0,
            256.0,
            stops(&[
                (
                    0.0,
                    Color {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 255,
                    },
                ),
                (1.0, dark),
            ]),
            Spread::Pad,
        );
        let pm = painted(256, 32, &paint);

        // Banding is a *local* error: the mean over a tile is far from the
        // exact value, and constant across the tile. Measure both the dithered
        // render and the plain rounding it replaces, over 8x8 tiles.
        let exact = |x: u32| 8.0 * (f64::from(x) + 0.5) / 256.0;
        let (mut worst_dither, mut worst_round) = (0.0f64, 0.0f64);
        for ty in 0..4 {
            for tx in 0..32 {
                let (mut got, mut want, mut rounded) = (0.0, 0.0, 0.0);
                for y in 0..8 {
                    for x in 0..8 {
                        let x = tx * 8 + x;
                        got += f64::from(px(&pm, x, ty * 8 + y)[0]);
                        want += exact(x);
                        rounded += exact(x).round();
                    }
                }
                worst_dither = worst_dither.max((got - want).abs() / 64.0);
                worst_round = worst_round.max((rounded - want).abs() / 64.0);
            }
        }
        // The undithered error is the banding: half a byte, everywhere inside
        // a band. The dithered error is bounded by the 1/64 of the matrix plus
        // the ramp's own variation across a tile.
        // The undithered error is the banding: most of a byte, and constant
        // across a whole band. The dithered one is bounded by the 1/64 the
        // matrix can resolve, and measures 0 -- a linear ramp over a whole
        // tile hits every threshold exactly once, so the mean is exact.
        assert!(
            worst_round > 0.3,
            "the reference is not banded: {worst_round}"
        );
        assert!(
            worst_dither < 1.0 / 64.0,
            "dither left {worst_dither} of band, reference {worst_round}"
        );
        // The pixels themselves are still exact bytes of the ramp, never an
        // average: dithering trades spatial noise for the mean, it does not
        // smooth anything.
        for x in 0..256 {
            let v = px(&pm, x, 0)[0];
            assert!(v <= 8, "x={x}: {v} is outside the ramp");
        }
    }

    #[test]
    fn a_solid_paint_is_never_dithered() {
        // Dithering a flat colour would be pure damage, and it is the same
        // blend as the gradient path.
        let pm = painted(
            8,
            8,
            &Paint::Solid(Color {
                r: 3,
                g: 200,
                b: 17,
                a: 129,
            }),
        );
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(px(&pm, x, y), px(&pm, 0, 0), "({x}, {y})");
            }
        }
    }

    // ------------------------------------------------------------ degenerate

    #[test]
    fn degenerate_ramps_are_harmless() {
        let paints = [
            linear(0.0, 8.0, vec![], Spread::Pad),
            linear(0.0, 8.0, stops(&[(0.5, RED)]), Spread::Repeat),
            // Two stops at the same offset: a hard edge, not a divide by zero.
            linear(
                0.0,
                8.0,
                stops(&[(0.0, RED), (0.5, RED), (0.5, BLUE), (1.0, BLUE)]),
                Spread::Reflect,
            ),
            // Offsets outside [0, 1], and a non-finite axis.
            linear(0.0, 8.0, stops(&[(-2.0, RED), (3.0, BLUE)]), Spread::Pad),
            linear(
                f64::NAN,
                8.0,
                stops(&[(0.0, RED), (1.0, BLUE)]),
                Spread::Repeat,
            ),
        ];
        for paint in &paints {
            let pm = painted(8, 4, paint);
            // The only real requirement: no panic, and no channel above alpha.
            for y in 0..4 {
                for x in 0..8 {
                    let p = px(&pm, x, y);
                    assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "{p:?}");
                }
            }
        }
        // The hard edge really is hard.
        let pm = painted(
            8,
            1,
            &linear(
                0.0,
                8.0,
                stops(&[(0.0, RED), (0.5, RED), (0.5, BLUE), (1.0, BLUE)]),
                Spread::Pad,
            ),
        );
        assert_eq!(px(&pm, 3, 0), [255, 0, 0, 255]);
        assert_eq!(px(&pm, 4, 0), [0, 0, 255, 255]);
    }
}
