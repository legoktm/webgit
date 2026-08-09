//! A module for working with git objects
//!
//! This module contains data types for all git objects. Objects are acquired
//! from a [`Repo`] by looking them up using their [`ObjectId`], or from one of
//! the `lookup_*` family of methods on existing objects.

use crate::{
    error::{Error, GResult, InternalObjectError, UnexpectedObjectType, annotate_with_object_id},
    file_system::FileSystem,
    object_store::lookup::lookup,
    repo::Repo,
};
use gib_parse::ParseResult;
use jiff::{
    Timestamp, Zoned,
    tz::{Offset, TimeZone},
};
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take, take_until},
    character::complete::{char, i32, i64},
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
pub use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};

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
    use crate::test::open_test_repo;
    use futures::executor::block_on;
    use gib_testkit::{make_basic_repo, make_similar_commits};

    #[test]
    fn lookup_commit() {
        let test_repo = make_basic_repo().unwrap();
        let commit_id = test_repo.run_git(["rev-parse", "HEAD"]).unwrap();
        let commit_id = ObjectId::from_hex(commit_id.trim_ascii()).unwrap();

        let repo = open_test_repo(&test_repo);
        let object = block_on(Object::lookup(&repo, commit_id)).unwrap();
        assert_eq!(object.id(), commit_id);
        assert!(matches!(object, Object::Commit(_)));
    }

    #[test]
    fn lookup_packfile_object() {
        let test_repo = make_basic_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
        let repo = open_test_repo(&test_repo);
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
}
