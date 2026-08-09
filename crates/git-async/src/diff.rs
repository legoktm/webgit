//! A module for computing diffs between git trees
//!
//! # Usage
//!
//! Construct a [`TreeDiff`] object, which walks the specified trees and finds
//! the files that differ between them, recording the [`ObjectId`] of each side.
//! Iterate [`TreeDiff::entries`] to inspect the changes; resolving the object
//! IDs to blobs and computing line-by-line diffs is left to the caller.
//!
//! # Example
//!
//! ```
//! # use git_async::{diff::TreeDiff, error::GResult, object::Tree, Repo, file_system::FileSystem};
//! async fn changed_files<F: FileSystem>(repo: &Repo<F>, left: &Tree, right: &Tree) -> GResult<TreeDiff> {
//!     TreeDiff::new(repo, left, right).await
//! }
//! ```

use crate::{
    Repo,
    error::{Error, GResult},
    file_system::FileSystem,
    object::{Object, ObjectId, Tree, TreeEntry, TreeEntryIter, TreeEntryType},
};
use accessory::Accessors;
use alloc::{string::String, vec::Vec};
use core::cmp::Ordering;

/// A path for a file in a diff
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path(Vec<u8>);

impl core::fmt::Debug for Path {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match str::from_utf8(&self.0) {
            Ok(p) => f.debug_tuple("Path").field(&p).finish(),
            Err(_) => f
                .debug_tuple("Path")
                .field(&String::from_utf8_lossy(&self.0))
                .finish(),
        }
    }
}

impl Path {
    /// View the path as a slice of bytes
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// Consume the path and return its inner [`Vec<u8>`]
    pub fn inner(self) -> Vec<u8> {
        self.0
    }
}

fn join(path: Option<&Path>, component: &[u8]) -> Path {
    match path {
        Some(p) => {
            let mut out = Vec::with_capacity(p.0.len() + 1 + component.len());
            out.extend_from_slice(&p.0);
            out.push(b'/');
            out.extend_from_slice(component);
            Path(out)
        }
        None => Path(component.to_vec()),
    }
}

/// Represents a diff of a single file
///
/// It is generic over the content of the file diff. For tree diffs, `Content`
/// is a pair of [`ObjectId`]s, one of which may be zero.
#[expect(missing_docs)]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum DiffEntry<Content> {
    LeftOnly {
        path: Path,
        entry_type: TreeEntryType,
        content: Content,
    },
    Both {
        path: Path,
        left_type: TreeEntryType,
        right_type: TreeEntryType,
        content: Content,
    },
    RightOnly {
        path: Path,
        entry_type: TreeEntryType,
        content: Content,
    },
}

impl<Content> DiffEntry<Content> {
    /// The content of the diff entry
    pub fn content(&self) -> &Content {
        match self {
            DiffEntry::LeftOnly { content, .. }
            | DiffEntry::Both { content, .. }
            | DiffEntry::RightOnly { content, .. } => content,
        }
    }

    /// The path of the file that the entry represents
    pub fn path(&self) -> &Path {
        match self {
            DiffEntry::LeftOnly { path, .. }
            | DiffEntry::Both { path, .. }
            | DiffEntry::RightOnly { path, .. } => path,
        }
    }
}

/// A diff of git trees, holding the [`ObjectId`]s of differing files
#[derive(Accessors)]
pub struct TreeDiff {
    /// The entries of the diff, one per differing path in the tree
    #[access(get(ty(&[DiffEntry<(ObjectId, ObjectId)>])))]
    entries: Vec<DiffEntry<(ObjectId, ObjectId)>>,
}

