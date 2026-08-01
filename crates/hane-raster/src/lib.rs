//! CPU scanline anti-aliased rasterizer -- the correctness oracle (D-002).
//!
//! Phase P1. See `docs/PLAN.md`.

mod fill;
pub mod png;

pub use fill::{Color, Pixmap};
