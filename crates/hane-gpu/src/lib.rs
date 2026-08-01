//! Tile-based GPU rasterizer. WebGL2 first, WebGPU second (D-003).
//!
//! Phase P2. See `docs/PLAN.md`.

mod render;
mod tile;

pub use render::{
    DrawData, EDGE_TEX_WIDTH, FLATTEN_TOLERANCE, MAX_CLIP_DEPTH, MAX_STOPS, Op, PaintData,
};
pub use tile::{TILE_SIZE, TileBinner};
