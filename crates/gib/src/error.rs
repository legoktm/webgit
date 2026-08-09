//! A module for errors which may occur during the use of `gib`

use crate::{file_system::FileSystemError, object::ObjectId, reference::RefName};
use gib_commitgraph::CommitGraphError;
use gib_diff::DiffError;
use gib_object::ObjectError;
use gib_odb::OdbError;
use gib_pack::PackError;
use gib_ref::RefError;
use miniz_oxide::inflate::TINFLStatus;

pub use gib_object::UnexpectedObjectType;

#[expect(missing_docs)]
pub type GResult<T> = Result<T, Error>;

/// Anything that can go wrong while reading a repository.
///
/// Each sub-crate raises its own error; this enum is where they are reunited,
/// so a consumer needs to match on one type. The `From` impls below are the
/// only place that translation happens.
#[expect(missing_docs)]
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    FileSystem(FileSystemError),
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

impl From<DiffError> for Error {
    fn from(value: DiffError) -> Self {
        match value {
            DiffError::Canceled => Self::DiffCanceled,
            DiffError::Odb(e) => e.into(),
            DiffError::Object(e) => e.into(),
            DiffError::MissingObject(id) => Self::MissingObject(id),
            DiffError::UnexpectedObjectType(e) => Self::UnexpectedObjectType(e),
        }
    }
}

impl From<OdbError> for Error {
    fn from(value: OdbError) -> Self {
        match value {
            OdbError::FileSystem(e) => Self::FileSystem(e),
            OdbError::Pack(e) => e.into(),
            OdbError::MalformedInfoPacks => Self::MalformedInfoPacks,
            OdbError::MalformedPackObject(id) => Self::MalformedPackObject(id),
            OdbError::MalformedObject(id) => Self::MalformedObject(id),
            OdbError::ObjectTooLarge(id) => Self::ObjectTooLarge(id),
            OdbError::PackObjectDecompressError { id, status } => {
                Self::PackObjectDecompressError { id, status }
            }
            OdbError::LooseObjectDecompressError { id, status } => {
                Self::LooseObjectDecompressError { id, status }
            }
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
