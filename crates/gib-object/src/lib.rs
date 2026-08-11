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
