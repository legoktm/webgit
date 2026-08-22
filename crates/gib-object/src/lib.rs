//! Types and parsers for git objects
//!
//! This crate contains data types for all git objects, and parses them from
//! decompressed object bytes. It performs no IO: fetching objects is
//! `gib-odb`'s job, and peeling (which needs lookups) lives in the facade's
//! extension traits.

#![deny(clippy::all)]

use gib_parse::{ParseError, ParseResult};
use jiff::{
    Timestamp, Zoned,
    tz::{Offset, TimeZone},
};
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_until},
    character::complete::{char, i32, i64, u64},
    combinator::all_consuming,
    sequence::terminated,
};
use sha1::{Digest, Sha1};

mod blob;
mod commit;
mod header;
mod tag;
mod tree;

pub use blob::Blob;
pub use commit::Commit;
pub use header::{ObjectHeader, ObjectHeaderIter};
pub use tag::Tag;
pub use tree::{Tree, TreeEntry, TreeEntryIter, TreeEntryType};

pub use gib_hash::ObjectId;

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

impl ObjectType {
    /// The name git writes in a loose object's header, which is also the name
    /// `git cat-file` answers to.
    pub fn name(self) -> &'static str {
        match self {
            ObjectType::Commit => "commit",
            ObjectType::Tag => "tag",
            ObjectType::Tree => "tree",
            ObjectType::Blob => "blob",
        }
    }
}

/// The size of a git object; a newtype wrapper around a [`u64`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectSize(pub u64);

/// An object's type and its decompressed body, before parsing.
#[derive(Debug)]
pub struct RawObject {
    /// The type of the object
    pub object_type: ObjectType,
    /// The raw decoded body bytes
    pub body: Vec<u8>,
}

impl RawObject {
    /// The ID these bytes actually have: SHA-1 over the loose-object header
    /// and the body, which is how git derives an object's name.
    ///
    /// The header hashed here is byte-for-byte the one [`parse_header`] reads,
    /// so the two stay in step.
    pub fn compute_id(&self) -> ObjectId {
        let mut hasher = Sha1::new();
        hasher.update(self.object_type.name().as_bytes());
        hasher.update(b" ");
        hasher.update(self.body.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(&self.body);
        ObjectId::from_bytes(hasher.finalize().into())
    }

    /// Check these bytes against the ID they were fetched under.
    ///
    /// A mismatch means the data is corrupt — a truncated range response, a
    /// bad delta reconstruction, a damaged pack — since an honest repository
    /// cannot name an object anything but its own hash.
    pub fn verify(&self, expected: ObjectId) -> Result<(), ObjectError> {
        let computed = self.compute_id();
        if computed == expected {
            Ok(())
        } else {
            Err(ObjectError::HashMismatch { expected, computed })
        }
    }
}

/// An object turned out to be of a different type than the caller required.
#[expect(missing_docs)]
#[derive(Debug)]
pub struct UnexpectedObjectType {
    pub id: ObjectId,
    pub expected: ObjectType,
    pub received: ObjectType,
}

/// Something went wrong turning an object's bytes into an [`Object`].
///
/// Each variant carries the [`ObjectId`] whose bytes were at fault, so a
/// caller can report *which* object is corrupt without having to remember what
/// it asked for.
#[derive(Debug)]
pub enum ObjectError {
    /// The object's bytes did not parse; `snippet` is the start of the input
    /// at which parsing failed.
    Parse {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        snippet: Vec<u8>,
    },
    /// A header the object type requires (e.g. a commit's `tree`) was absent.
    MissingFields(ObjectId),
    /// The object's bytes do not hash to the ID it was fetched under.
    HashMismatch {
        /// The ID the object was asked for by.
        expected: ObjectId,
        /// The ID its bytes actually have.
        computed: ObjectId,
    },
}

impl ObjectError {
    /// Attach `id` to a parse failure, which by itself doesn't know which
    /// object it was reading.
    fn annotate(id: ObjectId) -> impl Fn(ParseError) -> Self {
        move |error| match error {
            ParseError::ParseError { input_snippet } => ObjectError::Parse {
                id,
                snippet: input_snippet,
            },
            ParseError::MissingFields => ObjectError::MissingFields(id),
        }
    }
}

/// A git object
///
/// This type encapsulates the four possible types of git object.
#[derive(Clone)]
pub enum Object {
    #[expect(missing_docs)]
    Commit(Commit),
    #[expect(missing_docs)]
    Tree(Tree),
    #[expect(missing_docs)]
    Tag(Tag),
    #[expect(missing_docs)]
    Blob(Blob),
}

impl Object {
    /// The ID of the object
    pub fn id(&self) -> ObjectId {
        use Object::*;
        match self {
            Commit(c) => c.id(),
            Tree(t) => t.id(),
            Tag(t) => t.id(),
            Blob(b) => b.id(),
        }
    }

