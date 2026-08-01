//! Paths, winding, stroke expansion and boolean operations.
//!
//! Phase P4. See `docs/PLAN.md`.

mod path;
mod stroke;

pub use path::{Path, Segment, Subpath};
pub use stroke::{Align, Cap, Join, StrokeStyle};
