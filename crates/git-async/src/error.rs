//! A module for errors which may occur during the use of `git-async`

use crate::{file_system::FileSystemError, object::ObjectId, reference::RefName};
use alloc::vec::Vec;
use gib_commitgraph::CommitGraphError;
use gib_object::ObjectError;
use gib_pack::{PackError, PackObjectError};
use gib_ref::RefError;
use miniz_oxide::inflate::TINFLStatus;

pub use gib_object::UnexpectedObjectType;

#[expect(missing_docs)]
pub type GResult<T> = core::result::Result<T, Error>;

#[expect(missing_docs)]
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    FileSystem(FileSystemError),
    PathError(Vec<u8>),
    LooseObjectDecompressError {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        status: TINFLStatus,
    },
    PackObjectDecompressError {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        status: TINFLStatus,
    },
    FromHexError(hex::FromHexError),
    UnsupportedIndexVersion,
    CorruptIndexFile,
    CorruptCommitGraph,
    UnsupportedPackVersion,
    CorruptPackFile,
    MalformedPackedRefs,
    MalformedInfoRefs,
    MalformedInfoPacks,
    MalformedRef(RefName),
    RefNotFound(RefName),
    MalformedPackObject(ObjectId),
    MalformedObject(ObjectId),
    ObjectParseError {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        snippet: Vec<u8>,
    },
    ObjectMissingRequiredFields(ObjectId),
    MissingObject(ObjectId),
    ObjectTooLarge(ObjectId),
    UnexpectedThinPack,
    NotAnnotatedWithRepo,
    UnexpectedObjectType(UnexpectedObjectType),
    DiffCanceled,
    NotAGitRepository,
}

impl From<UnexpectedObjectType> for Error {
    fn from(value: UnexpectedObjectType) -> Self {
        Self::UnexpectedObjectType(value)
    }
}

impl From<FileSystemError> for Error {
    fn from(value: FileSystemError) -> Self {
        Self::FileSystem(value)
    }
}

impl From<hex::FromHexError> for Error {
    fn from(value: hex::FromHexError) -> Self {
        Self::FromHexError(value)
    }
}

impl From<RefError> for Error {
    fn from(value: RefError) -> Self {
        match value {
            RefError::MalformedPackedRefs => Self::MalformedPackedRefs,
            RefError::MalformedInfoRefs => Self::MalformedInfoRefs,
        }
    }
}

impl From<ObjectError> for Error {
    fn from(value: ObjectError) -> Self {
        match value {
            ObjectError::Parse { id, snippet } => Self::ObjectParseError { id, snippet },
            ObjectError::MissingFields(id) => Self::ObjectMissingRequiredFields(id),
        }
    }
}

impl From<CommitGraphError> for Error {
    fn from(value: CommitGraphError) -> Self {
        match value {
            CommitGraphError::FileSystem(e) => Self::FileSystem(e),
            CommitGraphError::Corrupt => Self::CorruptCommitGraph,
        }
    }
}

impl From<PackError> for Error {
    fn from(value: PackError) -> Self {
        match value {
            PackError::FileSystem(e) => Self::FileSystem(e),
            PackError::UnsupportedIndexVersion => Self::UnsupportedIndexVersion,
            PackError::CorruptIndexFile => Self::CorruptIndexFile,
            PackError::UnsupportedPackVersion => Self::UnsupportedPackVersion,
            PackError::CorruptPackFile => Self::CorruptPackFile,
            PackError::UnexpectedThinPack => Self::UnexpectedThinPack,
        }
    }
}

/// Attach the [`ObjectId`] a lookup asked for to the errors `gib-pack` raises
/// while reconstructing it.
///
/// The pack reader works from an offset and so cannot name the object itself,
/// but every caller here does know which ID it was resolving — and an error
/// that doesn't say which object is corrupt is much less useful.
pub(crate) fn annotate_pack_object_error(id: ObjectId) -> impl Fn(PackObjectError) -> Error {
    move |internal| match internal {
        PackObjectError::Pack(error) => error.into(),
        PackObjectError::ObjectTooLarge => Error::ObjectTooLarge(id),
        PackObjectError::MalformedObject => Error::MalformedPackObject(id),
        PackObjectError::Decompress(status) => Error::PackObjectDecompressError { id, status },
    }
}
