//! Tools, selection, hit testing, transforms and the undo command log (D-007).
//!
//! Phase P5. See `docs/PLAN.md`.

mod document;
mod group;
mod hit;
mod node;
mod pen;
mod select;
mod snap;
mod transform;
mod undo;

pub use document::{Document, Shape, document_length};
pub use group::{GroupId, Groups, Member, Reorder};
pub use hit::{MarqueeMode, hit_test, marquee};
pub use node::{EditPath, Node, NodeTarget, NodeType, Nodes, Side, node_hit};
pub use pen::Pen;
pub use select::{SelectMode, Selection};
pub use snap::{Snap, SnapTarget, Snapper};
pub use transform::{Handle, Transform, TransformBox, gesture_start};
pub use undo::{Command, UndoLog};
