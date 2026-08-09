//! A module for computing diffs between git trees
//!
//! The walk itself lives in the `gib-diff` crate and is re-exported here.
//! [`Repo::tree_diff`](crate::Repo::tree_diff) is the convenient entry point:
//! it diffs two trees through the repository's object database.
//!
//! ```
//! # use gib::{diff::TreeDiff, error::GResult, object::Tree, Repo, file_system::FileSystem};
//! async fn changed_files<F: FileSystem>(
//!     repo: &Repo<F>,
//!     left: &Tree,
//!     right: &Tree,
//! ) -> GResult<TreeDiff> {
//!     repo.tree_diff(left, right).await
//! }
//! ```

pub use gib_diff::{DiffEntry, DiffError, Path, TreeDiff};
