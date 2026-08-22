//! The object database: finding an object's bytes, wherever they live.
//!
//! [`ObjectDb`] is opened on a repository's `objects/` directory. It discovers
//! the packs, keeps their indexes cached, and answers lookups packed-first with
//! a loose fallback. It owns all the "where does an object live" policy;
//! parsing what comes back is `gib-object`'s job and refs are the facade's.

#![deny(clippy::all)]

mod cache;
mod loose;

use cache::IndexCache;
use gib_fs::{
    CachingPageReader, DirEntry, Directory, FileSystem, FileSystemError, Offset,
    read_file_if_exists,
};
use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};
use gib_object::RawObject;
use gib_pack::{
    IndexedPackFile, PackError, PackObjectError, find_object_in_pack_index,
    find_prefix_in_pack_index, form_deltified_chain, reconstruct_deltified_object_from_chain,
};
use miniz_oxide::inflate::TINFLStatus;

/// Something went wrong finding or reading an object.
///
/// Where the failure is about a particular object, the variant carries its
/// [`ObjectId`]: the layers underneath work from pack offsets and file paths,
/// so attaching the ID the caller asked for is this crate's job.
#[derive(Debug)]
pub enum OdbError {
    #[expect(missing_docs)]
    FileSystem(FileSystemError),
    /// A pack or index could not be read.
    Pack(PackError),
    /// `objects/info/packs` was not in the expected `P <name>` form.
    MalformedInfoPacks,
    /// A packed object's header names a type this library does not know.
    MalformedPackObject(ObjectId),
    /// A loose object's header did not parse.
    MalformedObject(ObjectId),
    /// An object is larger than this platform's `usize` can address.
    ObjectTooLarge(ObjectId),
    /// A packed object's compressed body did not inflate.
    PackObjectDecompressError {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        status: TINFLStatus,
    },
    /// A loose object's compressed body did not inflate.
    LooseObjectDecompressError {
        #[expect(missing_docs)]
        id: ObjectId,
        #[expect(missing_docs)]
        status: TINFLStatus,
    },
}

impl From<FileSystemError> for OdbError {
    fn from(value: FileSystemError) -> Self {
        Self::FileSystem(value)
    }
}

impl From<PackError> for OdbError {
    fn from(value: PackError) -> Self {
        Self::Pack(value)
    }
}

/// Attach the [`ObjectId`] a lookup asked for to the errors `gib-pack` raises
/// while reconstructing it, which name only a pack offset.
fn annotate(id: ObjectId) -> impl Fn(PackObjectError) -> OdbError {
    move |error| match error {
        PackObjectError::Pack(error) => OdbError::Pack(error),
        PackObjectError::MalformedObject => OdbError::MalformedPackObject(id),
        PackObjectError::ObjectTooLarge => OdbError::ObjectTooLarge(id),
        PackObjectError::Decompress(status) => OdbError::PackObjectDecompressError { id, status },
    }
}

type OdbResult<T> = Result<T, OdbError>;

/// The `.idx`/`.pack` filename pair of one packfile.
#[derive(Clone)]
pub(crate) struct PackName {
    pub(crate) index_filename: Vec<u8>,
    pub(crate) pack_filename: Vec<u8>,
}

impl PackName {
    pub(crate) fn new(filename: Vec<u8>) -> Option<Self> {
        let stripped = filename.strip_suffix(b".idx")?;
        let mut pack_filename = Vec::with_capacity(filename.len() + 1);
        pack_filename.extend_from_slice(stripped);
        pack_filename.extend_from_slice(b".pack");
        Some(Self {
            index_filename: filename,
            pack_filename,
        })
    }

    pub(crate) fn from_pack_filename(filename: Vec<u8>) -> Option<Self> {
        let stripped = filename.strip_suffix(b".pack")?;
        let mut index_filename = Vec::with_capacity(filename.len());
        index_filename.extend_from_slice(stripped);
        index_filename.extend_from_slice(b".idx");
        Some(Self {
            index_filename,
            pack_filename: filename,
        })
    }
}

/// A repository's object store.
pub struct ObjectDb<F: FileSystem> {
    objects_dir: F::Directory,
    pack_dir: F::Directory,
    index_cache: IndexCache,
}

