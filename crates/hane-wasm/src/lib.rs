//! The browser boundary: raw wasm exports the JS shell calls into.
//!
//! # Two export styles, on purpose
//!
//! P0 said: everything exposed is numbers in and numbers out, where
//! `wasm-bindgen` buys nothing and costs a `wasm-bindgen-cli` step in the
//! release pipeline, so plain `extern "C"` -- and bring wasm-bindgen in when
//! strings, buffers or objects have to cross, P2's GL context being the likely
//! trigger.
//!
//! That trigger has fired. #17 needs a `WebGl2RenderingContext`, extension
//! names and a console line -- objects and strings, none of which cross a raw
//! `extern "C"` boundary without hand-rolling the marshalling wasm-bindgen
//! already generates. So `glctx` is `#[wasm_bindgen]` and the P0 exports below
//! stay `extern "C"`: wasm-bindgen emits both in one module, and rewriting
//! working scalar exports would buy nothing.
//!
//! The cost is real and is now paid: the build has a `wasm-bindgen` CLI step
//! and the shell loads generated glue instead of instantiating the `.wasm`
//! directly.
//!
//! # What this proves
//!
//! These are not placeholders. Each one drives real geometry across the wasm
//! boundary, so a release artifact that answers correctly has proved the whole
//! toolchain end to end: cargo, the `wasm32-unknown-unknown` target, `f64`
//! maths under wasm, and the JS loader.

pub mod bench;
pub mod glctx;
pub mod glrender;
pub mod wgpurender;

use hane_geom::{CubicBez, Point};

/// The engine version, as `major * 10000 + minor * 100 + patch`.
///
/// Lets the shell detect a stale cached `.wasm` against the JS it shipped with.
#[unsafe(no_mangle)]
pub extern "C" fn hane_version() -> u32 {
    10000
}

#[expect(clippy::too_many_arguments, reason = "a cubic is eight coordinates")]
fn cubic_from(x0: f64, y0: f64, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) -> CubicBez {
    CubicBez::new(
        Point::new(x0, y0),
        Point::new(x1, y1),
        Point::new(x2, y2),
        Point::new(x3, y3),
    )
}

/// The number of points the flattener emits for this cubic at `tolerance`.
///
/// Scalar in, scalar out, so it needs no shared-memory protocol -- which is the
/// whole point at P0. Exercises the adaptive flattening and its error bound.
// No `#[expect(too_many_arguments)]` here: clippy exempts `extern` fns from
// that lint, since an FFI signature is not the author's to shorten.
#[unsafe(no_mangle)]
pub extern "C" fn hane_flatten_count(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    x3: f64,
    y3: f64,
    tolerance: f64,
) -> u32 {
    let mut out = Vec::new();
    cubic_from(x0, y0, x1, y1, x2, y2, x3, y3).flatten(tolerance, &mut out);
    u32::try_from(out.len()).unwrap_or(u32::MAX)
}

/// The arc length of this cubic.
///
/// Exercises the Gauss-Legendre quadrature, and gives the shell a value it can
/// check against a known answer at load time.
#[unsafe(no_mangle)]
pub extern "C" fn hane_curve_length(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    x3: f64,
    y3: f64,
) -> f64 {
    cubic_from(x0, y0, x1, y1, x2, y2, x3, y3).length_at_t(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight-line cubic: its length is the endpoint distance, and
    /// flattening should not subdivide it at all.
    #[test]
    fn straight_line_cubic() {
        let c = (0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 3.0, 0.0);
        let len = hane_curve_length(c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7);
        assert!((len - 3.0).abs() < 1e-9, "length was {len}");

        let n = hane_flatten_count(c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, 0.1);
        assert_eq!(n, 2, "a straight line needs only its endpoints");
    }

    #[test]
    fn tighter_tolerance_emits_more_points() {
        let c = (0.0, 0.0, 0.0, 100.0, 100.0, 100.0, 100.0, 0.0);
        let coarse = hane_flatten_count(c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, 1.0);
        let fine = hane_flatten_count(c.0, c.1, c.2, c.3, c.4, c.5, c.6, c.7, 0.01);
        assert!(fine > coarse, "coarse={coarse} fine={fine}");
    }

    #[test]
    fn version_is_reported() {
        assert_eq!(hane_version(), 10000);
    }
}
