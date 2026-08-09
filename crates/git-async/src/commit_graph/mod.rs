//! Reader for git's commit-graph file (`objects/info/commit-graph`).
//!
//! The commit-graph is an optional cache that stores, for every reachable
//! commit, its root-tree id, parents, and commit time in a compact binary form
//! — so history can be walked without inflating and parsing a commit object per
//! step. When present with changed-path Bloom filters it also answers "did this
//! commit touch path P?" cheaply (see [`bloom`]).
//!
//! Only the single-file form is supported. A missing, split (`base graph
//! count > 0`), or otherwise unsupported/corrupt file makes [`CommitGraph::open`]
//! return `Ok(None)`, so callers transparently fall back to reading objects.
//!
//! Reads go through the same paged [`CachingPageReader`] used for pack indexes,
//! so only the touched 4 KiB windows of the file are fetched, and they are
//! shared across lookups for the lifetime of the graph.
//!
//! Reference: `gitformat-commit-graph(5)`.

// Chunk ids (oidf/oidl/bidx/bdat) and parent slots (parent1/parent2/parents)
// follow git's own names, which pedantic flags as too similar.
#![allow(clippy::similar_names)]

pub mod bloom;

use crate::{
    error::{Error, GResult},
    file_system::{Directory, File, FileSystem, FileSystemError, Offset},
    object::ObjectId,
};
use alloc::{vec, vec::Vec};
use bloom::BloomSettings;
use core::cmp::Ordering;
use gib_fs::{CachingPageReader, PageCache, new_page_cache};

/// A commit's parent that is absent (root commit, or the second slot of a
/// single-parent commit).
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
/// High bit on the second-parent slot: the remaining bits index the `EDGE`
/// chunk, which lists the third and further parents of an octopus merge.
const GRAPH_EXTRA_EDGES: u32 = 0x8000_0000;
/// Mask selecting a graph position out of a parent/edge word.
const GRAPH_POSITION_MASK: u32 = 0x7fff_ffff;

const OID_LEN: u64 = 20;
const CDAT_ENTRY_LEN: u64 = OID_LEN + 4 + 4 + 8;
const BLOOM_HEADER_LEN: usize = 12;

/// The metadata the commit-graph records for a single commit.
#[derive(Clone, Debug)]
pub struct CommitGraphEntry {
    /// The commit's root tree.
    pub tree: ObjectId,
    /// The commit's parents, in order.
    pub parents: Vec<ObjectId>,
    /// The committer timestamp, in seconds since the Unix epoch.
    pub commit_time: i64,
}

/// A parsed commit-graph file, ready for lookups.
///
/// Generic over the [`FileSystem`] like the rest of the crate. Holds only chunk
/// offsets, the fanout table, and a shared page cache; record bytes are read on
/// demand.
pub struct CommitGraph<F: FileSystem> {
    info_dir: F::Directory,
    pages: PageCache,
    /// `fanout[b]` = number of commits whose first oid byte is ≤ `b`.
    fanout: [u32; 256],
    num_commits: u32,
    oidl_offset: u64,
    cdat_offset: u64,
    edge_offset: Option<u64>,
    bidx_offset: Option<u64>,
    bdat_offset: Option<u64>,
    bloom: Option<BloomSettings>,
    /// Offset of the end of chunk data (= start of the trailing checksum),
    /// i.e. the byte length of everything worth reading. Used to warm the whole
    /// file in a single read for a bulk load.
    data_end: u64,
}

