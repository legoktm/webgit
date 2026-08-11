use crate::ObjectId;

/// A blob object
///
/// Represents arbitrary data, e.g. the contents of a file
#[derive(Debug, Clone)]
pub struct Blob {
    /// The [`ObjectId`] of the blob object
    id: ObjectId,

    /// The data that the blob contains
    data: Vec<u8>,
}

impl Blob {
    /// The [`ObjectId`] of the blob object
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The data that the blob contains
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl PartialEq for Blob {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Blob {}
impl PartialOrd for Blob {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Blob {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl Blob {
    pub(crate) fn new(id: ObjectId, data: Vec<u8>) -> Self {
        Blob { id, data }
    }

    /// Move the data out of the blob object.
    pub fn data_owned(self) -> Vec<u8> {
        self.data
    }
}