    /// Get the object type as a plain (fieldless) enum.
    pub fn object_type(&self) -> ObjectType {
        use Object::*;
        match self {
            Commit(_) => ObjectType::Commit,
            Tree(_) => ObjectType::Tree,
            Tag(_) => ObjectType::Tag,
            Blob(_) => ObjectType::Blob,
        }
    }

    /// Coerce the object to a [`Commit`].
    ///
    /// Returns `Err` if the object was not a commit.
    pub fn commit(self) -> Result<Commit, UnexpectedObjectType> {
        use Object::*;
        match self {
            Commit(c) => Ok(c),
            _ => Err(UnexpectedObjectType {
                id: self.id(),
                expected: ObjectType::Commit,
                received: self.object_type(),
            }),
        }
    }

    /// Coerce the object to a [`Tag`].
    ///
    /// Returns `Err` if the object was not a tag.
    pub fn tag(self) -> Result<Tag, UnexpectedObjectType> {
        use Object::*;
        match self {
            Tag(t) => Ok(t),
            _ => Err(UnexpectedObjectType {
                id: self.id(),
                expected: ObjectType::Tag,
                received: self.object_type(),
            }),
        }
    }

    /// Coerce the object to a [`Tree`]
    ///
    /// Returns `Err` if the object was not a tree.
    pub fn tree(self) -> Result<Tree, UnexpectedObjectType> {
        use Object::*;
        match self {
            Tree(t) => Ok(t),
            _ => Err(UnexpectedObjectType {
                id: self.id(),
                expected: ObjectType::Tree,
                received: self.object_type(),
            }),
        }
    }

    /// Coerce the object to a [`Blob`]
    ///
    /// Returns `Err` if the object was not a blob.
    pub fn blob(self) -> Result<Blob, UnexpectedObjectType> {
        use Object::*;
        match self {
            Blob(b) => Ok(b),
            _ => Err(UnexpectedObjectType {
                id: self.id(),
                expected: ObjectType::Blob,
                received: self.object_type(),
            }),
        }
    }

