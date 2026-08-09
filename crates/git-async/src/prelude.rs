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
    object::{Commit, Object, ObjectId, Tree, TreeEntry, TreeEntryType},
    reference::{Ref, RefTarget},
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

/// Repository-aware operations on [`Ref`].
pub trait RefExt {
    /// Follow a chain of refs until a direct ref is obtained, and return the
    /// object ID that it points to.
    fn resolve_object_id<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<ObjectId>>;

    /// Peel the ref to a commit object.
    ///
    /// Returns `None` if the ref does not point to a commit object.
    fn peel_to_commit<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Option<Commit>>>;

    /// Peel the ref to a tree object.
    ///
    /// Returns `None` if the ref does not point to a commit or a tree object.
    fn peel_to_tree<F: FileSystem>(
        &self,
        repo: &Repo<F>,
    ) -> impl Future<Output = GResult<Option<Tree>>>;
}

impl RefExt for Ref {
    async fn resolve_object_id<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<ObjectId> {
        let mut target: Ref = self.clone();
        while let RefTarget::Symbolic(name) = target.target() {
            let name = name.clone();
            target = repo.lookup_ref(&name).await?;
        }
        match target.target() {
            RefTarget::Symbolic(_) => unreachable!(),
            RefTarget::Direct(oid) => Ok(*oid),
        }
    }

    async fn peel_to_commit<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Commit>> {
        let oid = self.resolve_object_id(repo).await?;
        let object = repo.lookup_object(oid).await?;
        object.peel_to_commit(repo).await
    }

    async fn peel_to_tree<F: FileSystem>(&self, repo: &Repo<F>) -> GResult<Option<Tree>> {
        let oid = self.resolve_object_id(repo).await?;
        let object = repo.lookup_object(oid).await?;
        object.peel_to_tree(repo).await
    }
}
