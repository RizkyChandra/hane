//! OpenType parsing, shaping and text layout (D-009).
//!
//! Phase P8. See `docs/PLAN.md`.

pub mod layout;
pub mod opentype;
pub mod shape;
#[cfg(test)]
mod testfont;
