use crate::{
    commit_graph::CommitGraph,
    error::{Error, GResult},
    file_system::{
        DirEntry, Directory, FileSystem, FileSystemError, read_file_if_exists, search_for_files,
    },
    object::{Object, ObjectId},
    object_store::{ObjectSize, ObjectType, RawObject, cache::IndexCache, lookup::PackName},
    reference::{
        Ref, RefEntry, RefName, RefTarget, lookup_loose_ref, parse_info_refs, parse_packed_refs,
    },
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

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
    pub(crate) pack_dir: F::Directory,
    pub(crate) index_cache: IndexCache,
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
        let pack_dir = objects_dir.open_subdir(b"pack").await?;
        let pack_ids = Self::discover_packs(&objects_dir, &pack_dir).await?;
        let index_cache = IndexCache::new(&pack_dir, pack_ids, config).await?;
        // Best-effort: a missing or unsupported commit-graph just means falling
        // back to object reads, so any error degrades to `None`.
        let commit_graph = CommitGraph::open(&objects_dir).await.ok().flatten();
        Ok(Repo {
            git_dir,
            pack_dir,
            index_cache,
            commit_graph,
        })
    }

    /// The repository's commit-graph cache, if it has a usable one.
    pub fn commit_graph(&self) -> Option<&CommitGraph<F>> {
        self.commit_graph.as_ref()
    }

    /// Find the repository's packfiles.
    ///
    /// Prefer `objects/info/packs` — the manifest written by
    /// `git update-server-info` for fetching over dumb HTTP — so a repository
    /// prepared for static serving is discovered without ever listing a
    /// directory. This matters because many HTTP servers disable directory
    /// indexes, and listing them would be a guaranteed wasted (often failing)
    /// request. Only when the manifest is absent do we fall back to listing
    /// the pack directory, which still works on servers that expose an
    /// autoindex.
    ///
    /// The manifest is only as fresh as the last `update-server-info` run;
    /// this mirrors how [`Repo::all_refs`] prefers `info/refs` over a `refs/`
    /// walk for the same reason.
    async fn discover_packs(
        objects_dir: &F::Directory,
        pack_dir: &F::Directory,
    ) -> GResult<Vec<PackName>> {
        if let Some(packs) = Self::info_packs(objects_dir).await? {
            return Ok(packs);
        }
        Self::list_packs(pack_dir).await
    }

    /// Read `objects/info/packs` if present, returning `None` when there is no
    /// such manifest (the repository wasn't prepared with `update-server-info`).
    async fn info_packs(objects_dir: &F::Directory) -> GResult<Option<Vec<PackName>>> {
        let info_dir = match objects_dir.open_subdir(b"info").await {
            Ok(info_dir) => info_dir,
            Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(data) = read_file_if_exists(&info_dir, b"packs").await? else {
            return Ok(None);
        };
        Ok(Some(parse_info_packs(&data)?))
    }

    /// Discover packs by listing the pack directory's autoindex.
    async fn list_packs(pack_dir: &F::Directory) -> GResult<Vec<PackName>> {
        let entries = pack_dir.list_dir().await?;
        Ok(entries
            .into_iter()
            .filter_map(|dirent| {
                let DirEntry::File(name) = dirent else {
                    return None;
                };
                PackName::new(name)
            })
            .collect())
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
        Ref::lookup(self, &RefName::Head).await
    }

    /// Take a ref name and look up its content.
    pub async fn lookup_ref(&self, name: &RefName) -> GResult<Ref> {
        Ref::lookup(self, name).await
    }

    /// Look up a particular object in the repository, reading the entire object
    /// into memory.
    pub async fn lookup_object(&self, id: ObjectId) -> GResult<Object> {
        Object::lookup(self, id).await
    }

    /// Look up the raw (unparsed) bytes and type of an object.
    ///
    /// Returns `None` if the object does not exist in the repository.
    /// Use [`Object::from_raw`] to parse the result into a typed object.
    pub async fn lookup_raw(&self, id: ObjectId) -> GResult<Option<RawObject>> {
        crate::object_store::lookup::lookup(self, id).await
    }

    /// Look up the size and type of an object, without reading it to memory or
    /// parsing its content.
    pub async fn lookup_object_size_type(&self, id: ObjectId) -> GResult<(ObjectSize, ObjectType)> {
        Object::lookup_size_type(self, id).await
    }
}

