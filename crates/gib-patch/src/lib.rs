//! Rendering a commit as a patch, in the shape `git format-patch` writes.
//!
//! A patch is three things glued together: an mbox-style header carrying the
//! author and message, a diffstat, and the diff itself. [`diff_file`] produces
//! the last of those one file at a time — the caller loads each side's blob,
//! since only it knows how — and [`format_patch`] assembles the whole document
//! from the results.
//!
//! The diff is emitted as classified [`PatchLine`]s rather than as one string
//! so that a caller rendering the diff on screen can style each line without
//! re-parsing it, and so that a large diff is never held twice over. Joining
//! the lines back up with newlines is exactly what [`format_patch`] does.
//!
//! # What it matches, and what it doesn't
//!
//! The output is compared against `git format-patch --no-binary --no-renames`
//! in `differential.rs`, which is the standard being aimed at. Three knowing
//! departures, all of them things a browser cannot do cheaply or at all:
//!
//! * object IDs in `index` lines are abbreviated to a fixed seven digits,
//!   where git widens them until they are unambiguous in the repository;
//! * renames are not detected, so a rename is a delete and an add;
//! * binary files are reported as differing, never encoded into the patch —
//!   the same choice cgit makes.

#![deny(clippy::all)]

mod diff;
mod mail;
mod stat;

pub use diff::{DiffOptions, FileDiff, LineKind, PatchLine, Side, diff_file, is_binary};
pub use gib_xdiff::Whitespace;
pub use mail::{PatchMeta, format_patch};

#[cfg(test)]
mod differential;
