//! CPU scanline anti-aliased rasterizer -- the correctness oracle (D-002).
//!
//! Phase P1. See `docs/PLAN.md`.

mod clip;
pub mod corpus;
pub mod diff;
mod fill;
mod paint;
pub mod png;
mod scene;

pub use clip::Clip;
pub use fill::{BlendMode, Color, FillRule, Pixmap, fill_edges};
pub use paint::{Paint, Spread, Stop};
pub use scene::{Draw, Node, Scene};
