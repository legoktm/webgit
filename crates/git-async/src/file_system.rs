//! Traits and error types for interacting with files and directories
//!
//! Consumers of this library implement [`FileSystem`], [`File`] and
//! [`Directory`] to supply the filesystem operations `git-async` needs. The
//! definitions live in the `gib-fs` crate and are re-exported here; see its
//! documentation for how to implement them.

pub use gib_fs::{DirEntry, Directory, File, FileSystem, FileSystemError, Offset};

pub(crate) use gib_fs::{read_file_if_exists, search_for_files};

#[cfg(test)]
mod tests {
    use crate::test::{directory::TestRepoDirectory, repo::TestDirectory};

    use super::*;
    use futures::executor::block_on;
    use gib_fs::Path;
    use std::{
        fs::{OpenOptions, create_dir},
        io::{self, Write},
        path::PathBuf,
        sync::Arc,
    };
    use tempfile::TempDir;

    #[test]
    fn test_search_for_files() {
        fn touch(path: impl AsRef<std::path::Path>) -> io::Result<()> {
            let mut f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)?;
            f.flush()?;
            Ok(())
        }
        let dir = TempDir::new().unwrap();
        touch(dir.path().join("file-a")).unwrap();
        touch(dir.path().join("file-b")).unwrap();
        create_dir(dir.path().join("dir-a")).unwrap();
        touch(dir.path().join("dir-a").join("file-c")).unwrap();
        create_dir(dir.path().join("dir-a").join("dir-b")).unwrap();
        touch(dir.path().join("dir-a").join("dir-b").join("file-d")).unwrap();
        let mut expected: Vec<Path> = vec![
            vec![b"file-a".to_vec()],
            vec![b"file-b".to_vec()],
            vec![b"dir-a".to_vec(), b"file-c".to_vec()],
            vec![b"dir-a".to_vec(), b"dir-b".to_vec(), b"file-d".to_vec()],
        ];
        expected.sort();
        let dir = TestRepoDirectory {
            root: TestDirectory::Temp(Arc::new(dir)),
            sub_path: PathBuf::new(),
        };
        let mut paths = block_on(search_for_files(&dir)).unwrap();
        paths.sort();
        assert_eq!(paths, expected);
    }
}
