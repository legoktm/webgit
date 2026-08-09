//! Traits that add repository-aware operations to the object types.
//!
//! The object and ref types come from crates that know nothing about
//! repositories, so anything needing a lookup — peeling a tag to its commit,
//! reading a commit's parents, resolving a tree entry — is an extension trait
//! here rather than an inherent method. Import the prelude to get them all:
//!
//! ```
//! # #[allow(unused_imports)]
//! use git_async::prelude::*;
//! ```

use crate::{
    error::GResult,
    file_system::FileSystem,
    object::{Commit, Object, Tree, TreeEntry, TreeEntryType},
    repo::Repo,
};

/// Repository-aware operations on [`Object`].
pub trait ObjectExt {
    /// Peel the object to a [`Commit`], if possible.
    fn peel_to_commit<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Option<Commit>>>;

    /// Peel the object to a [`Tree`], if possible.
    fn peel_to_tree<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Option<Tree>>>;
}

impl ObjectExt for Object {
    async fn peel_to_commit<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Commit>> {
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

    async fn peel_to_tree<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Tree>> {
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
}

/// Repository-aware operations on [`Commit`].
pub trait CommitExt {
    /// Look up all the parents of the commit, using the provided [`Repo`].
    fn lookup_parents<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Vec<Commit>>>;
}

impl CommitExt for Commit {
    async fn lookup_parents<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Vec<Commit>> {
        let mut out = Vec::with_capacity(self.parents().len());
        for parent in self.parents() {
            out.push(repo.lookup_object(*parent).await?.commit()?);
        }
        Ok(out)
    }
}

/// Repository-aware operations on [`TreeEntry`].
pub trait TreeEntryExt {
    /// Look up the target object using the provided [`Repo`].
    ///
    /// Returns `None` if the tree entry is a commit, because in that case it is
    /// a pointer to a commit in an external repository.
    fn lookup<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Option<Object>>>;
}

impl TreeEntryExt for TreeEntry<'_> {
    async fn lookup<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Object>> {
        if self.entry_type() == TreeEntryType::Commit {
            Ok(None)
        } else {
            Ok(Some(repo.lookup_object(self.id()).await?))
        }
    }
}
