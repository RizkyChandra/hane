//! Points, vectors, affine transforms, rectangles and Bezier primitives.
//!
//! Phase P0. See `docs/PLAN.md`.
//!
//! Everything in the engine is built on this crate, so it carries the highest
//! test coverage per line in the project.
//!
//! # Precision
//!
//! All geometry is `f64`. A design tool zooms deeply enough that `f32` loses
//! visible precision in document space -- at high zoom the mantissa runs out
//! and control points visibly snap. The narrowing to `f32` happens exactly
//! once, at the GPU buffer boundary in `hane-gpu`, and nowhere else.

mod affine;
mod arc;
mod arclen;
mod bbox;
mod curve;
mod flatten;
pub mod fuzz;
mod intersect;
mod nearest;
mod path;
mod rect;
mod split;
mod vec2;

pub use affine::Affine;
pub use curve::{CubicBez, QuadBez};
pub use intersect::Intersections;
pub use path::PathEl;
pub use rect::Rect;
pub use vec2::{Point, Vec2};