impl<F: FileSystem> CommitGraph<F> {
    /// Open and validate the commit-graph under `objects_dir/info`.
    ///
    /// Returns `Ok(None)` when there is no usable single-file commit-graph (file
    /// absent, wrong magic/version, the split form, or required chunks missing),
    /// so the caller can fall back to object reads. `Err` is reserved for I/O
    /// errors and structurally corrupt files.
    pub(crate) async fn open(objects_dir: &F::Directory) -> GResult<Option<Self>> {
        let info_dir = match objects_dir.open_subdir(b"info").await {
            Ok(dir) => dir,
            Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let pages = new_page_cache();
        let file = match info_dir.open_file(b"commit-graph").await {
            Ok(file) => file,
            Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut reader = CachingPageReader::with_cache(file, pages.clone());

        // Header: "CGPH", version, hash version, chunk count, base graph count.
        let mut header = [0u8; 8];
        match reader.read_segment(Offset(0), &mut header).await {
            Ok(8) => {}
            // Too short to be a header, or the file is absent: fall back.
            Ok(_) | Err(FileSystemError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        if &header[0..4] != b"CGPH" || header[4] != 1 || header[5] != 1 {
            // Not a commit-graph, an unsupported format version, or a non-SHA-1
            // hash: fall back rather than misread.
            return Ok(None);
        }
        let num_chunks = header[6];
        let base_graph_count = header[7];
        if base_graph_count != 0 {
            // Split/chained commit-graph: unsupported, fall back.
            return Ok(None);
        }

        // Chunk table of contents: (num_chunks + 1) entries of (4-byte id,
        // 8-byte offset); the final terminating entry marks end-of-chunks.
        let table_len = (usize::from(num_chunks) + 1) * 12;
        let table = read_vec(&mut reader, 8, table_len).await?;
        let (mut oidf, mut oidl, mut cdat) = (None, None, None);
        let (mut edge, mut bidx, mut bdat) = (None, None, None);
        // The terminating TOC entry's offset marks the end of all chunk data
        // (the trailing checksum follows); it is the largest offset in the table.
        let mut data_end = 0u64;
        for entry in table.chunks_exact(12) {
            let id: [u8; 4] = entry[0..4].try_into().unwrap();
            let offset = u64::from_be_bytes(entry[4..12].try_into().unwrap());
            data_end = data_end.max(offset);
            match &id {
                b"OIDF" => oidf = Some(offset),
                b"OIDL" => oidl = Some(offset),
                b"CDAT" => cdat = Some(offset),
                b"EDGE" => edge = Some(offset),
                b"BIDX" => bidx = Some(offset),
                b"BDAT" => bdat = Some(offset),
                _ => {}
            }
        }
        let (Some(oidf_offset), Some(oidl_offset), Some(cdat_offset)) = (oidf, oidl, cdat) else {
            return Ok(None);
        };

        // Fanout: 256 big-endian u32 cumulative counts; the last is the total.
        let fanout_bytes = read_vec(&mut reader, oidf_offset, 256 * 4).await?;
        let mut fanout = [0u32; 256];
        for (slot, chunk) in fanout.iter_mut().zip(fanout_bytes.chunks_exact(4)) {
            *slot = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        let num_commits = fanout[255];

        // Changed-path Bloom filters are usable only with both chunks present.
        let bloom = if let (Some(_), Some(bdat_offset)) = (bidx, bdat) {
            let hdr = read_vec(&mut reader, bdat_offset, BLOOM_HEADER_LEN).await?;
            Some(BloomSettings {
                hash_version: u32::from_be_bytes(hdr[0..4].try_into().unwrap()),
                num_hashes: u32::from_be_bytes(hdr[4..8].try_into().unwrap()),
                bits_per_entry: u32::from_be_bytes(hdr[8..12].try_into().unwrap()),
            })
        } else {
            None
        };

        Ok(Some(Self {
            info_dir,
            pages,
            fanout,
            num_commits,
            oidl_offset,
            cdat_offset,
            edge_offset: edge,
            bidx_offset: bidx,
            bdat_offset: bdat,
            bloom,
            data_end,
        }))
    }

    /// The number of commits recorded in the graph.
    pub fn num_commits(&self) -> u32 {
        self.num_commits
    }

    /// Whether the graph carries changed-path Bloom filters.
    pub fn has_bloom(&self) -> bool {
        self.bloom.is_some()
    }

    /// The changed-path Bloom filter settings, if the graph has filters. Callers
    /// that cache filter bytes should tag them with these, since a filter is only
    /// interpretable under the settings it was written with.
    pub fn bloom_settings(&self) -> Option<BloomSettings> {
        self.bloom
    }

    /// Look up a commit's position and metadata in one pass, reusing a single
    /// page reader. Returns `None` if the commit is not in the graph.
    pub async fn lookup(&self, id: ObjectId) -> GResult<Option<(u32, CommitGraphEntry)>> {
        let mut reader = self.reader().await?;
        let Some(pos) = self.position_of(&mut reader, id).await? else {
            return Ok(None);
        };
        let entry = self.commit_data(&mut reader, pos).await?;
        Ok(Some((pos, entry)))
    }

    /// Everything the walk needs for one commit: its metadata and changed-path
    /// Bloom filter bytes (`None` if the graph has no filters or the commit's is
    /// empty). Returns `None` if the commit is not in the graph. Resolves with a
    /// single page reader.
    pub async fn record(
        &self,
        id: ObjectId,
    ) -> GResult<Option<(CommitGraphEntry, Option<Vec<u8>>)>> {
        let mut reader = self.reader().await?;
        let Some(pos) = self.position_of(&mut reader, id).await? else {
            return Ok(None);
        };
        let entry = self.commit_data(&mut reader, pos).await?;
        let bloom = self.changed_path_filter(&mut reader, pos).await?;
        Ok(Some((entry, bloom)))
    }

    /// Read every commit's `(oid, metadata, Bloom bytes)` in one pass. The whole
    /// file is warmed with a single read up front, so this issues ~one request
    /// regardless of commit count — the basis for seeding a persistent per-commit
    /// cache.
    pub async fn all_records(&self) -> GResult<Vec<(ObjectId, CommitGraphEntry, Option<Vec<u8>>)>> {
        let mut reader = self.reader().await?;
        // Warm the entire file in one read so the per-commit reads below are all
        // served from the page cache.
        if self.data_end > 0 {
            let mut whole = vec![0u8; usize::try_from(self.data_end).unwrap()];
            reader.read_segment(Offset(0), &mut whole).await?;
        }
        let mut out = Vec::with_capacity(usize::try_from(self.num_commits).unwrap());
        for pos in 0..self.num_commits {
            let oid = self.oid_at(&mut reader, pos).await?;
            let entry = self.commit_data(&mut reader, pos).await?;
            let bloom = self.changed_path_filter(&mut reader, pos).await?;
            out.push((oid, entry, bloom));
        }
        Ok(out)
    }

    /// Whether the commit at `pos` definitively did **not** change `path`,
    /// according to its changed-path Bloom filter (a first-parent diff).
    ///
    /// `false` means "unknown" — no Bloom data, or the filter reports a possible
    /// match — and the caller must confirm with a real diff. `true` is only ever
    /// returned when the filter is conclusive, so it is always safe to skip the
    /// commit.
    pub async fn path_unchanged(&self, pos: u32, path: &[u8]) -> GResult<bool> {
        let Some(settings) = self.bloom else {
            return Ok(false);
        };
        let mut reader = self.reader().await?;
        let Some(filter) = self.changed_path_filter(&mut reader, pos).await? else {
            return Ok(false);
        };
        Ok(!bloom::path_maybe_changed(&filter, &settings, path))
    }

    async fn reader(&self) -> GResult<CachingPageReader<F::File>> {
        let file = self.info_dir.open_file(b"commit-graph").await?;
        Ok(CachingPageReader::with_cache(file, self.pages.clone()))
    }

    /// Binary-search the sorted `OIDL` chunk for `id`, bounded by the fanout.
    /// Mirrors [`crate::object_store::index`]'s pack-index search.
    async fn position_of(
        &self,
        reader: &mut CachingPageReader<F::File>,
        id: ObjectId,
    ) -> GResult<Option<u32>> {
        let first = id.bytes()[0];
        let mut lower = if first == 0 {
            0
        } else {
            self.fanout[usize::from(first - 1)]
        };
        let mut upper = self.fanout[usize::from(first)];
        while lower < upper {
            let mid = u32::midpoint(lower, upper);
            let oid = self.oid_at(reader, mid).await?;
            match oid.bytes().cmp(id.bytes()) {
                Ordering::Equal => return Ok(Some(mid)),
                Ordering::Less => lower = mid + 1,
                Ordering::Greater => upper = mid,
            }
        }
        Ok(None)
    }

    async fn oid_at(&self, reader: &mut CachingPageReader<F::File>, pos: u32) -> GResult<ObjectId> {
        let buf: [u8; 20] = read_array(reader, self.oidl_offset + u64::from(pos) * OID_LEN).await?;
        Ok(ObjectId::from_bytes(buf))
    }

    async fn commit_data(
        &self,
        reader: &mut CachingPageReader<F::File>,
        pos: u32,
    ) -> GResult<CommitGraphEntry> {
        let rec: [u8; 36] =
            read_array(reader, self.cdat_offset + u64::from(pos) * CDAT_ENTRY_LEN).await?;
        let tree = ObjectId::from_bytes(rec[0..20].try_into().unwrap());
        let parent1 = u32::from_be_bytes(rec[20..24].try_into().unwrap());
        let parent2 = u32::from_be_bytes(rec[24..28].try_into().unwrap());
        // The trailing 8 bytes pack a 30-bit generation number (high) and a
        // 34-bit commit time (low); only the time is needed for ordering.
        let word0 = u32::from_be_bytes(rec[28..32].try_into().unwrap());
        let word1 = u32::from_be_bytes(rec[32..36].try_into().unwrap());
        let commit_time = ((i64::from(word0 & 0x3)) << 32) | i64::from(word1);

        let mut parents = Vec::new();
        if parent1 != GRAPH_PARENT_NONE {
            parents.push(self.oid_at(reader, parent1).await?);
            if parent2 != GRAPH_PARENT_NONE {
                if parent2 & GRAPH_EXTRA_EDGES != 0 {
                    self.read_extra_edges(reader, parent2 & GRAPH_POSITION_MASK, &mut parents)
                        .await?;
                } else {
                    parents.push(self.oid_at(reader, parent2).await?);
                }
            }
        }
        Ok(CommitGraphEntry {
            tree,
            parents,
            commit_time,
        })
    }

    /// Append an octopus merge's third-and-later parents from the `EDGE` chunk.
    /// Each entry is a position; the final entry of the run has its high bit set.
    async fn read_extra_edges(
        &self,
        reader: &mut CachingPageReader<F::File>,
        start: u32,
        parents: &mut Vec<ObjectId>,
    ) -> GResult<()> {
        let edge_offset = self.edge_offset.ok_or(Error::CorruptCommitGraph)?;
        let mut index = start;
        loop {
            let raw =
                u32::from_be_bytes(read_array(reader, edge_offset + u64::from(index) * 4).await?);
            parents.push(self.oid_at(reader, raw & GRAPH_POSITION_MASK).await?);
            if raw & GRAPH_EXTRA_EDGES != 0 {
                return Ok(());
            }
            index += 1;
        }
    }

    /// The raw changed-path Bloom filter bytes for the commit at `pos`, or
    /// `None` when there are no Bloom chunks or the filter is empty (which the
    /// format uses for "not recorded" — callers treat it as "maybe").
    async fn changed_path_filter(
        &self,
        reader: &mut CachingPageReader<F::File>,
        pos: u32,
    ) -> GResult<Option<Vec<u8>>> {
        let (Some(bidx_offset), Some(bdat_offset)) = (self.bidx_offset, self.bdat_offset) else {
            return Ok(None);
        };
        let end = u32::from_be_bytes(read_array(reader, bidx_offset + u64::from(pos) * 4).await?);
        let start = if pos == 0 {
            0
        } else {
            u32::from_be_bytes(read_array(reader, bidx_offset + u64::from(pos - 1) * 4).await?)
        };
        if end <= start {
            return Ok(None);
        }
        let data_offset = bdat_offset + BLOOM_HEADER_LEN as u64 + u64::from(start);
        let filter = read_vec(reader, data_offset, (end - start) as usize).await?;
        Ok(Some(filter))
    }
}

/// Read exactly `N` bytes at `offset`, erroring if the file ends short.
async fn read_array<const N: usize, R: File>(reader: &mut R, offset: u64) -> GResult<[u8; N]> {
    let mut buf = [0u8; N];
    let read = reader.read_segment(Offset(offset), &mut buf).await?;
    if read < N {
        return Err(Error::CorruptCommitGraph);
    }
    Ok(buf)
}

/// Read exactly `len` bytes at `offset`, erroring if the file ends short.
async fn read_vec<R: File>(reader: &mut R, offset: u64, len: usize) -> GResult<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let read = reader.read_segment(Offset(offset), &mut buf).await?;
    if read < len {
        return Err(Error::CorruptCommitGraph);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{impls::TestFileSystem, repo::TestRepo};
    use futures::executor::block_on;
    use std::fs;

    type TestGraph = CommitGraph<TestFileSystem>;

    /// Oids of the commits built by [`graph_repo`].
    struct Oids {
        c2: ObjectId,
        c3: ObjectId,
        c4: ObjectId,
        c5: ObjectId,
        b1: ObjectId,
        b2: ObjectId,
        merge: ObjectId,
    }

    /// Build a repo whose history exercises root, normal, and octopus-merge
    /// commits, then write a single-file commit-graph with changed-path filters.
    fn graph_repo() -> (TestRepo, Oids) {
        let repo = TestRepo::new().unwrap();
        let root = repo.location.path().to_path_buf();
        fs::create_dir(root.join("src")).unwrap();

        let head = || {
            let out = repo.run_git(["rev-parse", "HEAD"]).unwrap();
            ObjectId::from_hex(out.trim_ascii_end()).unwrap()
        };
        let commit = |msg: &str| {
            repo.run_git(["add", "--all"]).unwrap();
            repo.commit(msg, "a user", "an-email-address", "2000-01-01T00:00:00Z")
                .unwrap();
            head()
        };

        fs::write(root.join("README"), "a\n").unwrap();
        fs::write(root.join("src/route.rs"), "x\n").unwrap();
        commit("c1");
        fs::write(root.join("src/route.rs"), "x\ny\n").unwrap();
        let c2 = commit("c2");
        fs::write(root.join("README"), "a\nb\n").unwrap();
        let c3 = commit("c3");
        // A non-ASCII path name exercises version-1 sign-extended hashing.
        fs::write(root.join("café.txt"), "z\n").unwrap();
        let c4 = commit("c4");

        // Two side branches off c4, each adding a distinct file...
        repo.run_git(["checkout", "-q", "-b", "b1"]).unwrap();
        fs::write(root.join("f1"), "1\n").unwrap();
        let b1 = commit("on b1");
        repo.run_git(["checkout", "-q", "main"]).unwrap();
        repo.run_git(["checkout", "-q", "-b", "b2"]).unwrap();
        fs::write(root.join("f2"), "2\n").unwrap();
        let b2 = commit("on b2");
        // ...plus a commit on main so it isn't a fast-forward of either branch,
        // forcing a real 3-parent octopus merge (which uses the EDGE chunk).
        repo.run_git(["checkout", "-q", "main"]).unwrap();
        fs::write(root.join("f3"), "3\n").unwrap();
        let c5 = commit("c5");
        repo.run_git(["merge", "--no-edit", "b1", "b2"]).unwrap();
        let merge = head();

        repo.run_git(["commit-graph", "write", "--reachable", "--changed-paths"])
            .unwrap();
        (
            repo,
            Oids {
                c2,
                c3,
                c4,
                c5,
                b1,
                b2,
                merge,
            },
        )
    }

    fn graph(repo: &TestRepo) -> TestGraph {
        // Repo::open also loads the graph; assert that wiring works, then open a
        // standalone instance for direct testing of the lower-level methods.
        assert!(repo.repo().commit_graph().is_some());
        let objects_dir = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
        block_on(CommitGraph::open(&objects_dir))
            .unwrap()
            .expect("repo should have a commit-graph")
    }

    #[test]
    fn lookup_matches_commit_objects() {
        let (repo, oids) = graph_repo();
        let cg = graph(&repo);
        let backing = repo.repo();
        assert!(cg.has_bloom());

        for id in [
            oids.c2, oids.c3, oids.c4, oids.c5, oids.b1, oids.b2, oids.merge,
        ] {
            let (_pos, entry) = block_on(cg.lookup(id)).unwrap().expect("in graph");
            let commit = block_on(backing.lookup_object(id))
                .unwrap()
                .commit()
                .unwrap();
            assert_eq!(entry.tree, commit.tree(), "tree of {id}");
            assert_eq!(entry.parents, commit.parents().to_vec(), "parents of {id}");
            assert_eq!(
                entry.commit_time,
                commit.commit_date().timestamp().as_second(),
                "time of {id}"
            );
        }
    }

    #[test]
    fn octopus_merge_parents() {
        let (repo, oids) = graph_repo();
        let cg = graph(&repo);
        let (_pos, entry) = block_on(cg.lookup(oids.merge)).unwrap().unwrap();
        // c5, b1 tip, b2 tip — the third parent is only reachable via EDGE.
        assert_eq!(entry.parents, vec![oids.c5, oids.b1, oids.b2]);
    }

    #[test]
    fn all_records_covers_every_commit() {
        let (repo, oids) = graph_repo();
        let cg = graph(&repo);
        let records = block_on(cg.all_records()).unwrap();
        assert_eq!(records.len() as u32, cg.num_commits());

        // Each bulk record must agree with the single-commit lookup path.
        for id in [oids.c2, oids.c4, oids.merge] {
            let (bulk_entry, bulk_bloom) = records
                .iter()
                .find(|(o, ..)| *o == id)
                .map(|(_, e, b)| (e, b))
                .unwrap();
            let (entry, bloom) = block_on(cg.record(id)).unwrap().unwrap();
            assert_eq!(bulk_entry.tree, entry.tree);
            assert_eq!(bulk_entry.parents, entry.parents);
            assert_eq!(bulk_entry.commit_time, entry.commit_time);
            assert_eq!(*bulk_bloom, bloom);
        }
        // The octopus merge's third parent must survive the bulk read too.
        let (_, merge_entry, _) = records.iter().find(|(o, ..)| *o == oids.merge).unwrap();
        assert_eq!(merge_entry.parents, vec![oids.c5, oids.b1, oids.b2]);
    }

    #[test]
    fn unknown_commit_is_none() {
        let (repo, _) = graph_repo();
        let cg = graph(&repo);
        assert!(
            block_on(cg.lookup(ObjectId::from_bytes([0xab; 20])))
                .unwrap()
                .is_none()
        );
    }

    fn assert_changed(cg: &TestGraph, id: ObjectId, path: &str) {
        let (pos, _) = block_on(cg.lookup(id)).unwrap().unwrap();
        assert!(
            !block_on(cg.path_unchanged(pos, path.as_bytes())).unwrap(),
            "{path} should read as maybe-changed"
        );
    }
    fn assert_unchanged(cg: &TestGraph, id: ObjectId, path: &str) {
        let (pos, _) = block_on(cg.lookup(id)).unwrap().unwrap();
        assert!(
            block_on(cg.path_unchanged(pos, path.as_bytes())).unwrap(),
            "{path} should read as definitely-unchanged"
        );
    }

    #[test]
    fn bloom_matches_real_diffs() {
        let (repo, oids) = graph_repo();
        let cg = graph(&repo);

        // c2 modified only src/route.rs.
        assert_changed(&cg, oids.c2, "src/route.rs");
        assert_changed(&cg, oids.c2, "src"); // parent-directory query
        assert_unchanged(&cg, oids.c2, "README");

        // c3 modified only README.
        assert_changed(&cg, oids.c3, "README");
        assert_unchanged(&cg, oids.c3, "src/route.rs");

        // c4 added a non-ASCII path: validates version-1 sign-extended murmur3.
        assert_changed(&cg, oids.c4, "café.txt");
        assert_unchanged(&cg, oids.c4, "README");
    }

    /// Cross-check against `git log --format=%H -- <path>`: every commit git
    /// reports for the path must be flagged maybe-changed by the filter (no
    /// false negatives, which would silently drop commits from a per-file log).
    #[test]
    fn bloom_has_no_false_negatives() {
        let (repo, _) = graph_repo();
        let cg = graph(&repo);
        for path in ["src/route.rs", "README", "café.txt", "f1", "f3", "src"] {
            let log = repo.run_git(["log", "--format=%H", "--", path]).unwrap();
            for line in log.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                let id = ObjectId::from_hex(line).unwrap();
                let Some((pos, _)) = block_on(cg.lookup(id)).unwrap() else {
                    continue;
                };
                assert!(
                    !block_on(cg.path_unchanged(pos, path.as_bytes())).unwrap(),
                    "false negative: git says {path} changed in {id} but filter hid it"
                );
            }
        }
    }
}
