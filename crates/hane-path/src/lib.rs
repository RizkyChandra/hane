//! Paths, winding, stroke expansion and boolean operations.
//!
//! Phase P4. See `docs/PLAN.md`.

mod boolean;
mod path;
mod stroke;

pub use boolean::{Arrangement, BoolOp, FillRule, Preview};
pub use path::{Path, Segment, Subpath};
pub use stroke::{Align, Cap, Join, StrokeStyle};
