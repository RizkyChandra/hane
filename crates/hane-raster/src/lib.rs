//! CPU scanline anti-aliased rasterizer -- the correctness oracle (D-002).
//!
//! Phase P1. See `docs/PLAN.md`.

mod clip;
mod fill;
mod paint;
pub mod png;

pub use clip::Clip;
pub use fill::{Color, FillRule, Pixmap};
pub use paint::{Paint, Spread, Stop};
