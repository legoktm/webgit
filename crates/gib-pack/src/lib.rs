//! Reading objects out of git packfiles and their `.idx` indexes.
//!
//! This crate does perform IO — it reads lazily through the `gib-fs` traits so
//! only the touched pages of a `.idx` or `.pack` are fetched — but it knows
//! nothing about repository layout or pack discovery. Callers open the index
//! and pack files and hand them over as an [`IndexedPackFile`].

#![deny(clippy::all)]

mod index;
mod pack;

pub use index::{
    FanoutTable, ShortOffsetTable, find_object_in_pack_index, find_prefix_in_pack_index,
};
pub use pack::{
    PackObject, form_deltified_chain, reconstruct_deltified_object_from_chain,
    validate_packfile_version,
};

use gib_fs::{CachingPageReader, FileSystemError};
use miniz_oxide::inflate::TINFLStatus;

/// Something went wrong reading a pack or its index, in a way that names no
/// particular object.
#[derive(Debug)]
pub enum PackError {
    #[expect(missing_docs)]
    FileSystem(FileSystemError),
    /// The `.idx` file is not a version 2 pack index.
    UnsupportedIndexVersion,
    /// The `.idx` file is shorter or otherwise less well-formed than its own
    /// header claims.
    CorruptIndexFile,
    /// The `.pack` file is not a version 2 packfile.
    UnsupportedPackVersion,
    /// The `.pack` file's bytes do not form a well-shaped object header.
    CorruptPackFile,
    /// A ref-delta object named a base object that is not in this pack. Thin
    /// packs only occur in transit, so an on-disk one is corrupt.
    UnexpectedThinPack,
}

impl From<FileSystemError> for PackError {
    fn from(value: FileSystemError) -> Self {
        Self::FileSystem(value)
    }
}

/// Something went wrong reconstructing one object from a pack.
///
/// The variants other than [`Pack`](Self::Pack) name no object, because the
/// code that hit them was working from a pack offset. The caller knows which
/// [`ObjectId`](gib_hash::ObjectId) it asked for and is expected to attach it.
#[derive(Debug)]
pub enum PackObjectError {
    /// An error that is not specific to one object.
    Pack(PackError),
    /// The object's header names a type this crate does not know.
    MalformedObject,
    /// The object is larger than this platform's `usize` can address.
    ObjectTooLarge,
    /// The object's compressed body did not inflate.
    Decompress(TINFLStatus),
}

impl From<PackError> for PackObjectError {
    fn from(value: PackError) -> Self {
        Self::Pack(value)
    }
}

impl From<FileSystemError> for PackObjectError {
    fn from(value: FileSystemError) -> Self {
        Self::Pack(PackError::FileSystem(value))
    }
}

type PackResult<T> = Result<T, PackError>;
type ObjectResult<T> = Result<T, PackObjectError>;

/// A pack and its index, both open and ready to read.
///
/// The tables are borrowed because a caller typically keeps them cached across
/// lookups, while the page readers are per-lookup.
pub struct IndexedPackFile<'f, F> {
    /// A reader over the `.idx` file.
    pub index: CachingPageReader<F>,
    /// The index's fanout table.
    pub fanout: &'f FanoutTable,
    /// The index's short offset table, when the caller chose to cache it.
    pub offsets: Option<&'f ShortOffsetTable>,
    /// A reader over the `.pack` file.
    pub pack: CachingPageReader<F>,
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod differential;