#[expect(clippy::too_many_lines)]
async fn tree_diff_impl(
    left: &Tree,
    right: &Tree,
    mut lookup: impl AsyncFnMut(ObjectId) -> GResult<Object>,
    mut cancel: impl AsyncFnMut() -> bool,
) -> GResult<TreeDiff> {
    type StackInner = (Option<Path>, Option<Tree>, Option<Tree>);
    if left.id() == right.id() {
        return Ok(TreeDiff {
            entries: Vec::new(),
        });
    }
    let mut out: Vec<DiffEntry<(ObjectId, ObjectId)>> = Vec::new();
    let mut stack: Vec<StackInner> = Vec::new();
    stack.push((None, Some(left.clone()), Some(right.clone())));

    while let Some((parent_path, left, right)) = stack.pop() {
        // Loop invariants:
        // - one of left or right is Some()
        // - left and right have different IDs
        debug_assert!(left.is_some() || right.is_some());
        debug_assert!(left.as_ref().map(Tree::id) != right.as_ref().map(Tree::id));
        if cancel().await {
            return Err(Error::DiffCanceled);
        }

        let mut left_entries: Option<TreeEntryIter> = left.as_ref().map(Tree::entries);
        let mut right_entries: Option<TreeEntryIter> = right.as_ref().map(Tree::entries);
        let mut left_entry: Option<TreeEntry> = left_entries.as_mut().and_then(Iterator::next);
        let mut right_entry: Option<TreeEntry> = right_entries.as_mut().and_then(Iterator::next);
        while left_entry.is_some() || right_entry.is_some() {
            let name_ordering: Ordering = match (&left_entry, &right_entry) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(l), Some(r)) => l.name().cmp(r.name()),
            };
            if name_ordering == Ordering::Equal
                && left_entry.as_ref().map(TreeEntry::id) == right_entry.as_ref().map(TreeEntry::id)
            {
                left_entry = left_entries.as_mut().and_then(Iterator::next);
                right_entry = right_entries.as_mut().and_then(Iterator::next);
                continue;
            }

            match name_ordering {
                Ordering::Less => {
                    if let Some(left) = left_entry {
                        let path = join(parent_path.as_ref(), left.name());
                        if left.entry_type() == TreeEntryType::Tree {
                            let tree = lookup(left.id()).await?.tree()?;
                            stack.push((Some(path), Some(tree), None));
                        } else {
                            out.push(DiffEntry::LeftOnly {
                                path,
                                entry_type: left.entry_type(),
                                content: (left.id(), ObjectId::zero()),
                            });
                        }
                    }
                    left_entry = left_entries.as_mut().and_then(Iterator::next);
                }
                Ordering::Greater => {
                    if let Some(right) = right_entry {
                        let path = join(parent_path.as_ref(), right.name());
                        if right.entry_type() == TreeEntryType::Tree {
                            let tree = lookup(right.id()).await?.tree()?;
                            stack.push((Some(path), None, Some(tree)));
                        } else {
                            out.push(DiffEntry::RightOnly {
                                path,
                                entry_type: right.entry_type(),
                                content: (ObjectId::zero(), right.id()),
                            });
                        }
                    }
                    right_entry = right_entries.as_mut().and_then(Iterator::next);
                }
                Ordering::Equal => {
                    if let (Some(left), Some(right)) = (left_entry, right_entry) {
                        let path = join(parent_path.as_ref(), left.name()); // names are equal
                        match (left.entry_type(), right.entry_type()) {
                            (TreeEntryType::Tree, TreeEntryType::Tree) => {
                                let left_tree = lookup(left.id()).await?.tree()?;
                                let right_tree = lookup(right.id()).await?.tree()?;
                                stack.push((Some(path), Some(left_tree), Some(right_tree)));
                            }
                            (TreeEntryType::Tree, _) => {
                                let left_tree = lookup(left.id()).await?.tree()?;
                                stack.push((Some(path.clone()), Some(left_tree), None));
                                out.push(DiffEntry::RightOnly {
                                    path,
                                    entry_type: right.entry_type(),
                                    content: (ObjectId::zero(), right.id()),
                                });
                            }
                            (_, TreeEntryType::Tree) => {
                                let right_tree = lookup(right.id()).await?.tree()?;
                                stack.push((Some(path.clone()), None, Some(right_tree)));
                                out.push(DiffEntry::LeftOnly {
                                    path,
                                    entry_type: left.entry_type(),
                                    content: (left.id(), ObjectId::zero()),
                                });
                            }
                            _ => out.push(DiffEntry::Both {
                                path,
                                left_type: left.entry_type(),
                                right_type: right.entry_type(),
                                content: (left.id(), right.id()),
                            }),
                        }
                    }
                    left_entry = left_entries.as_mut().and_then(Iterator::next);
                    right_entry = right_entries.as_mut().and_then(Iterator::next);
                }
            }
        }
    }
    Ok(TreeDiff { entries: out })
}

