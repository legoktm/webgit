use crate::directory::{TestRepoDirectory, TestRepoFile};
use gib_fs::FileSystem;

/// A [`FileSystem`] over `std::fs`, reading synchronously inside the async
/// trait methods.
pub struct TestFileSystem;
impl FileSystem for TestFileSystem {
    type File = TestRepoFile;
    type Directory = TestRepoDirectory;
}
