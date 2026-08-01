//! Scene graph, spatial index, tile cache and culling (D-004, D-006).
//!
//! Phase P3. See `docs/PLAN.md`.

mod arena;
mod quadtree;
mod view;

pub use arena::{Arena, NodeId};
pub use quadtree::Quadtree;
pub use view::View;
