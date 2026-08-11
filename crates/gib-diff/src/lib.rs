//! Computing diffs between git trees
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
//! # use gib_diff::{DiffError, TreeDiff};
//! # use gib_fs::FileSystem;
//! # use gib_object::Tree;
//! # use gib_odb::ObjectDb;
//! async fn changed_files<F: FileSystem>(
//!     odb: &ObjectDb<F>,
//!     left: &Tree,
//!     right: &Tree,
//! ) -> Result<TreeDiff, DiffError> {
//!     TreeDiff::new(odb, left, right).await
//! }
//! ```

#![deny(clippy::all)]

use gib_fs::FileSystem;
use gib_hash::ObjectId;
use gib_object::{
    Object, ObjectError, Tree, TreeEntry, TreeEntryIter, TreeEntryType, UnexpectedObjectType,
};
use gib_odb::{ObjectDb, OdbError};
use std::cmp::Ordering;

/// Something went wrong while walking two trees.
#[derive(Debug)]
pub enum DiffError {
    /// The caller's `cancel` callback asked for the walk to stop.
    Canceled,
    /// A sub-tree could not be read.
    Odb(OdbError),
    /// A sub-tree's bytes did not parse.
    Object(ObjectError),
    /// A tree entry pointed at an object the database does not have.
    MissingObject(ObjectId),
    /// A tree entry marked as a sub-tree turned out to name something else.
    UnexpectedObjectType(UnexpectedObjectType),
}

impl From<OdbError> for DiffError {
    fn from(value: OdbError) -> Self {
        Self::Odb(value)
    }
}

