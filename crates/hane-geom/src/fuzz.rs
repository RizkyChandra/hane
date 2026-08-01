//! A deterministic property-testing harness for geometry invariants.
//!
//! D-001 rules out `proptest`, so this is the replacement: a seeded generator
//! of points, vectors, transforms and curves, plus [`check`] to drive an
//! assertion over many of them.
//!
//! # Determinism
//!
//! A failing case must reproduce from the seed printed in the failure message,
//! on any machine, forever. That constrains the implementation more than it
//! looks:
//!
//! - The [`Rng`] state is pure integer arithmetic. No floating-point state.
//! - Every generated `f64` is built from integers and powers of two, so it is
//!   exactly representable and identical bit-for-bit everywhere. Nothing here
//!   calls `sin`, `cos` or any other libm function, whose last-bit results are
//!   platform-dependent -- which is why transforms are built from raw
//!   coefficients rather than from [`Affine::rotate`].
//! - Nothing iterates a `HashMap` or depends on thread scheduling.
//!
//! This module is a normal `pub mod`, not `#[cfg(test)]`, because `hane-path`,
//! `hane-raster` and `hane-scene` all need it and a test-only module is
//! invisible across crate boundaries.
//!
//! # Degenerate inputs
//!
//! Uniform-random coordinates almost never hit the cases that actually break
//! geometry code, so the generators emit them on purpose: coincident points,
//! zero-length vectors, collinear control points, exactly-singular and
//! near-singular transforms, and magnitudes from `2^-40` to `2^57`.
//!
//! # Example
//!
//! ```
//! use hane_geom::fuzz::{check, Rng};
//!
//! check("perp is orthogonal", 1000, Rng::vec2, |v| v.dot(v.perp()) == 0.0);
//! ```

use crate::{Affine, CubicBez, Point, QuadBez, Vec2};

