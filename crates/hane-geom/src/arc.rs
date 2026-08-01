//! SVG elliptical arcs, converted to cubics.
//!
//! `PathEl` has no arc variant on purpose: converting here, at the parse edge,
//! is what keeps every consumer downstream -- stroking, flattening, filling,
//! hit testing -- handling three segment kinds instead of four. The conversion
//! is exact in endpoints and direction; only the curve between them is an
//! approximation, and a good one.
//!
//! # The spec's edge cases are the common cases
//!
//! SVG 1.1 F.6.5 is the endpoint-to-centre parameterisation, and F.6.6 the
//! correction that follows it. F.6.6 is not a nicety: radii too small to reach
//! from one endpoint to the other are *scaled up* until they just span the
//! chord, never rejected, and real files are full of them -- rounding a radius
//! for output, or scaling a shape without scaling its arcs, produces exactly
//! that. Illustrator and Inkscape both emit it.
//!
//! # Why the sweep splits into 90-degree pieces
//!
//! A cubic matched to a circular arc at both endpoints, both end tangents and
//! the sweep midpoint has a maximum radial error of about `2.7e-4 * r` over a
//! quarter turn, and that error grows like the sixth power of the sweep angle:
//! halving the piece cuts it by 64. A quarter turn is where the error stops
//! mattering at any sane zoom and where the segment count stops at four for a
//! full circle -- the same choice every browser and every serious 2D library
//! makes.

use crate::{Affine, PathEl, Point, Vec2};
use core::f64::consts::{FRAC_PI_2, TAU};

/// A sweep of at most `2*pi` cut into pieces of at most `pi/2`.
const MAX_SEGMENTS: usize = 4;

impl PathEl {
    /// The cubics for one SVG `A rx ry x-rotation large-arc sweep x y`
    /// command, continuing from `from`.
    ///
    /// `x_rotation` is in radians. The iterator yields at most four elements,
    /// the last of which ends exactly at `to`; it yields a single
    /// [`LineTo`](PathEl::LineTo) when either radius is zero and nothing at all
    /// when the endpoints coincide, both per F.6.2.
    pub fn arc(
        from: Point,
        radii: Vec2,
        x_rotation: f64,
        large_arc: bool,
        sweep: bool,
        to: Point,
    ) -> impl Iterator<Item = Self> + Clone {
        // Filled with the degenerate answer rather than junk, so a slot that
        // somehow escapes the `take` is still a valid path element.
        let mut els = [Self::LineTo(to); MAX_SEGMENTS];
        let n = arc_segments(&mut els, from, radii, x_rotation, large_arc, sweep, to);
        els.into_iter().take(n)
    }
}