impl From<ObjectError> for DiffError {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<UnexpectedObjectType> for DiffError {
    fn from(value: UnexpectedObjectType) -> Self {
        Self::UnexpectedObjectType(value)
    }
}

/// A path for a file in a diff
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path(Vec<u8>);

impl std::fmt::Debug for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// Compare two tree entries by name the way git orders them within a tree.
///
/// Git sorts entries as if every directory name ended in a `/`, so the file
/// `a.b` sorts *before* the directory `a` — `"a.b" < "a/"` — even though the
/// raw bytes say the opposite. The merge walk below advances two trees in
/// lockstep and so has to order them by the same rule git wrote them with;
/// comparing raw bytes misaligns the walk wherever a file and a directory
/// share a prefix, spuriously reporting the directory as deleted and re-added.
fn entry_name_cmp(left: &TreeEntry<'_>, right: &TreeEntry<'_>) -> Ordering {
    /// The byte that follows a name at index `at` for ordering purposes:
    /// a real name byte, else `/` for a directory, else nothing — the end of
    /// a name sorts before every byte a name may contain.
    fn byte_at(entry: &TreeEntry<'_>, at: usize) -> Option<u8> {
        entry
            .name()
            .get(at)
            .copied()
            .or_else(|| (entry.entry_type() == TreeEntryType::Tree).then_some(b'/'))
    }

    let common = left.name().len().min(right.name().len());
    left.name()[..common]
        .cmp(&right.name()[..common])
        .then_with(|| byte_at(left, common).cmp(&byte_at(right, common)))
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
pub struct TreeDiff {
    /// The entries of the diff, one per differing path in the tree
    entries: Vec<DiffEntry<(ObjectId, ObjectId)>>,
}

#[expect(clippy::too_many_lines)]
async fn tree_diff_impl<E: From<DiffError>>(
    left: &Tree,
    right: &Tree,
    mut lookup: impl AsyncFnMut(ObjectId) -> Result<Object, E>,
    mut cancel: impl AsyncFnMut() -> bool,
) -> Result<TreeDiff, E> {
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
            return Err(DiffError::Canceled.into());
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
                (Some(l), Some(r)) => entry_name_cmp(l, r),
            };
            // Skip entries that are identical on both sides. The mode is part
            // of that, not just the object: a file that only gained the
            // executable bit — or was replaced by a symlink to the same
            // content — keeps its blob id, and git still calls that a change.
            if name_ordering == Ordering::Equal
                && left_entry.as_ref().map(TreeEntry::id) == right_entry.as_ref().map(TreeEntry::id)
                && left_entry.as_ref().map(TreeEntry::entry_type)
                    == right_entry.as_ref().map(TreeEntry::entry_type)
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
                            let tree = lookup(left.id()).await?.tree().map_err(DiffError::from)?;
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
                            let tree = lookup(right.id()).await?.tree().map_err(DiffError::from)?;
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
                                let left_tree =
                                    lookup(left.id()).await?.tree().map_err(DiffError::from)?;
                                let right_tree =
                                    lookup(right.id()).await?.tree().map_err(DiffError::from)?;
                                stack.push((Some(path), Some(left_tree), Some(right_tree)));
                            }
                            (TreeEntryType::Tree, _) => {
                                let left_tree =
                                    lookup(left.id()).await?.tree().map_err(DiffError::from)?;
                                stack.push((Some(path.clone()), Some(left_tree), None));
                                out.push(DiffEntry::RightOnly {
                                    path,
                                    entry_type: right.entry_type(),
                                    content: (ObjectId::zero(), right.id()),
                                });
                            }
                            (_, TreeEntryType::Tree) => {
                                let right_tree =
                                    lookup(right.id()).await?.tree().map_err(DiffError::from)?;
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
    /// The entries of the diff, one per differing path in the tree
    pub fn entries(&self) -> &[DiffEntry<(ObjectId, ObjectId)>] {
        &self.entries
    }

    /// Construct a [`TreeDiff`] by diffing two trees
    pub async fn new<F: FileSystem>(
        odb: &ObjectDb<F>,
        left: &Tree,
        right: &Tree,
    ) -> Result<Self, DiffError> {
        Self::new_cancelable(odb, left, right, async || false).await
    }

    /// Construct a [`TreeDiff`] by diffing two trees using a custom async lookup function.
    ///
    /// This is useful when you have a caching wrapper around object lookups and want all
    /// sub-tree fetches during the tree walk to go through that cache.
    ///
    /// The lookup's error type is the caller's, so a consumer with its own
    /// error enum keeps it, as long as this crate's failures can be folded in.
    pub async fn new_with_lookup<E: From<DiffError>>(
        left: &Tree,
        right: &Tree,
        lookup: impl AsyncFnMut(ObjectId) -> Result<Object, E>,
    ) -> Result<Self, E> {
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
    /// # use gib_diff::{DiffError, TreeDiff};
    /// # use gib_fs::FileSystem;
    /// # use gib_object::Tree;
    /// # use gib_odb::ObjectDb;
    /// # use std::rc::Rc;
    /// # use std::cell::Cell;
    /// struct CancelableDiffFactory { canceled: Rc<Cell<bool>> }
    /// impl CancelableDiffFactory {
    ///     pub async fn make_diff<F: FileSystem>(
    ///         &self,
    ///         odb: &ObjectDb<F>,
    ///         left: &Tree,
    ///         right: &Tree
    ///     ) -> Result<TreeDiff, DiffError> {
    ///         let canceled = self.canceled.clone();
    ///         let cancel = async move || canceled.get();
    ///         TreeDiff::new_cancelable(odb, left, right, cancel).await
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
        odb: &ObjectDb<F>,
        left: &Tree,
        right: &Tree,
        cancel: impl AsyncFnMut() -> bool,
    ) -> Result<Self, DiffError> {
        tree_diff_impl(
            left,
            right,
            async |id| {
                let raw = odb.lookup(id).await?.ok_or(DiffError::MissingObject(id))?;
                Ok(Object::from_raw(id, raw)?)
            },
            cancel,
        )
        .await
    }
}

#[cfg(test)]
mod test_support {
    use futures::executor::block_on;
    use gib_fs::Directory;
    use gib_hash::ObjectId;
    use gib_object::{Object, Tree};
    use gib_odb::ObjectDb;
    use gib_testkit::{TestFileSystem, TestRepo};

    pub(crate) fn open_odb(test_repo: &TestRepo) -> ObjectDb<TestFileSystem> {
        let objects_dir = block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap();
        block_on(ObjectDb::open(objects_dir, 64 * 1024 * 1024)).unwrap()
    }

