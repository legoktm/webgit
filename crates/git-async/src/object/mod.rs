//! A module for working with git objects
//!
//! This module contains data types for all git objects. Objects are acquired
//! from a [`Repo`] by looking them up using their [`ObjectId`], or from one of
//! the `lookup_*` family of methods on existing objects.

use crate::{
    error::{Error, GResult, InternalObjectError, UnexpectedObjectType, annotate_with_object_id},
    file_system::FileSystem,
    object_store::lookup::lookup,
    parsing::ParseResult,
    repo::Repo,
};
use accessory::Accessors;
use alloc::format;
use jiff::{
    Timestamp, Zoned,
    tz::{Offset, TimeZone},
};
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_until},
    character::complete::{char, hex_digit0, i32, i64},
    combinator::all_consuming,
    sequence::terminated,
};

mod blob;
mod commit;
mod header;
mod tag;
mod tree;

pub use crate::object::blob::Blob;
pub use crate::object::commit::Commit;
pub use crate::object::header::{ObjectHeader, ObjectHeaderIter};
pub use crate::object::tag::Tag;
pub use crate::object::tree::{Tree, TreeEntry, TreeEntryIter, TreeEntryType};
pub use crate::object_store::{ObjectType, RawObject};

/// The ID of a git object
///
/// `git-async` only supports SHA-1 repositories, so this is always 20 bytes or
/// 40 hex characters
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Accessors)]
pub struct ObjectId {
    /// The object ID as an array of bytes
    #[access(get)]
    pub(crate) bytes: [u8; 20],
}

impl alloc::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut chars = [0u8; 40];
        hex::encode_to_slice(self.bytes, &mut chars).unwrap();
        write!(f, "{}", str::from_utf8(&chars).unwrap())
    }
}

impl alloc::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("ObjectId").field(&format!("{self}")).finish()
    }
}

impl ObjectId {
    /// Construct an [`ObjectId`] from an array of bytes.
    pub const fn from_bytes(id: [u8; 20]) -> Self {
        Self { bytes: id }
    }

    /// Construct an [`ObjectId`] from a hex (byte)string.
    ///
    /// Returns `None` if the provided string was not 40 hexadecimal characters.
    pub fn from_hex(s: &[u8]) -> Option<Self> {
        let (_, oid) = all_consuming(Self::parse).parse(s).ok()?;
        Some(oid)
    }

    #[cfg_attr(not(feature = "diff"), allow(dead_code))]
    pub(crate) const fn zero() -> Self {
        Self { bytes: [0u8; 20] }
    }

    pub(crate) fn parse(input: &[u8]) -> ParseResult<&[u8], Self> {
        take(40usize)
            .and_then(all_consuming(hex_digit0))
            .map_res(|hex_str| {
                let mut buf = [0u8; 20];
                hex::decode_to_slice(hex_str, &mut buf)?;
                Ok::<ObjectId, hex::FromHexError>(ObjectId::from_bytes(buf))
            })
            .parse(input)
    }
}

/// An abbreviated [`ObjectId`]: a hex prefix naming every object whose ID
/// starts with it.
///
/// Git lets an object be named by any prefix of its hash that is unambiguous,
/// and commit messages conventionally quote 7-12 character abbreviations.
/// [`Repo::resolve_prefix`](crate::Repo::resolve_prefix) expands one back into
/// a full [`ObjectId`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectIdPrefix {
    /// The prefix's hex characters, packed two per byte and zero-padded to the
    /// width of a full ID, so it can be compared against one directly.
    bytes: [u8; 20],
    /// How many hex characters (nibbles) of `bytes` are significant.
    nibbles: usize,
}

impl ObjectIdPrefix {
    /// Parse an abbreviated object ID from a hex (byte)string.
    ///
    /// Returns `None` unless the input is 4 to 40 hexadecimal characters (of
    /// either case). Four is git's own minimum abbreviation length; anything
    /// shorter is not a useful name for an object, and a resolver would be
    /// searching only to report that it matched everything.
    pub fn from_hex(s: &[u8]) -> Option<Self> {
        if !(4..=40).contains(&s.len()) {
            return None;
        }
        let mut bytes = [0u8; 20];
        for (i, c) in s.iter().enumerate() {
            let nibble = (*c as char).to_digit(16)?;
            // The first character of a pair is the high nibble of its byte.
            bytes[i / 2] |= (nibble as u8) << if i % 2 == 0 { 4 } else { 0 };
        }
        Some(Self {
            bytes,
            nibbles: s.len(),
        })
    }

    /// The number of hex characters in the abbreviation.
    pub fn num_chars(&self) -> usize {
        self.nibbles
    }

    /// The prefix's first byte, which is fully determined because an
    /// abbreviation is at least four characters. Pack index lookups use it to
    /// pick the fanout bucket to search.
    pub(crate) fn first_byte(&self) -> u8 {
        self.bytes[0]
    }

