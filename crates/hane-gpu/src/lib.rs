//! Tile-based GPU rasterizer. WebGL2 first, WebGPU second (D-003).
//!
//! Phase P2. See `docs/PLAN.md`.

mod tile;

pub use tile::{TILE_SIZE, TileBinner};
