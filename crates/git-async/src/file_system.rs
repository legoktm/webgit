//! Traits and error types for interacting with files and directories
//!
//! Consumers of this library implement [`FileSystem`], [`File`] and
//! [`Directory`] to supply the filesystem operations `git-async` needs. The
//! definitions live in the `gib-fs` crate and are re-exported here; see its
//! documentation for how to implement them.

pub use gib_fs::{DirEntry, Directory, File, FileSystem, FileSystemError, Offset};

pub(crate) use gib_fs::{read_file_if_exists, search_for_files};