impl<F: FileSystem> ObjectDb<F> {
    /// Open the object database rooted at a repository's `objects/` directory.
    ///
    /// `index_offset_cache_max` bounds, in bytes, how much of the packs' offset
    /// tables is held in memory; past it, offsets are read from the index file
    /// per lookup instead.
    pub async fn open(objects_dir: F::Directory, index_offset_cache_max: usize) -> OdbResult<Self> {
        let pack_dir = objects_dir.open_subdir(b"pack").await?;
        let pack_names = Self::discover_packs(&objects_dir, &pack_dir).await?;
        let index_cache = IndexCache::new(&pack_dir, pack_names, index_offset_cache_max).await?;
        Ok(Self {
            objects_dir,
            pack_dir,
            index_cache,
        })
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
    /// this mirrors how the facade's `Repo::all_refs` prefers `info/refs` over
    /// a `refs/` walk for the same reason.
    async fn discover_packs(
        objects_dir: &F::Directory,
        pack_dir: &F::Directory,
    ) -> OdbResult<Vec<PackName>> {
        if let Some(packs) = Self::info_packs(objects_dir).await? {
            return Ok(packs);
        }
        Self::list_packs(pack_dir).await
    }

    /// Read `objects/info/packs` if present, returning `None` when there is no
    /// such manifest (the repository wasn't prepared with `update-server-info`).
    async fn info_packs(objects_dir: &F::Directory) -> OdbResult<Option<Vec<PackName>>> {
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
    async fn list_packs(pack_dir: &F::Directory) -> OdbResult<Vec<PackName>> {
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

    /// Look up the raw bytes and type of an object, or `None` if the database
    /// does not have it.
    pub async fn lookup(&self, id: ObjectId) -> OdbResult<Option<RawObject>> {
        // Look in packs first, falling back to loose objects only on a miss.
        // Most objects are packed, so probing loose first would mean a
        // guaranteed-404 request per lookup on a packed repo. A loose object
        // (e.g. from a recent push) is still found by the fallback, and since
        // git objects are content-addressed a packed copy is byte-identical to
        // any loose copy, so the order does not affect correctness.
        if let Some((mut indexed_pack, offset)) = self.find_packed_object(id).await? {
            let (chain, object_type, final_object) =
                form_deltified_chain(&mut indexed_pack, offset)
                    .await
                    .map_err(annotate(id))?;
            let body =
                reconstruct_deltified_object_from_chain(&mut indexed_pack, &chain, &final_object)
                    .await
                    .map_err(annotate(id))?;
            return Ok(Some(RawObject { object_type, body }));
        }
        loose::read_loose_object::<F>(&self.objects_dir, id).await
    }

    /// Expand an abbreviated object ID by searching every pack index.
    ///
    /// Loose objects are not considered: finding one would mean knowing its
    /// full ID already (they are stored at `objects/ab/cdef…`, and a directory
    /// listing is not available over dumb HTTP — the transport this library
    /// exists to serve). Packed objects are the overwhelming majority in any
    /// repository that has been gc'd, and are the ones a published repository
    /// serves.
    pub async fn resolve_prefix(&self, prefix: &ObjectIdPrefix) -> OdbResult<PrefixResolution> {
        let mut resolution = PrefixResolution::NotFound;
        for pack in &self.index_cache.indexes {
            let idx_file = self.pack_dir.open_file(&pack.name.index_filename).await?;
            // Share the pack's persistent index page cache, as object lookups
            // do: the binary search walks the same pages they do.
            let mut idx_file = CachingPageReader::with_cache(idx_file, pack.idx_pages.clone());
            resolution = resolution
                .merge(find_prefix_in_pack_index(&pack.fanout, &mut idx_file, prefix).await?);
            if resolution == PrefixResolution::Ambiguous {
                // No later pack can un-ambiguate it, so stop reading.
                break;
            }
        }
        Ok(resolution)
    }

    async fn find_packed_object(
        &self,
        id: ObjectId,
    ) -> OdbResult<Option<(IndexedPackFile<'_, F::File>, Offset)>> {
        for pack in &self.index_cache.indexes {
            let idx_file = self.pack_dir.open_file(&pack.name.index_filename).await?;
            // Reuse the pack's persistent index page cache so binary-search
            // reads are shared across lookups; the pack body reader stays
            // per-lookup since body pages have little cross-lookup reuse.
            let mut idx_file = CachingPageReader::with_cache(idx_file, pack.idx_pages.clone());
            if let Some(offset) =
                find_object_in_pack_index(&pack.fanout, pack.offsets.as_ref(), &mut idx_file, id)
                    .await?
            {
                let pack_file = self.pack_dir.open_file(&pack.name.pack_filename).await?;
                return Ok(Some((
                    IndexedPackFile {
                        fanout: &pack.fanout,
                        offsets: pack.offsets.as_ref(),
                        index: idx_file,
                        pack: CachingPageReader::new(pack_file),
                    },
                    offset,
                )));
            }
        }
        Ok(None)
    }
}

/// Parse the `objects/info/packs` file written by `git update-server-info`.
///
/// Each line is `P <packfile-name>`.
fn parse_info_packs(data: &[u8]) -> OdbResult<Vec<PackName>> {
    let mut packs = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let name = line
            .strip_prefix(b"P ")
            .ok_or(OdbError::MalformedInfoPacks)?;
        packs
            .push(PackName::from_pack_filename(name.to_vec()).ok_or(OdbError::MalformedInfoPacks)?);
    }
    Ok(packs)
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use test_support::open_odb;

    #[test]
    fn parse_info_packs_lines() {
        let packs = parse_info_packs(b"P pack-0123abcd.pack\nP pack-fedcba98.pack\n\n").unwrap();
        assert_eq!(packs.len(), 2);
        assert_eq!(packs[0].pack_filename, b"pack-0123abcd.pack");
        assert_eq!(packs[0].index_filename, b"pack-0123abcd.idx");
        assert!(matches!(
            parse_info_packs(b"garbage\n"),
            Err(OdbError::MalformedInfoPacks)
        ));
    }

    /// Every object the odb hands back must hash to the ID it was asked for,
    /// including the ones rebuilt from a delta chain — the only path where the
    /// bytes are assembled rather than read straight out of the pack.
    #[test]
    fn packed_objects_hash_to_their_ids() {
        let test_repo = gib_testkit::make_basic_repo().unwrap();
        gib_testkit::make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc", "--aggressive"]).unwrap();

        let listing = test_repo
            .run_git([
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(deltabase)",
            ])
            .unwrap();

        let odb = open_odb(&test_repo);
        let (mut seen, mut deltified) = (0, 0);
        for line in listing.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let (name, base) = line.split_at(40);
            let id = ObjectId::from_hex(name).unwrap();
            let raw = block_on(odb.lookup(id)).unwrap().unwrap();
            raw.verify(id).unwrap();
            seen += 1;
            if base.trim_ascii() != b"0000000000000000000000000000000000000000" {
                deltified += 1;
            }
        }

        assert!(seen > 4, "expected a populated pack, saw {seen} objects");
        // Without a delta in the pack this would only be testing the plain
        // read path, which the loose-object tests already cover.
        assert!(deltified > 0, "expected the pack to contain a delta chain");
    }

    #[test]
    fn discover_packs_prefers_info_packs() {
        let test_repo = gib_testkit::make_packfile_repo().unwrap();
        test_repo.run_git(["update-server-info"]).unwrap();
        let objects_dir = test_support::objects_dir(&test_repo);
        let pack_dir = block_on(objects_dir.open_subdir(b"pack")).unwrap();
        let expected = block_on(ObjectDb::<gib_testkit::TestFileSystem>::discover_packs(
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
        let from_info = block_on(ObjectDb::<gib_testkit::TestFileSystem>::discover_packs(
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
        let test_repo = gib_testkit::make_packfile_repo().unwrap();
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
        let objects_dir = test_support::objects_dir(&test_repo);
        let pack_dir = block_on(objects_dir.open_subdir(b"pack")).unwrap();
        let listed = block_on(ObjectDb::<gib_testkit::TestFileSystem>::discover_packs(
            &objects_dir,
            &pack_dir,
        ))
        .unwrap();
        assert_eq!(listed.len(), 1);
    }

    /// A repository with no packs at all still opens and serves loose objects.
    #[test]
    fn loose_only_repository() {
        let test_repo = gib_testkit::make_basic_repo().unwrap();
        let odb = open_odb(&test_repo);
        let head = ObjectId::from_hex(
            test_repo
                .run_git(["rev-parse", "HEAD"])
                .unwrap()
                .trim_ascii_end(),
        )
        .unwrap();
        assert!(block_on(odb.lookup(head)).unwrap().is_some());
        // With no packs, abbreviations resolve to nothing at all.
        let prefix = ObjectIdPrefix::from_hex(&head.to_string().as_bytes()[..6]).unwrap();
        assert_eq!(
            block_on(odb.resolve_prefix(&prefix)).unwrap(),
            PrefixResolution::NotFound
        );
    }
}

#[cfg(test)]
mod differential;