    pub(crate) fn object(odb: &ObjectDb<TestFileSystem>, id: ObjectId) -> Object {
        let raw = block_on(odb.lookup(id)).unwrap().unwrap();
        Object::from_raw(id, raw).unwrap()
    }

    /// The tree of the commit `rev` names, read through the odb.
    pub(crate) fn tree_at(test_repo: &TestRepo, odb: &ObjectDb<TestFileSystem>, rev: &str) -> Tree {
        let out = test_repo
            .run_git(["rev-parse", &format!("{rev}^{{tree}}")])
            .unwrap();
        let id = ObjectId::from_hex(out.trim_ascii_end()).unwrap();
        object(odb, id).tree().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{open_odb, tree_at};
    use futures::executor::block_on;
    use gib_testkit::{TestRepo, make_basic_repo, make_file};
    use std::{
        collections::BTreeSet,
        fs::{create_dir, remove_file},
        io::Write,
        path::PathBuf,
    };

    fn commit(test_repo: &TestRepo) {
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit("a commit", "a user", "an-email", "2000-01-01T00:00:00Z")
            .unwrap();
    }

    fn diff(
        test_repo: &TestRepo,
        left: &str,
        right: &str,
    ) -> BTreeSet<DiffEntry<(ObjectId, ObjectId)>> {
        let odb = open_odb(test_repo);
        let left = tree_at(test_repo, &odb, left);
        let right = tree_at(test_repo, &odb, right);
        block_on(TreeDiff::new(&odb, &left, &right))
            .unwrap()
            .entries()
            .iter()
            .map(Clone::clone)
            .collect()
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    const EMPTY: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    const SOME_DATA: &str = "7c0646bfd53c1f0ed45ffd81563f30017717ca58";

    #[test]
    fn diff_same() {
        let test_repo = make_basic_repo().unwrap();
        let odb = open_odb(&test_repo);
        let tree = tree_at(&test_repo, &odb, "HEAD");
        assert!(
            block_on(TreeDiff::new(&odb, &tree, &tree))
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn basic_root_diff() {
        let test_repo = make_basic_repo().unwrap();
        let mut file_a = make_file(&test_repo, "a").unwrap();
        commit(&test_repo);
        file_a.write_all(b"some data").unwrap();
        file_a.flush().unwrap();
        let mut file_b = make_file(&test_repo, "b").unwrap();
        file_b.write_all(b"some more data").unwrap();
        commit(&test_repo);

        assert_eq!(
            diff(&test_repo, "HEAD~1", "HEAD"),
            vec![
                DiffEntry::Both {
                    path: Path(b"a".to_vec()),
                    left_type: TreeEntryType::File,
                    right_type: TreeEntryType::File,
                    content: (oid(EMPTY), oid(SOME_DATA)),
                },
                DiffEntry::RightOnly {
                    path: Path(b"b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        ObjectId::zero(),
                        oid("dfa37ec69ffae3abcf7efbb386226cb84b510fa8")
                    )
                }
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            diff(&test_repo, "HEAD", "HEAD~1"),
            vec![
                DiffEntry::Both {
                    path: Path(b"a".to_vec()),
                    left_type: TreeEntryType::File,
                    right_type: TreeEntryType::File,
                    content: (oid(SOME_DATA), oid(EMPTY)),
                },
                DiffEntry::LeftOnly {
                    path: Path(b"b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (
                        oid("dfa37ec69ffae3abcf7efbb386226cb84b510fa8"),
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
        create_dir(test_repo.location.path().join("dir")).unwrap();
        let mut file_a = make_file(&test_repo, PathBuf::from("dir").join("a")).unwrap();
        commit(&test_repo);
        file_a.write_all(b"some data").unwrap();
        file_a.flush().unwrap();
        commit(&test_repo);

        assert_eq!(
            diff(&test_repo, "HEAD~1", "HEAD"),
            vec![DiffEntry::Both {
                path: Path(b"dir/a".to_vec()),
                left_type: TreeEntryType::File,
                right_type: TreeEntryType::File,
                content: (oid(EMPTY), oid(SOME_DATA)),
            }]
            .into_iter()
            .collect()
        );
        assert_eq!(
            diff(&test_repo, "HEAD", "HEAD~1"),
            vec![DiffEntry::Both {
                path: Path(b"dir/a".to_vec()),
                left_type: TreeEntryType::File,
                right_type: TreeEntryType::File,
                content: (oid(SOME_DATA), oid(EMPTY)),
            }]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn complex_subtree_diff() {
        let test_repo = make_basic_repo().unwrap();
        make_file(&test_repo, "a").unwrap();
        commit(&test_repo);
        // `a` becomes a directory, and a second directory appears: the walk has
        // to descend into a path that used to be a file.
        remove_file(test_repo.location.path().join("a")).unwrap();
        create_dir(test_repo.location.path().join("a")).unwrap();
        make_file(&test_repo, PathBuf::from("a").join("b")).unwrap();
        create_dir(test_repo.location.path().join("dir")).unwrap();
        make_file(&test_repo, PathBuf::from("dir").join("c")).unwrap();
        commit(&test_repo);

        assert_eq!(
            diff(&test_repo, "HEAD~1", "HEAD"),
            vec![
                DiffEntry::RightOnly {
                    path: Path(b"a/b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (ObjectId::zero(), oid(EMPTY)),
                },
                DiffEntry::LeftOnly {
                    path: Path(b"a".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (oid(EMPTY), ObjectId::zero())
                },
                DiffEntry::RightOnly {
                    path: Path(b"dir/c".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (ObjectId::zero(), oid(EMPTY)),
                },
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            diff(&test_repo, "HEAD", "HEAD~1"),
            vec![
                DiffEntry::LeftOnly {
                    path: Path(b"a/b".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (oid(EMPTY), ObjectId::zero())
                },
                DiffEntry::RightOnly {
                    path: Path(b"a".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (ObjectId::zero(), oid(EMPTY)),
                },
                DiffEntry::LeftOnly {
                    path: Path(b"dir/c".to_vec()),
                    entry_type: TreeEntryType::File,
                    content: (oid(EMPTY), ObjectId::zero())
                },
            ]
            .into_iter()
            .collect()
        );
    }

    /// Git writes tree entries as if directory names ended in `/`, so the file
    /// `a.b` is stored *before* the directory `a`. A walk that orders the two
    /// sides by raw bytes disagrees, falls out of step, and reports the
    /// untouched `a/` as wholly deleted and re-added.
    #[test]
    fn file_and_directory_name_collision() {
        let test_repo = make_basic_repo().unwrap();
        create_dir(test_repo.location.path().join("a")).unwrap();
        make_file(&test_repo, PathBuf::from("a").join("inside")).unwrap();
        make_file(&test_repo, "a.b").unwrap();
        make_file(&test_repo, "a-c").unwrap();
        commit(&test_repo);
        remove_file(test_repo.location.path().join("a.b")).unwrap();
        commit(&test_repo);

        let expected: BTreeSet<_> = vec![DiffEntry::LeftOnly {
            path: Path(b"a.b".to_vec()),
            entry_type: TreeEntryType::File,
            content: (oid(EMPTY), ObjectId::zero()),
        }]
        .into_iter()
        .collect();
        assert_eq!(diff(&test_repo, "HEAD~1", "HEAD"), expected);
    }

    /// A `cancel` callback that fires immediately stops the walk.
    #[test]
    fn cancellation() {
        let test_repo = make_basic_repo().unwrap();
        make_file(&test_repo, "a").unwrap();
        commit(&test_repo);
        make_file(&test_repo, "b").unwrap();
        commit(&test_repo);
        let odb = open_odb(&test_repo);
        let left = tree_at(&test_repo, &odb, "HEAD~1");
        let right = tree_at(&test_repo, &odb, "HEAD");
        let result = block_on(TreeDiff::new_cancelable(&odb, &left, &right, async || true));
        assert!(matches!(result, Err(DiffError::Canceled)));
    }
}

#[cfg(test)]
mod differential;
