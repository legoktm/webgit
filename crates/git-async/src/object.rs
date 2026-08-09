//! A module for working with git objects
//!
//! This module contains data types for all git objects. Objects are acquired
//! from a [`Repo`](crate::Repo) by looking them up using their [`ObjectId`].
//!
//! The types and parsers themselves live in the `gib-object` crate and are
//! re-exported here. Operations that need to *fetch* objects — peeling a tag
//! to its commit, listing a commit's parents, resolving a tree entry — take a
//! repository, so they are extension traits in [`crate::prelude`] rather than
//! inherent methods.

pub use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};
pub use gib_object::{
    Blob, Commit, Object, ObjectHeader, ObjectHeaderIter, ObjectType, RawObject, Tag, Tree,
    TreeEntry, TreeEntryIter, TreeEntryType,
};

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
        let object = block_on(repo.lookup_object(commit_id)).unwrap();
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
}
