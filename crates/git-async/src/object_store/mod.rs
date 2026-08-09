use alloc::vec::Vec;

pub(crate) mod cache;
mod index;
pub(crate) mod lookup;
mod loose;
mod pack;

/// The type of a git object, as a plain (fieldless) enum
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ObjectType {
    #[expect(missing_docs)]
    Commit,
    #[expect(missing_docs)]
    Tag,
    #[expect(missing_docs)]
    Blob,
    #[expect(missing_docs)]
    Tree,
}

/// The size of a git object; a newtype wrapper around a [`u64`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectSize(pub(crate) u64);

#[derive(Debug)]
pub struct RawObject {
    /// The type of the object
    pub object_type: ObjectType,
    /// The raw decoded body bytes
    pub body: Vec<u8>,
}