    /// The lowest [`ObjectId`] this prefix covers: the prefix with every
    /// remaining nibble zero. Together with [`Self::last`] this bounds a range
    /// scan over an ordered collection of IDs.
    pub fn first(&self) -> ObjectId {
        ObjectId::from_bytes(self.bytes)
    }

    /// The highest [`ObjectId`] this prefix covers: the prefix with every
    /// remaining nibble `f`.
    pub fn last(&self) -> ObjectId {
        let mut bytes = self.bytes;
        let mut full_bytes = self.nibbles / 2;
        if self.nibbles % 2 == 1 {
            // The odd trailing character occupies only the high nibble of its
            // byte, so the low one is still free to grow.
            bytes[full_bytes] |= 0x0f;
            full_bytes += 1;
        }
        for byte in &mut bytes[full_bytes..] {
            *byte = 0xff;
        }
        ObjectId::from_bytes(bytes)
    }

    /// Where `id` sorts relative to the block of IDs this prefix covers:
    /// [`Ordering::Equal`](core::cmp::Ordering::Equal) means `id` *starts with*
    /// the prefix, not that it equals it. Since IDs are compared bytewise and
    /// the covered IDs form a contiguous run, this is a valid ordering to
    /// binary-search a sorted table with.
    pub fn compare(&self, id: &ObjectId) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        let full_bytes = self.nibbles / 2;
        match id.bytes[..full_bytes].cmp(&self.bytes[..full_bytes]) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        if self.nibbles % 2 == 1 {
            return (id.bytes[full_bytes] >> 4).cmp(&(self.bytes[full_bytes] >> 4));
        }
        Ordering::Equal
    }

    /// Whether `id` starts with this prefix.
    pub fn matches(&self, id: &ObjectId) -> bool {
        self.compare(id) == core::cmp::Ordering::Equal
    }
}

impl alloc::fmt::Display for ObjectIdPrefix {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let full = format!("{}", self.first());
        write!(f, "{}", &full[..self.nibbles])
    }
}

/// What an abbreviated object ID resolved to.
///
/// Git refuses to guess between objects sharing an abbreviation, and so does
/// this: [`Ambiguous`](Self::Ambiguous) is a distinct outcome from a match, so
/// callers can report it rather than picking one arbitrarily.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrefixResolution {
    /// No known object has this prefix.
    NotFound,
    /// Exactly one object has this prefix.
    Found(ObjectId),
    /// Several objects have this prefix, so it names none of them.
    Ambiguous,
}