/// A 64-bit linear congruential generator.
///
/// The multiplier and increment are Knuth's MMIX constants. An LCG's low bits
/// cycle with a short period, so the output is the state passed through a
/// bit-mixer rather than the state itself -- otherwise `below(8)` would be
/// nearly cyclic and the degenerate-case branches below would not be reached
/// in the proportions they look like they are.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator started from `seed`. Every seed is valid, including zero.
    #[inline]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // MurmurHash3's finalizer, applied to the state on the way out.
        let mut x = self.state;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^ (x >> 33)
    }

    /// A value in `0..n`. Panics when `n` is zero.
    ///
    /// The modulo bias is real but irrelevant: `n` is never more than a few
    /// thousand here, so the bias is under one part in `2^50`.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) has no valid result");
        self.next_u64() % n
    }

    /// A parameter in `[0, 1]`, in steps of `2^-20`, hitting both ends.
    #[inline]
    pub fn unit(&mut self) -> f64 {
        const STEPS: u64 = 1 << 20;
        self.below(STEPS + 1) as f64 / STEPS as f64
    }

    /// A coordinate, spanning many orders of magnitude.
    ///
    /// Zero and the units get a fixed share of the draws. Sampling uniformly
    /// over the exponent range would produce them essentially never, and they
    /// are exactly the values that divide by zero, normalise to NaN or make a
    /// determinant vanish.
    pub fn coord(&mut self) -> f64 {
        match self.below(8) {
            0 => 0.0,
            1 => 1.0,
            2 => -1.0,
            _ => {
                // Both parts are integer-derived and the scale is a power of
                // two, so the product is exact -- the same bits everywhere.
                let mantissa = self.below(1 << 21) as i64 - (1 << 20);
                let exp = self.below(81) as i32 - 40;
                mantissa as f64 * pow2(exp)
            }
        }
    }

    /// A displacement. Zero-length whenever both coordinates draw zero.
    pub fn vec2(&mut self) -> Vec2 {
        Vec2::new(self.coord(), self.coord())
    }

    /// A position.
    pub fn point(&mut self) -> Point {
        Point::new(self.coord(), self.coord())
    }

    /// An affine transform, including exactly-singular and near-singular ones.
    pub fn affine(&mut self) -> Affine {
        match self.below(8) {
            0 => Affine::IDENTITY,
            1 => Affine::translate(self.vec2()),
            2 => Affine::scale_non_uniform(self.coord(), self.coord()),
            3 => {
                // Exactly singular: the columns are equal, so the determinant
                // is `a*b - b*a`, which is exactly zero in floating point
                // because the two products round identically. A merely
                // proportional column would not give a bit-exact zero.
                let (a, b) = (self.coord(), self.coord());
                Affine::new([a, b, a, b, self.coord(), self.coord()])
            }
            4 => {
                // Near-singular: one column a hair off equal to the other, so
                // the determinant is tiny but non-zero and the inverse is
                // enormous. This is the reachable-from-the-UI case of a view
                // zoomed almost to nothing.
                let (a, b) = (self.coord(), self.coord());
                let eps = pow2(-(40 + self.below(20) as i32));
                Affine::new([a, b, a + eps, b, self.coord(), self.coord()])
            }
            _ => Affine::new([
                self.coord(),
                self.coord(),
                self.coord(),
                self.coord(),
                self.coord(),
                self.coord(),
            ]),
        }
    }

    /// A quadratic segment, including degenerate shapes.
    pub fn quad(&mut self) -> QuadBez {
        let p0 = self.point();
        match self.below(8) {
            0 => QuadBez::new(p0, p0, p0),
            1 => {
                // Collinear controls: a straight line drawn as a curve, which
                // breaks anything that normalises a tangent or curvature.
                let d = self.vec2();
                QuadBez::new(p0, p0 + d * self.unit(), p0 + d)
            }
            2 => QuadBez::new(p0, self.point(), p0), // closed loop
            _ => QuadBez::new(p0, self.point(), self.point()),
        }
    }

    /// A cubic segment, including degenerate shapes.
    pub fn cubic(&mut self) -> CubicBez {
        let p0 = self.point();
        match self.below(8) {
            0 => CubicBez::new(p0, p0, p0, p0),
            1 => {
                let d = self.vec2();
                let (s, t) = (self.unit(), self.unit());
                CubicBez::new(p0, p0 + d * s, p0 + d * t, p0 + d)
            }
            2 => {
                // Coincident interior controls: a cusp, where the derivative
                // vanishes mid-curve.
                let p1 = self.point();
                CubicBez::new(p0, p1, p1, self.point())
            }
            3 => CubicBez::new(p0, self.point(), self.point(), p0), // closed loop
            _ => CubicBez::new(p0, self.point(), self.point(), self.point()),
        }
    }
}

/// `2^e`, exactly. `e` must be in `-63..=63`.
fn pow2(e: i32) -> f64 {
    if e >= 0 {
        (1u64 << e) as f64
    } else {
        1.0 / (1u64 << -e) as f64
    }
}

/// Runs `property` over `iterations` generated inputs, panicking on the first
/// counterexample with the seed and the input that produced it.
///
/// Each iteration gets its own [`Rng`], seeded from `name` and the iteration
/// index. That is what makes the printed seed self-contained: feeding it to
/// `Rng::new` and calling the same generator reproduces the failing input
/// directly, with no need to replay the iterations before it.
///
/// ```
/// use hane_geom::fuzz::{check, Rng};
///
/// check("difference is antisymmetric", 500, |r| (r.point(), r.point()), |&(a, b)| {
///     b - a == -(a - b)
/// });
/// ```
pub fn check<T, G, P>(name: &str, iterations: u32, generate: G, property: P)
where
    T: core::fmt::Debug,
    G: Fn(&mut Rng) -> T,
    P: Fn(&T) -> bool,
{
    for i in 0..iterations {
        let seed = seed_for(name, i);
        let input = generate(&mut Rng::new(seed));
        assert!(property(&input), "{}", failure_message(name, seed, &input));
    }
}

