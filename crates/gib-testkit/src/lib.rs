//! Test-only support for building and reading real git repositories.
//!
//! See `ARCHITECTURE.md` for what belongs here.

#![deny(clippy::all)]

mod directory;
mod helpers;
mod impls;
mod repo;

pub use directory::{TestRepoDirectory, TestRepoFile};
pub use helpers::{
    get_pack_id, make_basic_repo, make_file, make_packfile_repo, make_similar_commits,
};
pub use impls::TestFileSystem;
pub use repo::{TestDirectory, TestRepo};