/// Writes the elements of the arc into `out` and returns how many.
fn arc_segments(
    out: &mut [PathEl; MAX_SEGMENTS],
    from: Point,
    radii: Vec2,
    x_rotation: f64,
    large_arc: bool,
    sweep: bool,
    to: Point,
) -> usize {
    // F.6.2: coincident endpoints omit the arc entirely -- not a line, nothing.
    if from == to {
        return 0;
    }
    // F.6.6 step 1: the sign of a radius carries no information.
    let (mut rx, mut ry) = (radii.x.abs(), radii.y.abs());
    // F.6.2: a zero radius is a straight line. Written negated so a NaN radius
    // takes this branch too instead of propagating into the trigonometry.
    if !(rx > 0.0 && ry > 0.0) {
        out[0] = PathEl::LineTo(to);
        return 1;
    }

    let (sin_phi, cos_phi) = x_rotation.sin_cos();
    let rot = Affine::new([cos_phi, sin_phi, -sin_phi, cos_phi, 0.0, 0.0]);
    // F.6.5.1: half the endpoint difference, rotated *into* the ellipse's frame
    // -- so by -phi, which is `rot` transposed.
    let h = (from - to) * 0.5;
    let p = Vec2::new(
        cos_phi * h.x + sin_phi * h.y,
        -sin_phi * h.x + cos_phi * h.y,
    );

    // F.6.6 steps 2-3: radii too small to span the chord are scaled up to the
    // point where they just reach, which is the one solution the endpoints
    // admit. Rejecting the arc would drop geometry that every other renderer
    // draws.
    let lambda = (p.x * p.x) / (rx * rx) + (p.y * p.y) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    // F.6.5.2: the centre, still in the ellipse's frame.
    let (rx2, ry2) = (rx * rx, ry * ry);
    let den = rx2 * p.y * p.y + ry2 * p.x * p.x;
    // Exactly zero after the correction above when the radii only just span the
    // chord; rounding can put it a hair below, and a negative sqrt is a NaN
    // centre.
    let num = (rx2 * ry2 - den).max(0.0);
    let sign = if large_arc == sweep { -1.0 } else { 1.0 };
    let cp = sign * (num / den).sqrt() * Vec2::new(rx * p.y / ry, -ry * p.x / rx);
    // F.6.5.3: and back out of it.
    let center = from.midpoint(to) + rot * cp;

    // F.6.5.5-6: the start angle and the signed sweep, measured on the circle
    // the ellipse is a scaling of.
    let start = Vec2::new((p.x - cp.x) / rx, (p.y - cp.y) / ry);
    let end = Vec2::new((-p.x - cp.x) / rx, (-p.y - cp.y) / ry);
    let theta = start.angle();
    let mut delta = end.angle() - theta;
    // Two co-terminal sweeps reach the same point; the flag picks which.
    if sweep && delta < 0.0 {
        delta += TAU;
    } else if !sweep && delta > 0.0 {
        delta -= TAU;
    }

    // Coordinates and radii near f64's limits overflow `den` or `rx2 * ry2`
    // above; one check here covers every such path rather than a guard per
    // operation. A chord is the least wrong thing to draw.
    if !(center.is_finite() && theta.is_finite() && delta.is_finite()) {
        out[0] = PathEl::LineTo(to);
        return 1;
    }

    // Unit circle to this ellipse. Every segment below is built on the circle
    // and pushed through here, so the elliptical case needs no separate maths.
    let m = Affine::translate(center.to_vec2()) * rot * Affine::scale_non_uniform(rx, ry);

    let n = ((delta.abs() / FRAC_PI_2).ceil() as usize).clamp(1, MAX_SEGMENTS);
    let step = delta / n as f64;
    // The control arm length that puts the cubic on the circle at both ends and
    // at the midpoint of the sweep, with matching tangents: 4/3 tan(step/4).
    let k = 4.0 / 3.0 * (0.25 * step).tan();
    for (i, el) in out.iter_mut().enumerate().take(n) {
        let t0 = theta + step * i as f64;
        let t1 = t0 + step;
        let (s0, c0) = t0.sin_cos();
        let (s1, c1) = t1.sin_cos();
        let p0 = Point::new(c0, s0);
        let p3 = Point::new(c1, s1);
        // The last end point is `to` by construction but not bit-exactly, and a
        // fill sees a gap of one ulp as an open subpath.
        let end = if i + 1 == n { to } else { m * p3 };
        *el = PathEl::CurveTo(
            m * (p0 + Vec2::new(-s0, c0) * k),
            m * (p3 - Vec2::new(-s1, c1) * k),
            end,
        );
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CubicBez;
    use crate::fuzz::check;

    /// The emitted elements as cubics, so they can be sampled.
    fn cubics(from: Point, els: &[PathEl]) -> Vec<CubicBez> {
        let mut p = from;
        els.iter()
            .map(|el| match *el {
                PathEl::CurveTo(c0, c1, e) => {
                    let c = CubicBez::new(p, c0, c1, e);
                    p = e;
                    c
                }
                other => panic!("expected a cubic, got {other:?}"),
            })
            .collect()
    }

    fn arc(from: Point, radii: Vec2, phi: f64, large: bool, sweep: bool, to: Point) -> Vec<PathEl> {
        PathEl::arc(from, radii, phi, large, sweep, to).collect()
    }

    /// Largest distance from the true circle over the whole chain.
    fn circle_deviation(from: Point, els: &[PathEl], center: Point, r: f64) -> f64 {
        let mut worst: f64 = 0.0;
        for c in cubics(from, els) {
            for i in 0..=200 {
                let d = c.eval(i as f64 / 200.0).distance(center) - r;
                worst = worst.max(d.abs());
            }
        }
        worst
    }

    #[test]
    fn zero_radius_degenerates_to_a_line() {
        let (a, b) = (Point::new(1.0, 2.0), Point::new(9.0, 4.0));
        for radii in [Vec2::new(0.0, 5.0), Vec2::new(5.0, 0.0), Vec2::ZERO] {
            assert_eq!(arc(a, radii, 0.4, true, true, b), [PathEl::LineTo(b)]);
        }
    }

    #[test]
    fn coincident_endpoints_emit_nothing() {
        let a = Point::new(3.0, 4.0);
        assert!(arc(a, Vec2::new(5.0, 5.0), 0.0, true, true, a).is_empty());
    }

    #[test]
    fn negative_radii_are_taken_as_positive() {
        let (a, b) = (Point::new(100.0, 0.0), Point::new(0.0, 100.0));
        let r = Vec2::new(100.0, 100.0);
        assert_eq!(
            arc(a, Vec2::new(-100.0, -100.0), 0.0, false, true, b),
            arc(a, r, 0.0, false, true, b)
        );
    }

    /// The two endpoints and radius 100 admit exactly two centres, `(0, 0)` and
    /// `(100, 100)`, and each centre admits a short and a long way round. The
    /// four flag combinations must select the four distinct arcs -- getting
    /// these backwards is the classic arc bug, so each is pinned by the point
    /// halfway along its own sweep, which no other combination passes through.
    #[test]
    fn all_four_flag_combinations_pick_the_right_arc() {
        let (a, b) = (Point::new(100.0, 0.0), Point::new(0.0, 100.0));
        let r = Vec2::new(100.0, 100.0);
        let d = 100.0 * core::f64::consts::FRAC_1_SQRT_2;
        let cases = [
            // (large_arc, sweep, centre, quarter- or three-quarter-way point)
            (false, true, Point::ORIGIN, Point::new(d, d)),
            (
                false,
                false,
                Point::new(100.0, 100.0),
                Point::new(100.0 - d, 100.0 - d),
            ),
            (
                true,
                true,
                Point::new(100.0, 100.0),
                Point::new(100.0 + d, 100.0 + d),
            ),
            (true, false, Point::ORIGIN, Point::new(-d, -d)),
        ];
        for (large, sweep, center, mid) in cases {
            let els = arc(a, r, 0.0, large, sweep, b);
            assert_eq!(els.len(), if large { 3 } else { 1 }, "{large} {sweep}");
            assert_eq!(els.last().unwrap().end_point(), Some(b));
            assert!(
                circle_deviation(a, &els, center, 100.0) < 2.8e-4 * 100.0,
                "{large} {sweep} left the circle about {center:?}"
            );
            // Equal steps put the sweep midpoint at t = 0.5 of the middle
            // segment, which is one of the points the cubic matches exactly.
            let cs = cubics(a, &els);
            let got = cs[cs.len() / 2].eval(0.5);
            assert!(
                got.distance(mid) < 1e-9,
                "{large} {sweep}: {got:?} != {mid:?}"
            );
        }
    }

    /// F.6.6: radii of 10 cannot span a chord of 100, so both scale by 5 and
    /// the arc becomes the semicircle of radius 50 -- not an error, not a line.
    #[test]
    fn out_of_range_radii_scale_up_per_f6_6() {
        let (a, b) = (Point::ORIGIN, Point::new(100.0, 0.0));
        let els = arc(a, Vec2::new(10.0, 10.0), 0.0, false, true, b);
        assert_eq!(els.len(), 2);
        assert!(circle_deviation(a, &els, Point::new(50.0, 0.0), 50.0) < 2.8e-4 * 50.0);
        // Sweep flag set is the increasing-angle direction, which in SVG's
        // y-down space leaves the chord on the -y side.
        assert!(
            els[0]
                .end_point()
                .unwrap()
                .distance(Point::new(50.0, -50.0))
                < 1e-9
        );
        // Both flags mirror it, and the scaling is unchanged by them.
        let flipped = arc(a, Vec2::new(10.0, 10.0), 0.0, false, false, b);
        assert!(
            flipped[0]
                .end_point()
                .unwrap()
                .distance(Point::new(50.0, 50.0))
                < 1e-9
        );
    }

    /// Anisotropic radii scaled by F.6.6 keep their ratio, so the result is the
    /// unique ellipse of that shape through both endpoints.
    #[test]
    fn out_of_range_radii_keep_their_aspect_ratio() {
        let (a, b) = (Point::ORIGIN, Point::new(100.0, 0.0));
        let els = arc(a, Vec2::new(2.0, 1.0), 0.0, false, true, b);
        // rx scales to 50, so ry to 25: with a half-turn split in two, the
        // join between the halves is the top of the ellipse, 25 away.
        let mid = els[0].end_point().unwrap();
        assert!(mid.distance(Point::new(50.0, -25.0)) < 1e-9, "{mid:?}");
    }

    /// A rotated, eccentric ellipse sampled densely: mapping each sample back
    /// through the ellipse's own transform turns "distance from the ellipse"
    /// into "distance from the unit circle", where it is a single subtraction.
    #[test]
    fn deviation_from_the_true_ellipse_stays_under_tolerance() {
        let (rx, ry, phi) = (200.0, 50.0, 0.7);
        let center = Point::new(-30.0, 17.0);
        let m = Affine::translate(center.to_vec2())
            * Affine::rotate(phi)
            * Affine::scale_non_uniform(rx, ry);
        let inv = m.inverse().unwrap();
        // Every sweep from a sixth of a turn to nearly a full one, both ways.
        for steps in 1..=11 {
            let sweep_angle = steps as f64 * TAU / 12.0;
            for dir in [1.0, -1.0] {
                let t0 = 0.3;
                let t1 = t0 + dir * sweep_angle;
                let (from, to) = (
                    m * Point::new(t0.cos(), t0.sin()),
                    m * Point::new(t1.cos(), t1.sin()),
                );
                let els = arc(
                    from,
                    Vec2::new(rx, ry),
                    phi,
                    sweep_angle > core::f64::consts::PI,
                    dir > 0.0,
                    to,
                );
                let mut worst: f64 = 0.0;
                for c in cubics(from, &els) {
                    for i in 0..=200 {
                        let u = inv * c.eval(i as f64 / 200.0);
                        worst = worst.max((u.to_vec2().length() - 1.0).abs());
                    }
                }
                // Radial error on the unit circle; the largest radius turns it
                // into a distance bound in document space.
                assert!(
                    worst * rx < 2.8e-4 * rx,
                    "{sweep_angle} {dir}: {}",
                    worst * rx
                );
            }
        }
    }

    /// The half-turn-each spelling of a circle, which is how every SVG exporter
    /// writes one, has to close.
    #[test]
    fn a_full_circle_as_two_arcs_round_trips() {
        let (center, r) = (Point::new(10.0, 20.0), 7.0);
        let (a, b) = (Point::new(17.0, 20.0), Point::new(3.0, 20.0));
        let radii = Vec2::new(r, r);
        let mut els = arc(a, radii, 0.0, false, true, b);
        els.extend(arc(b, radii, 0.0, false, true, a));
        assert_eq!(els.len(), 4);
        assert_eq!(els.last().unwrap().end_point(), Some(a));
        assert!(circle_deviation(a, &els, center, r) < 2.8e-4 * r);
        // Both halves are present, not the same one twice.
        let cs = cubics(a, &els);
        assert!(cs[0].eval(0.5).y > center.y && cs[2].eval(0.5).y < center.y);
    }

    /// Sub-ulp gaps between an arc and what follows it read as an open subpath
    /// when filled, so the end point is exact rather than merely close, for
    /// every input including the degenerate ones.
    #[test]
    fn the_chain_always_ends_exactly_at_the_endpoint() {
        check(
            "arc ends exactly where it was told to",
            2000,
            |r| {
                (
                    r.point(),
                    r.point(),
                    r.vec2(),
                    r.unit(),
                    r.below(2) == 1,
                    r.below(2) == 1,
                )
            },
            |&(from, to, radii, phi, large, sweep)| {
                let els = arc(from, radii, phi, large, sweep, to);
                els.len() <= MAX_SEGMENTS
                    && match els.last() {
                        Some(el) => el.end_point() == Some(to),
                        None => from == to,
                    }
            },
        );
    }

    /// The same arc written from either end is the same arc: reversing the
    /// endpoints and flipping the sweep flag traces it backwards.
    #[test]
    fn reversing_the_endpoints_and_sweep_traces_the_same_arc() {
        let (a, b) = (Point::new(-4.0, 11.0), Point::new(23.0, 6.0));
        let radii = Vec2::new(30.0, 12.0);
        for large in [false, true] {
            for sweep in [false, true] {
                let f = cubics(a, &arc(a, radii, 0.9, large, sweep, b));
                let r = cubics(b, &arc(b, radii, 0.9, large, !sweep, a));
                assert_eq!(f.len(), r.len());
                for (i, c) in f.iter().enumerate() {
                    let mirror = r[r.len() - 1 - i];
                    for j in 0..=20 {
                        let t = j as f64 / 20.0;
                        assert!(c.eval(t).distance(mirror.eval(1.0 - t)) < 1e-9);
                    }
                }
            }
        }
    }
}