impl PrefixResolution {
    /// Combine the results of searching two sources (e.g. two packs). Two
    /// sources agreeing on the same ID is not ambiguity — objects are
    /// content-addressed, so the same ID in two packs is the same object.
    pub fn merge(self, other: Self) -> Self {
        use PrefixResolution::*;
        match (self, other) {
            (Ambiguous, _) | (_, Ambiguous) => Ambiguous,
            (NotFound, result) | (result, NotFound) => result,
            (Found(a), Found(b)) if a == b => Found(a),
            (Found(_), Found(_)) => Ambiguous,
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

    /// Peel the object to a [`Commit`], if possible.
    pub async fn peel_to_commit<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Commit>> {
        use Object::*;
        let mut obj: Object = self.clone();
        loop {
            match obj {
                Commit(c) => return Ok(Some(c)),
                Tag(t) => {
                    let target = repo.lookup_object(t.target()).await?;
                    obj = target;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Peel the object to a [`Tree`], if possible.
    pub async fn peel_to_tree<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Tree>> {
        use Object::*;
        let mut obj: Object = self.clone();
        loop {
            match obj {
                Tree(t) => return Ok(Some(t)),
                Commit(c) => {
                    let tree = repo.lookup_object(c.tree()).await?;
                    obj = tree;
                }
                Tag(t) => {
                    let target = repo.lookup_object(t.target()).await?;
                    obj = target;
                }
                Blob(_) => return Ok(None),
            }
        }
    }

    /// Parse a [`RawObject`] into a typed [`Object`].
    pub fn from_raw(id: ObjectId, raw: RawObject) -> GResult<Self> {
        let RawObject { object_type, body } = raw;
        let object = match object_type {
            ObjectType::Commit => Object::Commit(
                Commit::parse(id, body)
                    .map_err(InternalObjectError::from)
                    .map_err(annotate_with_object_id(id))?,
            ),
            ObjectType::Tag => Object::Tag(
                Tag::parse(id, body)
                    .map_err(InternalObjectError::from)
                    .map_err(annotate_with_object_id(id))?,
            ),
            ObjectType::Blob => Object::Blob(Blob::new(id, body)),
            ObjectType::Tree => Object::Tree(
                Tree::parse(id, body)
                    .map_err(InternalObjectError::from)
                    .map_err(annotate_with_object_id(id))?,
            ),
        };
        Ok(object)
    }

    pub(crate) async fn lookup<F: FileSystem>(repo: &Repo<F>, id: ObjectId) -> GResult<Self> {
        let raw = lookup(repo, id)
            .await?
            .ok_or_else(|| Error::MissingObject(id))?;
        Self::from_raw(id, raw)
    }
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
    use crate::test::helpers::{make_basic_repo, make_similar_commits};
    use futures::executor::block_on;

    #[test]
    fn lookup_commit() {
        let test_repo = make_basic_repo().unwrap();
        let commit_id = test_repo.run_git(["rev-parse", "HEAD"]).unwrap();
        let commit_id = ObjectId::from_hex(commit_id.trim_ascii()).unwrap();

        let repo = test_repo.repo();
        let object = block_on(Object::lookup(&repo, commit_id)).unwrap();
        assert_eq!(object.id(), commit_id);
        assert!(matches!(object, Object::Commit(_)));
    }

    #[test]
    fn lookup_packfile_object() {
        let test_repo = make_basic_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
        let repo = test_repo.repo();
        let head = block_on(repo.head()).unwrap();
        let oid = block_on(head.resolve_object_id(&repo)).unwrap();
        let Object::Commit(commit) = block_on(repo.lookup_object(oid)).unwrap() else {
            panic!()
        };
        let tree_id = commit.tree();
        let Object::Tree(tree) = block_on(repo.lookup_object(tree_id)).unwrap() else {
            panic!()
        };
        assert_eq!(tree.entries().len(), 1 + 26 - 2);
    }

    #[test]
    fn parse_author_committer_line() {
        let example = "an author <an-email-address> 0 +0000";
        parse_author_committer_tagger(example.as_bytes()).unwrap();
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    #[test]
    fn parse_object_id_prefix() {
        // Too short (git's own minimum is 4), too long, or not hex.
        assert!(ObjectIdPrefix::from_hex(b"abc").is_none());
        assert!(ObjectIdPrefix::from_hex(&[b'a'; 41]).is_none());
        assert!(ObjectIdPrefix::from_hex(b"abcg").is_none());
        // Both cases are accepted, as for a full ID.
        assert_eq!(
            ObjectIdPrefix::from_hex(b"ABC1").unwrap(),
            ObjectIdPrefix::from_hex(b"abc1").unwrap()
        );
        let prefix = ObjectIdPrefix::from_hex(b"0123abc").unwrap();
        assert_eq!(prefix.num_chars(), 7);
        assert_eq!(format!("{prefix}"), "0123abc");
    }

    #[test]
    fn object_id_prefix_bounds() {
        // An even-length prefix ends on a byte boundary...
        let even = ObjectIdPrefix::from_hex(b"0123ab").unwrap();
        assert_eq!(
            even.first(),
            oid("0123ab0000000000000000000000000000000000")
        );
        assert_eq!(even.last(), oid("0123abffffffffffffffffffffffffffffffffff"));
        // ...an odd-length one leaves the low nibble of its last byte free.
        let odd = ObjectIdPrefix::from_hex(b"0123a").unwrap();
        assert_eq!(odd.first(), oid("0123a00000000000000000000000000000000000"));
        assert_eq!(odd.last(), oid("0123afffffffffffffffffffffffffffffffffff"));
        // A full-length abbreviation covers exactly one ID.
        let full = ObjectIdPrefix::from_hex(&[b'3'; 40]).unwrap();
        assert_eq!(full.first(), full.last());
    }

    #[test]
    fn object_id_prefix_compare() {
        use core::cmp::Ordering;
        let prefix = ObjectIdPrefix::from_hex(b"0123a").unwrap();
        assert!(prefix.matches(&oid("0123a0deadbeef00000000000000000000000000")));
        assert!(prefix.matches(&oid("0123afffffffffffffffffffffffffffffffffff")));
        // Differing inside the odd trailing nibble is still a miss, in the
        // right direction, so a binary search over sorted IDs converges.
        assert_eq!(
            prefix.compare(&oid("01239fffffffffffffffffffffffffffffffffff")),
            Ordering::Less
        );
        assert_eq!(
            prefix.compare(&oid("0123b00000000000000000000000000000000000")),
            Ordering::Greater
        );
        assert_eq!(
            prefix.compare(&oid("0122ffffffffffffffffffffffffffffffffffff")),
            Ordering::Less
        );
    }

    #[test]
    fn prefix_resolution_merge() {
        use PrefixResolution::*;
        let a = oid("0123abcd0123abcd0123abcd0123abcd0123abcd");
        let b = oid("0123abcdffffffffffffffffffffffffffffffff");
        assert_eq!(NotFound.merge(Found(a)), Found(a));
        assert_eq!(Found(a).merge(NotFound), Found(a));
        // The same object packed twice is one object, not an ambiguity.
        assert_eq!(Found(a).merge(Found(a)), Found(a));
        assert_eq!(Found(a).merge(Found(b)), Ambiguous);
        assert_eq!(Ambiguous.merge(NotFound), Ambiguous);
        assert_eq!(NotFound.merge(NotFound), NotFound);
    }
}
