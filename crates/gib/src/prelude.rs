//! Traits that add repository-aware operations to the object types.
//!
//! The object and ref types come from crates that know nothing about
//! repositories, so anything needing a lookup — peeling a tag to its commit,
//! reading a commit's parents, resolving a tree entry — is an extension trait
//! here rather than an inherent method. Import the prelude to get them all:
//!
//! ```
//! # #[allow(unused_imports)]
//! use gib::prelude::*;
//! ```

use crate::{
    error::{Error, GResult},
    file_system::FileSystem,
    object::{Commit, Object, ObjectId, Tree, TreeEntry, TreeEntryType},
    reference::{Ref, RefTarget},
    repo::Repo,
};

/// How many refs a symbolic chain may pass through before resolution gives up.
///
/// Git stops after the same number (`SYMREF_MAXDEPTH` in refs.c), so a chain
/// this deep would not resolve anywhere else either. Counting hops rather than
/// remembering the refs already seen also breaks a symref loop, which a
/// repository can hold quite legally — `git symbolic-ref` will build one.
const SYMREF_MAX_DEPTH: usize = 5;

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
    ///
    /// Fails with [`Error::SymrefTooDeep`] if the chain passes through more
    /// than [`SYMREF_MAX_DEPTH`] refs.
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
        for _ in 0..SYMREF_MAX_DEPTH {
            match target.target() {
                RefTarget::Direct(oid) => return Ok(*oid),
                RefTarget::Symbolic(name) => {
                    let name = name.clone();
                    target = repo.lookup_ref(&name).await?;
                }
            }
        }
        Err(Error::SymrefTooDeep(self.name().clone()))
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

#[cfg(test)]
mod test {
    use crate::{
        error::Error, object::ObjectId, prelude::*, reference::RefName, test::open_test_repo,
    };
    use futures::executor::block_on;
    use gib_testkit::{TestRepo, make_basic_repo};
    use std::{sync::mpsc, thread, time::Duration};

    /// What resolution came back with, reduced to something that can cross a
    /// thread boundary — [`Error`] is not `Send`.
    #[derive(Debug, PartialEq, Eq)]
    enum Resolved {
        /// The commit `refs/heads/main` points at, i.e. the end of the chain.
        MainCommit,
        OtherOid(ObjectId),
        TooDeep,
        OtherError(String),
    }

    /// Resolve `refs/heads/entry` in a repository that `setup` has pointed
    /// somewhere, and give up waiting rather than block the test suite.
    ///
    /// An unbounded symref chain spins inside a single poll of the future, so
    /// nothing short of another thread can put a deadline on it; without one, a
    /// lost depth cap would hang the suite instead of failing it. The
    /// repository is built on that thread too, so nothing has to cross it but
    /// the answer.
    fn resolve_entry_ref(setup: fn(&TestRepo)) -> Resolved {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let test_repo = make_basic_repo().unwrap();
            setup(&test_repo);
            let main = test_repo.run_git(["rev-parse", "refs/heads/main"]).unwrap();
            let main = ObjectId::from_hex(main.trim_ascii_end()).unwrap();
            let repo = open_test_repo(&test_repo);
            let entry = RefName::Ref(b"heads/entry".to_vec());
            let entry = block_on(repo.lookup_ref(&entry)).unwrap();
            sender.send(match block_on(entry.resolve_object_id(&repo)) {
                Ok(oid) if oid == main => Resolved::MainCommit,
                Ok(oid) => Resolved::OtherOid(oid),
                Err(Error::SymrefTooDeep(_)) => Resolved::TooDeep,
                Err(e) => Resolved::OtherError(format!("{e:?}")),
            })
        });
        receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("resolving refs/heads/entry never finished")
    }

    /// Point `refs/heads/entry` at `refs/heads/main` through `hops` symbolic
    /// refs in total.
    fn make_symref_chain(test_repo: &TestRepo, hops: usize) {
        let mut target = "refs/heads/main".to_string();
        for link in 1..hops {
            let name = format!("refs/heads/link{link}");
            test_repo.run_git(["symbolic-ref", &name, &target]).unwrap();
            target = name;
        }
        test_repo
            .run_git(["symbolic-ref", "refs/heads/entry", &target])
            .unwrap();
    }

    /// A chain as long as git itself will follow still resolves.
    #[test]
    fn symref_chain_within_depth_limit_resolves() {
        let resolved = resolve_entry_ref(|test_repo| make_symref_chain(test_repo, 4));
        assert_eq!(resolved, Resolved::MainCommit);
    }

    /// A chain longer than git will follow is an error, not a truncated or
    /// invented answer.
    #[test]
    fn symref_chain_past_depth_limit_is_an_error() {
        let resolved = resolve_entry_ref(|test_repo| make_symref_chain(test_repo, 5));
        assert_eq!(resolved, Resolved::TooDeep);
    }

    /// A symref loop terminates instead of being followed forever.
    #[test]
    fn symref_loop_is_an_error() {
        let resolved = resolve_entry_ref(|test_repo| {
            test_repo
                .run_git(["symbolic-ref", "refs/heads/entry", "refs/heads/other"])
                .unwrap();
            test_repo
                .run_git(["symbolic-ref", "refs/heads/other", "refs/heads/entry"])
                .unwrap();
        });
        assert_eq!(resolved, Resolved::TooDeep);
    }
}
