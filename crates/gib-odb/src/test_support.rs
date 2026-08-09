//! Opening a test repository's object database, with no facade in the way.

use crate::ObjectDb;
use futures::executor::block_on;
use gib_fs::Directory;
use gib_testkit::{TestFileSystem, TestRepo, TestRepoDirectory};

/// The default index offset cache size, matching the facade's `RepoConfig`.
const DEFAULT_OFFSET_CACHE_MAX: usize = 64 * 1024 * 1024;

pub(crate) fn objects_dir(test_repo: &TestRepo) -> TestRepoDirectory {
    block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap()
}

pub(crate) fn open_odb(test_repo: &TestRepo) -> ObjectDb<TestFileSystem> {
    open_odb_with(test_repo, DEFAULT_OFFSET_CACHE_MAX)
}

pub(crate) fn open_odb_with(
    test_repo: &TestRepo,
    index_offset_cache_max: usize,
) -> ObjectDb<TestFileSystem> {
    block_on(ObjectDb::open(
        objects_dir(test_repo),
        index_offset_cache_max,
    ))
    .unwrap()
}
