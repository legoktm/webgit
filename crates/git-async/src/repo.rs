use crate::{
    commit_graph::CommitGraph,
    error::{Error, GResult},
    file_system::{Directory, FileSystem, FileSystemError, read_file_if_exists, search_for_files},
    object::{Object, ObjectId, ObjectIdPrefix, PrefixResolution, RawObject},
    prelude::RefExt,
    reference::{
        Ref, RefEntry, RefName, RefTarget, lookup_loose_ref, lookup_ref, parse_info_refs,
        parse_packed_refs,
    },
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use gib_odb::ObjectDb;

/// Configuration for opening a repository
pub struct RepoConfig {
    pub(crate) index_offset_cache_max: usize,
}
impl RepoConfig {
    /// Construct a default [`RepoConfig`].
    ///
    /// See [`RepoConfig::default()`] for further details.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum size of the cache that holds object offsets from pack index files.
    pub fn index_offset_cache_max(&mut self, size: usize) -> &mut Self {
        self.index_offset_cache_max = size;
        self
    }

    /// Open a repo with this configuration.
    pub async fn open<F: FileSystem>(&self, open_dir: F::Directory) -> GResult<Repo<F>> {
        Repo::open_with_config(open_dir, self).await
    }
}

impl Default for RepoConfig {
    /// Creates a default [`RepoConfig`].
    ///
    /// The default maximum size of the index offset cache is 64 MiB.
    fn default() -> Self {
        Self {
            index_offset_cache_max: 64 * 1024 * 1024,
        }
    }
}

/// A handle to a Git repository
///
/// It is generic over the implementation of filesystem operations.
pub struct Repo<F: FileSystem> {
    pub(crate) git_dir: F::Directory,
    /// Where objects are found: packs, their indexes, and loose objects.
    odb: ObjectDb<F>,
    /// The commit-graph cache, if the repository has a usable single-file one.
    commit_graph: Option<CommitGraph<F>>,
}

impl<F: FileSystem> Repo<F> {
    pub(crate) async fn open_with_config(
        open_dir: F::Directory,
        config: &RepoConfig,
    ) -> GResult<Self> {
        let git_dir = Self::resolve_git_dir(open_dir).await?;
        let objects_dir = git_dir.open_subdir(b"objects").await?;
        let odb = ObjectDb::open(objects_dir.clone(), config.index_offset_cache_max).await?;
        // Best-effort: a missing or unsupported commit-graph just means falling
        // back to object reads, so any error degrades to `None`.
        let commit_graph = CommitGraph::open(&objects_dir).await.ok().flatten();
        Ok(Repo {
            git_dir,
            odb,
            commit_graph,
        })
    }

    /// The repository's commit-graph cache, if it has a usable one.
    pub fn commit_graph(&self) -> Option<&CommitGraph<F>> {
        self.commit_graph.as_ref()
    }

    pub(crate) async fn resolve_git_dir(open_dir: F::Directory) -> GResult<F::Directory> {
        // Probe for HEAD by reading it: with lazily-opened files, existence is
        // only known once a read is attempted.
        if read_file_if_exists(&open_dir, b"HEAD").await?.is_some() {
            return Ok(open_dir);
        }
        let git_dir = open_dir.open_subdir(b".git").await?;
        if read_file_if_exists(&git_dir, b"HEAD").await?.is_some() {
            Ok(git_dir)
        } else {
            Err(Error::NotAGitRepository)
        }
    }

    /// Open the repository located at `git_dir` using a default [`RepoConfig`].
    pub async fn open(git_dir: F::Directory) -> GResult<Self> {
        Self::open_with_config(git_dir, &RepoConfig::default()).await
    }

    /// Collect all the refs tracked by the repository
    ///
    /// Includes HEAD, branches, tags, remotes and the stash
    pub async fn ref_names(&self) -> GResult<BTreeSet<RefName>> {
        let mut out: BTreeSet<RefName> = BTreeSet::new();
        out.insert(RefName::Head);
        if let Some(data) = read_file_if_exists(&self.git_dir, b"packed-refs").await? {
            for (ref_name, _) in parse_packed_refs(&data)? {
                out.insert(ref_name);
            }
        }
        out.extend(self.loose_ref_names().await?);
        Ok(out)
    }

    /// Resolve every ref under `refs/` to its object ID in as few reads as
    /// possible.
    ///
    /// If the repository has an `info/refs` file (written by
    /// `git update-server-info`; present on servers prepared for fetching over
    /// dumb HTTP), it is used as the single source — note it is only as fresh
    /// as the last `update-server-info` run. Otherwise refs are assembled from
    /// `packed-refs` plus a walk of the `refs/` directory, with loose refs
    /// shadowing stale packed entries.
    ///
    /// `HEAD` is not included; use [`Repo::head`] for it.
    ///
    /// For annotated tags, [`RefEntry::peeled`] carries the peeled commit ID
    /// when the source recorded one; otherwise callers must peel the tag
    /// object themselves.
    pub async fn all_refs(&self) -> GResult<BTreeMap<RefName, RefEntry>> {
        if let Some(refs) = self.info_refs().await? {
            return Ok(refs);
        }
        self.packed_and_loose_refs().await
    }

    async fn info_refs(&self) -> GResult<Option<BTreeMap<RefName, RefEntry>>> {
        let info_dir = match self.git_dir.open_subdir(b"info").await {
            Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(dir) => dir,
        };
        let Some(data) = read_file_if_exists(&info_dir, b"refs").await? else {
            return Ok(None);
        };
        Ok(Some(parse_info_refs(&data)?.into_iter().collect()))
    }

    async fn packed_and_loose_refs(&self) -> GResult<BTreeMap<RefName, RefEntry>> {
        let mut out: BTreeMap<RefName, RefEntry> = BTreeMap::new();
        if let Some(data) = read_file_if_exists(&self.git_dir, b"packed-refs").await? {
            out.extend(parse_packed_refs(&data)?);
        }
        for name in self.loose_ref_names().await? {
            let Some(target) = lookup_loose_ref(self, &name).await? else {
                continue;
            };
            let target = match target {
                RefTarget::Direct(oid) => oid,
                RefTarget::Symbolic(next) => {
                    self.lookup_ref(&next)
                        .await?
                        .resolve_object_id(self)
                        .await?
                }
            };
            out.insert(
                name,
                RefEntry {
                    target,
                    peeled: None,
                },
            );
        }
        Ok(out)
    }

    async fn loose_ref_names(&self) -> GResult<Vec<RefName>> {
        let refs_dir = self.git_dir.open_subdir(b"refs").await?;
        let refs_paths = search_for_files(&refs_dir).await?;
        Ok(refs_paths
            .into_iter()
            .map(|path| {
                let mut name: Vec<u8> = Vec::new();
                for component in path {
                    if !name.is_empty() {
                        name.push(b'/');
                    }
                    name.extend_from_slice(&component);
                }
                RefName::Ref(name)
            })
            .collect())
    }

    /// Get the repository's HEAD ref.
    pub async fn head(&self) -> GResult<Ref> {
        lookup_ref(self, &RefName::Head).await
    }

    /// Take a ref name and look up its content.
    pub(crate) async fn lookup_ref(&self, name: &RefName) -> GResult<Ref> {
        lookup_ref(self, name).await
    }

    /// Look up a particular object in the repository, reading the entire object
    /// into memory.
    pub async fn lookup_object(&self, id: ObjectId) -> GResult<Object> {
        let raw = self
            .lookup_raw(id)
            .await?
            .ok_or_else(|| Error::MissingObject(id))?;
        Ok(Object::from_raw(id, raw)?)
    }

    /// Expand an abbreviated object ID (see [`ObjectIdPrefix`]) into the full
    /// [`ObjectId`] of the object it names.
    ///
    /// Only packed objects are searched; see [`PrefixResolution`] for how an
    /// abbreviation shared by several objects is reported.
    pub async fn resolve_prefix(&self, prefix: &ObjectIdPrefix) -> GResult<PrefixResolution> {
        Ok(self.odb.resolve_prefix(prefix).await?)
    }

    /// Look up the raw (unparsed) bytes and type of an object.
    ///
    /// Returns `None` if the object does not exist in the repository.
    /// Use [`Object::from_raw`] to parse the result into a typed object.
    pub async fn lookup_raw(&self, id: ObjectId) -> GResult<Option<RawObject>> {
        Ok(self.odb.lookup(id).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{reference::RefTarget, test::open_test_repo};
    use futures::executor::block_on;
    use gib_testkit::{TestFileSystem, TestRepo, make_basic_repo, make_file, make_packfile_repo};

    #[test]
    fn read_head() {
        let test_repo = TestRepo::new().unwrap();
        let repo = open_test_repo(&test_repo);
        let head = block_on(repo.head()).unwrap();
        assert_eq!(
            head.target(),
            &RefTarget::Symbolic(RefName::Ref(Vec::from(b"heads/main")))
        );
    }

    #[test]
    fn read_refs() {
        let test_repo = make_basic_repo().unwrap();
        test_repo.run_git(["branch", "a-branch"]).unwrap();
        test_repo.run_git(["branch", "foo/a-branch"]).unwrap();
        test_repo.run_git(["tag", "thin-tag"]).unwrap();
        test_repo.run_git(["tag", "bar/thin-tag"]).unwrap();
        test_repo
            .run_git(["tag", "-a", "-m", "a tag message", "fat-tag"])
            .unwrap();
        test_repo
            .run_git(["update-ref", "refs/remotes/origin/main", "HEAD"])
            .unwrap();

        let repo = open_test_repo(&test_repo);
        let refs = block_on(repo.ref_names()).unwrap();
        let expected: BTreeSet<_> = vec![
            RefName::Head,
            RefName::Ref(b"stash".to_vec()),
            RefName::Ref(b"heads/main".to_vec()),
            RefName::Ref(b"heads/a-branch".to_vec()),
            RefName::Ref(b"heads/foo/a-branch".to_vec()),
            RefName::Ref(b"tags/thin-tag".to_vec()),
            RefName::Ref(b"tags/bar/thin-tag".to_vec()),
            RefName::Ref(b"tags/fat-tag".to_vec()),
            RefName::Ref(b"tags/a-fat-tag".to_vec()),
            RefName::Ref(b"remotes/origin/main".to_vec()),
        ]
        .into_iter()
        .collect();
        assert_eq!(&refs, &expected);
    }

    #[test]
    fn open_non_bare_repo() {
        let test_repo = make_basic_repo().unwrap();
        let root_dir = test_repo.root_dir();
        block_on(Repo::<TestFileSystem>::open(root_dir)).unwrap();
    }

    fn head_oid(test_repo: &TestRepo) -> ObjectId {
        let hex = test_repo.run_git(["rev-parse", "HEAD"]).unwrap();
        ObjectId::from_hex(hex.trim_ascii_end()).unwrap()
    }

    #[test]
    fn all_refs_loose() {
        let test_repo = make_basic_repo().unwrap();
        test_repo.run_git(["branch", "a-branch"]).unwrap();
        let repo = open_test_repo(&test_repo);
        let refs = block_on(repo.all_refs()).unwrap();

        let head = head_oid(&test_repo);
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        let branch = refs.get(&RefName::Ref(b"heads/a-branch".to_vec())).unwrap();
        assert_eq!(main.target(), head);
        assert_eq!(branch.target(), head);
        // Loose refs carry no peeled info; the annotated tag's target is the
        // tag object, not the commit.
        let fat_tag = refs.get(&RefName::Ref(b"tags/a-fat-tag".to_vec())).unwrap();
        assert_eq!(fat_tag.peeled(), None);
        assert_ne!(fat_tag.target(), head);
        assert!(!refs.contains_key(&RefName::Head));
    }

    /// `git gc` (via repack) runs `update-server-info`, so repos that have
    /// been packed also carry an `info/refs`. Remove it so a test exercises
    /// the packed-refs + loose-refs fallback rather than the info/refs path.
    fn remove_info_refs(test_repo: &TestRepo) {
        std::fs::remove_file(
            test_repo
                .location
                .path()
                .join(".git")
                .join("info")
                .join("refs"),
        )
        .unwrap();
    }

    #[test]
    fn all_refs_packed_with_peeled() {
        let test_repo = make_packfile_repo().unwrap();
        remove_info_refs(&test_repo);
        let repo = open_test_repo(&test_repo);
        let refs = block_on(repo.all_refs()).unwrap();

        let head = head_oid(&test_repo);
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        assert_eq!(main.target(), head);
        assert_eq!(main.peeled(), None);
        let fat_tag = refs.get(&RefName::Ref(b"tags/a-fat-tag".to_vec())).unwrap();
        assert_ne!(fat_tag.target(), head);
        assert_eq!(fat_tag.peeled(), Some(head));
        assert_eq!(fat_tag.commit_target(), head);
    }

    #[test]
    fn all_refs_loose_shadows_packed() {
        let test_repo = make_packfile_repo().unwrap();
        remove_info_refs(&test_repo);
        make_file(&test_repo, "shadow-file").unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit(
                "another commit",
                "a user",
                "an-email-address",
                "2000-01-02T00:00:00Z",
            )
            .unwrap();

        let repo = open_test_repo(&test_repo);
        let refs = block_on(repo.all_refs()).unwrap();
        // The new commit wrote a loose refs/heads/main which must win over
        // the stale entry still present in packed-refs.
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        assert_eq!(main.target(), head_oid(&test_repo));
    }

    #[test]
    fn all_refs_from_info_refs() {
        let test_repo = make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        let head = head_oid(&test_repo);
        // Remove packed-refs to prove info/refs alone is consulted.
        std::fs::remove_file(test_repo.location.path().join(".git").join("packed-refs")).unwrap();

        let repo = open_test_repo(&test_repo);
        let refs = block_on(repo.all_refs()).unwrap();
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        assert_eq!(main.target(), head);
        let fat_tag = refs.get(&RefName::Ref(b"tags/a-fat-tag".to_vec())).unwrap();
        assert_ne!(fat_tag.target(), head);
        assert_eq!(fat_tag.peeled(), Some(head));
    }
}