impl TreeDiff {
    /// Construct a [`TreeDiff`] by diffing two trees
    pub async fn new<F: FileSystem>(repo: &Repo<F>, left: &Tree, right: &Tree) -> GResult<Self> {
        Self::new_cancelable(repo, left, right, async || false).await
    }

    /// Construct a [`TreeDiff`] by diffing two trees using a custom async lookup function.
    ///
    /// This is useful when you have a caching wrapper around object lookups and want all
    /// sub-tree fetches during the tree walk to go through that cache.
    pub async fn new_with_lookup(
        left: &Tree,
        right: &Tree,
        lookup: impl AsyncFnMut(ObjectId) -> GResult<Object>,
    ) -> GResult<Self> {
        tree_diff_impl(left, right, lookup, async || false).await
    }

    /// Construct a [`TreeDiff`] by diffing two trees
    ///
    /// The `cancel` parameter is a function which may cancel the diff operation
    /// by returning `true` at any point. It is called regularly while the diff
    /// operation is running.
    ///
    /// For example,
    /// ```
    /// # use git_async::{diff::TreeDiff, error::GResult, object::Tree, Repo, file_system::FileSystem};
    /// # use std::rc::Rc;
    /// # use core::cell::Cell;
    /// struct CancelableDiffFactory { canceled: Rc<Cell<bool>> }
    /// impl CancelableDiffFactory {
    ///     pub async fn make_diff<F: FileSystem>(
    ///         &self,
    ///         repo: &Repo<F>,
    ///         left: &Tree,
    ///         right: &Tree
    ///     ) -> GResult<TreeDiff> {
    ///         let canceled = self.canceled.clone();
    ///         let cancel = async move || canceled.get();
    ///         TreeDiff::new_cancelable(repo, left, right, cancel).await
    ///     }
    ///
    ///     pub fn cancel(&self) {
    ///         self.canceled.set(true);
    ///     }
    /// }
    /// ```
    ///
    /// In this example, a diff operation may be started by some async routine,
    /// and then canceled by another by calling the
    /// `CancelableDiffFactory::cancel` method.
    pub async fn new_cancelable<F: FileSystem>(
        repo: &Repo<F>,
        left: &Tree,
        right: &Tree,
        cancel: impl AsyncFnMut() -> bool,
    ) -> GResult<Self> {
        tree_diff_impl(left, right, async |id| repo.lookup_object(id).await, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use crate::{Repo, prelude::*, reference::RefName, test::open_test_repo};
    use futures::executor::block_on;
    use gib_testkit::{TestFileSystem, make_basic_repo, make_file};
    use std::{
        collections::BTreeSet,
        fs::{create_dir, remove_file},
        io::Write,
        path::PathBuf,
    };

    use super::*;

    fn head_tree(repo: &Repo<TestFileSystem>) -> Tree {
        let head = block_on(repo.lookup_ref(&RefName::Head)).unwrap();
        block_on(head.peel_to_tree(repo)).unwrap().unwrap()
    }

    #[test]
    fn diff_same() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let tree = head_tree(&repo);
        assert!(
            block_on(TreeDiff::new(&repo, &tree, &tree))
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn basic_root_diff() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        let mut file_a = make_file(&test_repo, "a").unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let before = head_tree(&repo);
        file_a.write_all(b"some data").unwrap();
        file_a.flush().unwrap();
        let mut file_b = make_file(&test_repo, "b").unwrap();
        file_b.write_all(b"some more data").unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let after = head_tree(&repo);
        let the_diff = block_on(TreeDiff::new(&repo, &before, &after))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![
                DiffEntry::Both {
                    path: Path(b"a".to_vec()),
                    left_type: TreeEntryType::File,
                    right_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                        ObjectId::from_hex(b"7c0646bfd53c1f0ed45ffd81563f30017717ca58").unwrap(),
                    ),
                },
                DiffEntry::RightOnly {
                    path: Path(b"b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::zero(),
                        ObjectId::from_hex(b"dfa37ec69ffae3abcf7efbb386226cb84b510fa8").unwrap()
                    )
                }
            ]
            .into_iter()
            .collect()
        );
        let the_diff = block_on(TreeDiff::new(&repo, &after, &before))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![
                DiffEntry::Both {
                    path: Path(b"a".to_vec()),
                    left_type: TreeEntryType::File,
                    right_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"7c0646bfd53c1f0ed45ffd81563f30017717ca58").unwrap(),
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                    ),
                },
                DiffEntry::LeftOnly {
                    path: Path(b"b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"dfa37ec69ffae3abcf7efbb386226cb84b510fa8").unwrap(),
                        ObjectId::zero()
                    )
                }
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn basic_subtree_diff() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        create_dir(test_repo.location.path().join("dir")).unwrap();
        let mut file_a = make_file(&test_repo, PathBuf::from("dir").join("a")).unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let before = head_tree(&repo);
        file_a.write_all(b"some data").unwrap();
        file_a.flush().unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let after = head_tree(&repo);
        let the_diff = block_on(TreeDiff::new(&repo, &before, &after))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![DiffEntry::Both {
                path: Path(b"dir/a".to_vec()),
                left_type: TreeEntryType::File,
                right_type: TreeEntryType::File,
                content: (
                    ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                    ObjectId::from_hex(b"7c0646bfd53c1f0ed45ffd81563f30017717ca58").unwrap(),
                )
            },]
            .into_iter()
            .collect()
        );
        let the_diff = block_on(TreeDiff::new(&repo, &after, &before))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![DiffEntry::Both {
                path: Path(b"dir/a".to_vec()),
                left_type: TreeEntryType::File,
                right_type: TreeEntryType::File,
                content: (
                    ObjectId::from_hex(b"7c0646bfd53c1f0ed45ffd81563f30017717ca58").unwrap(),
                    ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                ),
            },]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn complex_subtree_diff() {
        let test_repo = make_basic_repo().unwrap();
        let repo = open_test_repo(&test_repo);
        make_file(&test_repo, "a").unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let before = head_tree(&repo);
        remove_file(test_repo.location.path().join("a")).unwrap();
        create_dir(test_repo.location.path().join("a")).unwrap();
        make_file(&test_repo, PathBuf::from("a").join("b")).unwrap();
        create_dir(test_repo.location.path().join("dir")).unwrap();
        make_file(&test_repo, PathBuf::from("dir").join("c")).unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
        let after = head_tree(&repo);
        let the_diff = block_on(TreeDiff::new(&repo, &before, &after))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![
                DiffEntry::RightOnly {
                    path: Path(b"a/b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::zero(),
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                    )
                },
                DiffEntry::LeftOnly {
                    path: Path(b"a".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                        ObjectId::zero()
                    )
                },
                DiffEntry::RightOnly {
                    path: Path(b"dir/c".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::zero(),
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                    )
                },
            ]
            .into_iter()
            .collect()
        );
        let the_diff = block_on(TreeDiff::new(&repo, &after, &before))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            the_diff,
            vec![
                DiffEntry::LeftOnly {
                    path: Path(b"a/b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                        ObjectId::zero()
                    )
                },
                DiffEntry::RightOnly {
                    path: Path(b"a".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::zero(),
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                    )
                },
                DiffEntry::LeftOnly {
                    path: Path(b"dir/c".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
                        ObjectId::zero()
                    )
                },
            ]
            .into_iter()
            .collect()
        );
    }
}