/// Parse the `objects/info/packs` file written by `git update-server-info`.
///
/// Each line is `P <packfile-name>`.
fn parse_info_packs(data: &[u8]) -> GResult<Vec<PackName>> {
    let mut packs = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let name = line.strip_prefix(b"P ").ok_or(Error::MalformedInfoPacks)?;
        packs.push(PackName::from_pack_filename(name.to_vec()).ok_or(Error::MalformedInfoPacks)?);
    }
    Ok(packs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reference::RefTarget,
        test::{helpers::make_basic_repo, impls::TestFileSystem, repo::TestRepo},
    };
    use futures::executor::block_on;

    #[test]
    fn read_head() {
        let test_repo = TestRepo::new().unwrap();
        let repo = test_repo.repo();
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

        let repo = test_repo.repo();
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
        let repo = test_repo.repo();
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
        let test_repo = crate::test::helpers::make_packfile_repo().unwrap();
        remove_info_refs(&test_repo);
        let repo = test_repo.repo();
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
        let test_repo = crate::test::helpers::make_packfile_repo().unwrap();
        remove_info_refs(&test_repo);
        crate::test::helpers::make_file(&test_repo, "shadow-file").unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit(
                "another commit",
                "a user",
                "an-email-address",
                "2000-01-02T00:00:00Z",
            )
            .unwrap();

        let repo = test_repo.repo();
        let refs = block_on(repo.all_refs()).unwrap();
        // The new commit wrote a loose refs/heads/main which must win over
        // the stale entry still present in packed-refs.
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        assert_eq!(main.target(), head_oid(&test_repo));
    }

    #[test]
    fn all_refs_from_info_refs() {
        let test_repo = crate::test::helpers::make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        let head = head_oid(&test_repo);
        // Remove packed-refs to prove info/refs alone is consulted.
        std::fs::remove_file(test_repo.location.path().join(".git").join("packed-refs")).unwrap();

        let repo = test_repo.repo();
        let refs = block_on(repo.all_refs()).unwrap();
        let main = refs.get(&RefName::Ref(b"heads/main".to_vec())).unwrap();
        assert_eq!(main.target(), head);
        let fat_tag = refs.get(&RefName::Ref(b"tags/a-fat-tag".to_vec())).unwrap();
        assert_ne!(fat_tag.target(), head);
        assert_eq!(fat_tag.peeled(), Some(head));
    }

    #[test]
    fn parse_info_packs_lines() {
        let packs = parse_info_packs(b"P pack-0123abcd.pack\nP pack-fedcba98.pack\n\n").unwrap();
        assert_eq!(packs.len(), 2);
        assert_eq!(packs[0].pack_filename, b"pack-0123abcd.pack");
        assert_eq!(packs[0].index_filename, b"pack-0123abcd.idx");
        assert!(matches!(
            parse_info_packs(b"garbage\n"),
            Err(Error::MalformedInfoPacks)
        ));
    }

    #[test]
    fn discover_packs_prefers_info_packs() {
        let test_repo = crate::test::helpers::make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        let git_dir = test_repo.git_dir();
        let objects_dir = block_on(git_dir.open_subdir(b"objects")).unwrap();
        let pack_dir = block_on(objects_dir.open_subdir(b"pack")).unwrap();
        let expected = block_on(Repo::<TestFileSystem>::discover_packs(
            &objects_dir,
            &pack_dir,
        ))
        .unwrap();
        assert_eq!(expected.len(), 1);

        // objects/info/packs is consulted before the pack directory is listed,
        // so a server with no autoindex (an empty stand-in pack directory)
        // still discovers the same pack without a single directory listing.
        std::fs::create_dir(
            test_repo
                .location
                .path()
                .join(".git")
                .join("objects")
                .join("empty"),
        )
        .unwrap();
        let empty_dir = block_on(objects_dir.open_subdir(b"empty")).unwrap();
        let from_info = block_on(Repo::<TestFileSystem>::discover_packs(
            &objects_dir,
            &empty_dir,
        ))
        .unwrap();
        assert_eq!(from_info.len(), 1);
        assert_eq!(from_info[0].index_filename, expected[0].index_filename);
        assert_eq!(from_info[0].pack_filename, expected[0].pack_filename);
    }

    #[test]
    fn discover_packs_falls_back_to_listing() {
        let test_repo = crate::test::helpers::make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        // Remove the manifest so discovery must list the pack directory, as on
        // a repo that was never prepared with update-server-info but is served
        // from a host that does expose an autoindex.
        std::fs::remove_file(
            test_repo
                .location
                .path()
                .join(".git")
                .join("objects")
                .join("info")
                .join("packs"),
        )
        .unwrap();
        let git_dir = test_repo.git_dir();
        let objects_dir = block_on(git_dir.open_subdir(b"objects")).unwrap();
        let pack_dir = block_on(objects_dir.open_subdir(b"pack")).unwrap();
        let listed = block_on(Repo::<TestFileSystem>::discover_packs(
            &objects_dir,
            &pack_dir,
        ))
        .unwrap();
        assert_eq!(listed.len(), 1);
    }
}
