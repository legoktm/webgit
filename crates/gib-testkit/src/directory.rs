use crate::repo::TestDirectory;
use gib_fs::{DirEntry, Directory, File, FileSystemError, Offset};
use std::{
    cmp::min,
    fs,
    io::{self, Read, Seek},
    path::PathBuf,
};

/// A directory handle rooted at a [`TestDirectory`], addressed by relative
/// path.
#[derive(Debug, Clone)]
pub struct TestRepoDirectory {
    /// The repository (or other) root this handle is relative to.
    pub root: TestDirectory,
    /// The path of this directory below `root`.
    pub sub_path: PathBuf,
}

/// A lazily-opened file: like the HTTP-backed filesystem used in production,
/// the underlying file is not opened until the first read, so a missing file
/// surfaces as [`FileSystemError::NotFound`] from `read_*` rather than from
/// `open_file`. This keeps the native test suite exercising the same code
/// paths as the lazy production implementation.
#[derive(Debug)]
pub struct TestRepoFile {
    /// The absolute path of the file.
    pub path: PathBuf,
    /// Kept alive so a `TempDir` root outlives handles into it.
    pub _dir: TestDirectory,
}

fn open_for_read(path: &PathBuf) -> Result<fs::File, FileSystemError> {
    match fs::OpenOptions::new().read(true).open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(FileSystemError::NotFound(Box::new(e)))
        }
        Err(e) => Err(FileSystemError::Other(Box::new(e))),
    }
}

impl Directory<TestRepoFile> for TestRepoDirectory {
    async fn open_subdir(&self, name: &[u8]) -> Result<Self, FileSystemError> {
        let new_sub_path = self.sub_path.join(str::from_utf8(name).unwrap());
        if let Err(e) = fs::metadata(self.root.path().join(&new_sub_path))
            && e.kind() == io::ErrorKind::NotFound
        {
            return Err(FileSystemError::NotFound(Box::new(e)));
        }
        Ok(Self {
            root: self.root.clone(),
            sub_path: new_sub_path,
        })
    }

    async fn list_dir(&self) -> Result<Vec<DirEntry>, FileSystemError> {
        let dir = fs::read_dir(self.root.path().join(&self.sub_path)).unwrap();
        let entries = dir
            .map_while(|entry| {
                if let Ok(entry) = entry {
                    let file_type = entry.file_type().unwrap();
                    let file_name = entry.file_name().into_encoded_bytes();
                    if file_type.is_dir() {
                        Some(DirEntry::Directory(file_name))
                    } else if file_type.is_file() {
                        Some(DirEntry::File(file_name))
                    } else {
                        panic!("symlinks not supported in tests");
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        Ok(entries)
    }

    async fn open_file(&self, name: &[u8]) -> Result<TestRepoFile, FileSystemError> {
        // Lazy: record the path but don't open it. Absence is reported by the
        // first read, mirroring the HTTP-backed filesystem.
        Ok(TestRepoFile {
            path: self
                .root
                .path()
                .join(&self.sub_path)
                .join(str::from_utf8(name).unwrap()),
            _dir: self.root.clone(),
        })
    }
}

impl File for TestRepoFile {
    async fn read_all(&mut self) -> Result<Vec<u8>, FileSystemError> {
        let mut file = open_for_read(&self.path)?;
        let mut out = vec![];
        file.read_to_end(&mut out).unwrap();
        Ok(out)
    }

    async fn read_segment(
        &mut self,
        offset: Offset,
        dest: &mut [u8],
    ) -> Result<usize, FileSystemError> {
        let mut file = open_for_read(&self.path)?;
        let metadata = file.metadata().unwrap();
        // Saturate so a read starting at or past EOF yields zero bytes rather
        // than panicking, matching the HTTP filesystem's range behaviour.
        let available_len = metadata.len().saturating_sub(offset.0);
        let read_len = min(usize::try_from(available_len).unwrap(), dest.len());
        file.seek(io::SeekFrom::Start(offset.0)).unwrap();
        file.read_exact(&mut dest[0..read_len]).unwrap();
        Ok(read_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::{fs::OpenOptions, io::Write, sync::Arc};
    use tempfile::tempdir;

    #[test]
    fn test_seek_offset() {
        let mut test_contents = vec![0u8; 1024];
        for (idx, item) in test_contents.iter_mut().enumerate() {
            *item = (idx % 256).try_into().unwrap();
        }
        let dir = tempdir().unwrap();
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.path().join("a-file"))
            .unwrap();
        f.write_all(&test_contents).unwrap();
        let dir = TestRepoDirectory {
            root: TestDirectory::Temp(Arc::new(dir)),
            sub_path: PathBuf::new(),
        };
        let offset = Offset(700);
        let length: usize = 32;
        let mut file = block_on(dir.open_file(b"a-file")).unwrap();
        let mut content = vec![0u8; length];
        block_on(file.read_segment(offset, &mut content)).unwrap();
        assert_eq!(content.len(), length);
        assert_eq!(
            &content,
            &test_contents[(offset.0 as usize)..((offset.0 as usize) + length)]
        );
    }
}