    /// Parse a [`RawObject`] into a typed [`Object`].
    pub fn from_raw(id: ObjectId, raw: RawObject) -> Result<Self, ObjectError> {
        let RawObject { object_type, body } = raw;
        let object = match object_type {
            ObjectType::Commit => {
                Object::Commit(Commit::parse(id, body).map_err(ObjectError::annotate(id))?)
            }
            ObjectType::Tag => {
                Object::Tag(Tag::parse(id, body).map_err(ObjectError::annotate(id))?)
            }
            ObjectType::Blob => Object::Blob(Blob::new(id, body)),
            ObjectType::Tree => {
                Object::Tree(Tree::parse(id, body).map_err(ObjectError::annotate(id))?)
            }
        };
        Ok(object)
    }
}

/// Parse a loose object's header: `<type> SP <size> NUL`, returning the rest of
/// the input (the object body) alongside them.
pub fn parse_header(input: &[u8]) -> nom::IResult<&[u8], (ObjectSize, ObjectType)> {
    let (rest, (object_type, size)) = (
        terminated(
            alt((
                tag("commit").map(|_| ObjectType::Commit),
                tag("tag").map(|_| ObjectType::Tag),
                tag("tree").map(|_| ObjectType::Tree),
                tag("blob").map(|_| ObjectType::Blob),
            )),
            char(' '),
        ),
        terminated(u64, char('\0')).map(ObjectSize),
    )
        .parse(input)?;
    Ok((rest, (size, object_type)))
}

#[allow(clippy::type_complexity)]
fn parse_author_committer_tagger(input: &[u8]) -> ParseResult<&[u8], (&[u8], &[u8], Zoned)> {
    (
        terminated(take_until(" <"), tag(" <")),
        terminated(take_until("> "), tag("> ")),
        (
            terminated(i64, char(' ')),
            alt((char('+').map(|_| 1), char('-').map(|_| -1))),
            take(2usize).and_then(all_consuming(i32)),
            take(2usize).and_then(all_consuming(i32)),
        )
            .map_opt(|(timestamp, tz_sign, tz_hour, tz_minute)| {
                let date = Timestamp::from_second(timestamp).ok()?;
                let offset =
                    Offset::from_seconds(tz_sign * (3600 * tz_hour + 60 * tz_minute)).ok()?;
                let author_date = date.to_zoned(TimeZone::fixed(offset));
                Some(author_date)
            }),
    )
        .parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_author_committer_line() {
        let example = "an author <an-email-address> 0 +0000";
        parse_author_committer_tagger(example.as_bytes()).unwrap();
    }

    fn raw(object_type: ObjectType, body: &[u8]) -> RawObject {
        RawObject {
            object_type,
            body: body.to_vec(),
        }
    }

    /// Vectors taken from `git hash-object`.
    #[test]
    fn compute_id_matches_git() {
        let cases: [(ObjectType, &[u8], &str); 4] = [
            (
                ObjectType::Blob,
                b"",
                "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
            ),
            (
                ObjectType::Blob,
                b"hello world\n",
                "3b18e512dba79e4c8300dd08aeb37f8e728b8dad",
            ),
            (
                ObjectType::Tree,
                b"",
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            ),
            (
                ObjectType::Commit,
                b"tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb\n\
                  author a user <an-email-address> 946684800 +0000\n\
                  committer a user <an-email-address> 946684800 +0000\n\
                  \n\
                  a commit\n",
                "78dc5b70bd81aa46ec7dfce87a69826e354a916b",
            ),
        ];
        for (object_type, body, expected) in cases {
            assert_eq!(raw(object_type, body).compute_id().to_string(), expected);
        }
    }

    /// The type name is part of the hash, so the same bytes under a different
    /// type must not produce the same ID.
    #[test]
    fn compute_id_covers_the_type() {
        assert_ne!(
            raw(ObjectType::Blob, b"").compute_id(),
            raw(ObjectType::Tree, b"").compute_id()
        );
    }

    #[test]
    fn verify_accepts_and_rejects() {
        let object = raw(ObjectType::Blob, b"hello world\n");
        let id = object.compute_id();
        assert!(object.verify(id).is_ok());

        let corrupt = raw(ObjectType::Blob, b"hello w0rld\n");
        let Err(ObjectError::HashMismatch { expected, computed }) = corrupt.verify(id) else {
            panic!("a body that does not hash to `id` must not verify against it");
        };
        assert_eq!(expected, id);
        assert_eq!(computed, corrupt.compute_id());
    }

    #[test]
    fn parse_loose_header() {
        let (body, (size, object_type)) = parse_header(b"blob 5\0hello").unwrap();
        assert_eq!(body, b"hello");
        assert_eq!(size, ObjectSize(5));
        assert_eq!(object_type, ObjectType::Blob);
        assert!(parse_header(b"nonsense 5\0hello").is_err());
    }
}

#[cfg(test)]
mod differential;