/// The seed for one iteration: FNV-1a over the name, so two properties in the
/// same file explore different inputs, mixed with the iteration index.
fn seed_for(name: &str, iteration: u32) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^ u64::from(iteration).wrapping_mul(0x9e3779b97f4a7c15)
}

/// Split out from [`check`] so the failure text can be tested without
/// unwinding a panic.
fn failure_message<T: core::fmt::Debug>(name: &str, seed: u64, input: &T) -> String {
    format!(
        "property `{name}` failed\n  seed:  {seed}\n  input: {input:?}\n\
         reproduce with: hane_geom::fuzz::Rng::new({seed})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcg_output_is_pinned() {
        // Hard-coded so any change to the generator -- or any platform that
        // disagrees about integer arithmetic -- fails loudly rather than
        // silently invalidating every seed ever printed.
        let mut r = Rng::new(0);
        let got: Vec<u64> = (0..4).map(|_| r.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                4743014998196486054,
                4457842737670799537,
                8315243246868073335,
                2638869334254340181,
            ]
        );
    }

    #[test]
    fn a_seed_reproduces_its_geometry() {
        let mut a = Rng::new(20250601);
        let mut b = Rng::new(20250601);
        for _ in 0..100 {
            assert_eq!(a.cubic(), b.cubic());
            assert_eq!(a.affine(), b.affine());
        }
    }

    #[test]
    fn generated_coordinates_are_exact_powers_of_two_times_integers() {
        // If a coordinate were not exactly representable, the same seed could
        // land on different bits under a different rounding mode or FMA
        // contraction. Every value here must survive a round trip through an
        // integer scaling.
        let mut r = Rng::new(1);
        for _ in 0..10_000 {
            let c = r.coord();
            assert!(c.is_finite());
            assert_eq!(c * pow2(40) / pow2(40), c);
        }
    }

    #[test]
    fn failure_message_reproduces_the_failing_input() {
        let name = "deliberately failing";
        let seed = seed_for(name, 7);
        let input = Rng::new(seed).cubic();
        let msg = failure_message(name, seed, &input);
        assert!(msg.contains(&seed.to_string()));
        assert!(msg.contains("p0"));
        // The whole point: the seed alone regenerates the input.
        assert_eq!(Rng::new(seed).cubic(), input);
    }

    #[test]
    fn degenerate_cases_actually_occur() {
        let mut r = Rng::new(99);
        let (mut zero_vec, mut coincident, mut collinear, mut singular, mut huge) =
            (false, false, false, false, false);
        for _ in 0..5_000 {
            zero_vec |= r.vec2() == Vec2::ZERO;
            let c = r.cubic();
            coincident |= c.p0 == c.p3;
            collinear |= (c.p1 - c.p0).cross(c.p3 - c.p0) == 0.0 && c.p1 != c.p0;
            let t = r.affine();
            singular |= t.determinant() == 0.0;
            huge |= r.coord().abs() > 1e15;
        }
        assert!(zero_vec, "no zero-length vector");
        assert!(coincident, "no coincident points");
        assert!(collinear, "no collinear control points");
        assert!(singular, "no singular transform");
        assert!(huge, "no extreme magnitude");
    }

    // --- Properties of the existing modules, which are also the harness's
    // --- own proof of ergonomics.

    #[test]
    fn perp_is_exactly_orthogonal_and_a_quarter_turn() {
        check(
            "perp",
            2_000,
            Rng::vec2,
            // `x*(-y) + y*x` cancels exactly: both products round the same
            // way, so this holds at every magnitude, not just approximately.
            |&v| v.dot(v.perp()) == 0.0 && v.perp().perp().perp().perp() == v,
        );
    }

    #[test]
    fn normalize_is_never_nan() {
        check("normalize", 2_000, Rng::vec2, |&v| {
            let n = v.normalize();
            n.is_finite() && (n == Vec2::ZERO || (n.length() - 1.0).abs() < 1e-12)
        });
    }

    #[test]
    fn midpoint_lies_between_its_endpoints() {
        // Rounding is monotone, so the average of two coordinates can never
        // fall outside them -- no tolerance needed.
        check(
            "midpoint",
            2_000,
            |r| (r.point(), r.point()),
            |&(a, b)| {
                let m = a.midpoint(b);
                m.x >= a.x.min(b.x)
                    && m.x <= a.x.max(b.x)
                    && m.y >= a.y.min(b.y)
                    && m.y <= a.y.max(b.y)
            },
        );
    }

    #[test]
    fn identity_composes_exactly() {
        check("identity composition", 2_000, Rng::affine, |&t| {
            t * Affine::IDENTITY == t && Affine::IDENTITY * t == t
        });
    }

    #[test]
    fn inverse_exists_exactly_when_the_determinant_is_usable() {
        check("inverse existence", 4_000, Rng::affine, |&t| {
            let det = t.determinant();
            t.inverse().is_some() == (det != 0.0 && det.is_finite())
        });
    }

    #[test]
    fn inverse_round_trips_when_well_conditioned() {
        check(
            "inverse round trip",
            4_000,
            |r| (r.affine(), r.point()),
            |&(t, p)| {
                let Some(inv) = t.inverse() else {
                    return true;
                };
                // sigma_max / sigma_min, which bounds how much the round trip
                // can amplify rounding error. Near-singular transforms are
                // generated on purpose and are vacuously excluded here: the
                // property under test is accuracy, not conditioning.
                let cond = t.max_scale() * t.max_scale() / t.determinant().abs();
                let usable = cond.is_finite() && cond < 1e6 && inv.is_finite();
                if !usable {
                    return true;
                }
                // The intermediate `t * p` carries the translation, so its
                // magnitude -- not the point's -- sets the absolute precision
                // the round trip can recover.
                let reach = p.to_vec2().length() + t.translation().length() / t.max_scale();
                (inv * (t * p) - p).length() <= 1e-8 * (1.0 + reach)
            },
        );
    }

    #[test]
    fn curve_endpoints_are_bit_exact() {
        // The one thing `eval` guarantees exactly; interior values are not
        // bit-reproducible and get tolerances instead.
        check("cubic endpoints", 2_000, Rng::cubic, |&c| {
            c.eval(0.0) == c.start() && c.eval(1.0) == c.end()
        });
        check("quad endpoints", 2_000, Rng::quad, |&q| {
            q.eval(0.0) == q.start() && q.eval(1.0) == q.end()
        });
    }

    #[test]
    fn a_cubic_stays_inside_its_control_polygon() {
        check(
            "cubic hull",
            4_000,
            |r| (r.cubic(), r.unit()),
            |&(c, t)| {
                let p = c.eval(t);
                let xs = [c.p0.x, c.p1.x, c.p2.x, c.p3.x];
                let ys = [c.p0.y, c.p1.y, c.p2.y, c.p3.y];
                let scale = xs
                    .iter()
                    .chain(ys.iter())
                    .fold(0.0f64, |m, v| m.max(v.abs()));
                // The Bernstein basis is non-negative and sums to one in exact
                // arithmetic, so the curve is inside the box. In f64 the basis
                // sums to one only to a few ulps, hence the slack.
                let slack = 64.0 * f64::EPSILON * scale;
                let (lo_x, hi_x) = bounds(&xs);
                let (lo_y, hi_y) = bounds(&ys);
                p.x >= lo_x - slack
                    && p.x <= hi_x + slack
                    && p.y >= lo_y - slack
                    && p.y <= hi_y + slack
            },
        );
    }

    #[test]
    fn quad_to_cubic_agrees_with_the_quad() {
        check(
            "quad to cubic",
            4_000,
            |r| (r.quad(), r.unit()),
            |&(q, t)| {
                let c = q.to_cubic();
                let scale =
                    q.p0.to_vec2()
                        .length()
                        .max(q.p1.to_vec2().length())
                        .max(q.p2.to_vec2().length());
                (q.eval(t) - c.eval(t)).length() <= 1e-12 * (1.0 + scale)
            },
        );
    }

    fn bounds(vs: &[f64; 4]) -> (f64, f64) {
        vs.iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            })
    }
}
