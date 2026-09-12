//! Shared low-level parsing support for the `gib` crates.
//!
//! See `ARCHITECTURE.md` for what belongs here.

mod parsing;
mod subslice_range;

pub use parsing::{ParseError, ParseResult};
pub use subslice_range::SubsliceRange;
