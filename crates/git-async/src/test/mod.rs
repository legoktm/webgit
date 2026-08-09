//! Dev-only support shared by this crate's tests.
//!
//! The repository fixtures themselves live in `gib-testkit`, which knows
//! nothing about [`Repo`] — opening one through the library is the facade's
//! job, so that helper lives here.

pub(crate) mod differential;

use crate::repo::{Repo, RepoConfig};
use futures::executor::block_on;
use gib_testkit::{TestFileSystem, TestRepo};

/// Open a [`TestRepo`] through the library.
pub(crate) fn open_test_repo(test_repo: &TestRepo) -> Repo<TestFileSystem> {
    block_on(RepoConfig::default().open(test_repo.git_dir())).unwrap()
}
