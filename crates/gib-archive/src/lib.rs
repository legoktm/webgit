//! Building the contents of a `git archive`, without a server to run it on.
//!
//! A tree is walked object by object — every blob fetched through whatever the
//! caller supplies as an [`ObjectSource`], and every `.gitattributes` along the
//! way consulted for `export-ignore` — and the entries that come out are
//! written into a tar by [`TarWriter`]. The tar half is deliberately
//! byte-for-byte what `git archive --format=tar` writes for the same commit:
//! same mode normalisation, same `pax_global_header` carrying the commit id,
//! same record padding, so an archive taken here is interchangeable with one
//! taken by git. `test_matches_git_archive` holds that claim to a real
//! `git archive` invocation.
//!
//! Neither half does any IO or any compression. Objects arrive through the
//! caller's [`ObjectSource`], which is what lets the walk overlap its fetches
//! without knowing where they come from, and the tar is handed back in pieces
//! for the caller to feed to whatever encoder it has — in webgit, the browser's
//! own `CompressionStream`.

#![deny(clippy::all)]

use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, select};
use futures::stream::{FuturesUnordered, StreamExt};
use gib_attributes::{AttributesFile, GITATTRIBUTES, Stack};
use gib_object::{Object, ObjectId, Tree, TreeEntryType, UnexpectedObjectType};
use std::collections::VecDeque;

#[cfg(test)]
mod differential;
mod writer;

pub use writer::TarWriter;

/// The most file content one archive may hold, uncompressed.
///
/// Everything — the blobs, the tar, and the compressed output — is live in the
/// caller's memory at once, and in a wasm tab that memory is the tab's: a big
/// enough repository doesn't merely take a while, it takes the tab down. This
/// cap turns that into an error message suggesting the obvious alternative
/// (cloning), well before the allocator starts refusing. It counts blob bytes, the part that scales with the
/// repository; the tar's own per-entry overhead is bounded by the file count.
pub const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

/// One entry to write into the archive: a directory, a file, or a symlink.
///
/// A submodule (gitlink) is a [`Directory`] like any other. It has no content
/// to archive — the commit it names lives in a repository we don't have — but
/// `git archive` still writes the empty directory, so the tarball has
/// somewhere to check the submodule out into.
///
/// [`Directory`]: EntryKind::Directory
#[derive(Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    /// A regular file and whether git recorded it executable.
    File {
        executable: bool,
    },
    /// A symlink; the payload is its target, which git stores as blob content.
    Symlink {
        target: Vec<u8>,
    },
}

/// A single archive member: its path relative to the archive's prefix
/// directory, what it is, and (for files) its bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// Path within the archive, below the prefix directory. Directories do not
    /// carry the trailing slash; [`TarWriter::append`] adds it.
    pub path: String,
    pub kind: EntryKind,
    /// File content. Empty for directories and symlinks, whose target lives in
    /// [`EntryKind::Symlink`].
    pub data: Vec<u8>,
}

/// How many blob fetches the walk keeps in flight at once.
///
/// The walk queues work as it discovers it rather than waiting for each fetch,
/// so without a cap a large repository would hand the browser one request per
/// file at once. 48 is the same reasoning as the log walk's `CONFIRM_BATCH`:
/// comfortably under the ~100–128 concurrent streams servers allow on HTTP/2,
/// so the requests are multiplexed rather than queued behind each other.
const MAX_IN_FLIGHT: usize = 48;

/// How many subtree fetches the walk keeps in flight at once.
///
/// Deliberately a *separate* budget rather than a share of [`MAX_IN_FLIGHT`].
/// Blobs are leaves — fetching one reveals nothing — while every subtree that
/// lands is what makes more blobs queueable, so subtree fetches competing with
/// blobs for the same slots throttles discovery and starves the walk of work.
/// Simulated against this repository's mediawiki test fixture (13,658 objects),
/// one shared budget of 48 took ~8× as many round trips as two of 48.
const MAX_TREES_IN_FLIGHT: usize = 48;

/// The attribute that keeps a path out of an archive.
///
/// TODO: `export-subst`, the other attribute `git archive` reads, expands
/// `$Format:...$` placeholders in a file's content as it writes it. Nothing
/// here looks at it, so a file carrying one is archived with the placeholder
/// still in it.
const EXPORT_IGNORE: &str = "export-ignore";

/// A directory the walk has discovered but not yet read.
struct Pending {
    /// Index into [`Walk::frames`] of the frame its contents belong in.
    frame: usize,
    /// The path it was reached by, which names it in errors and prefixes
    /// everything inside it.
    path: String,
    /// The attributes files covering it: its parent's stack, plus its own
    /// `.gitattributes` once that has been read.
    attrs: Stack,
}

/// A blob fetch in flight: the slot its bytes belong in, and the result.
type BlobFetch<'a> = LocalBoxFuture<'a, (usize, anyhow::Result<Vec<u8>>)>;
/// Every directory's blob fetches, in one pool so they overlap across the tree.
type BlobPool<'a> = FuturesUnordered<BlobFetch<'a>>;
/// A subtree fetch in flight: the directory it will fill in, and the result.
type TreeFetch<'a> = LocalBoxFuture<'a, (Pending, anyhow::Result<Tree>)>;
/// Every directory's subtree fetches, in one pool: the walk expands whichever
/// lands first, which is what keeps discovery ahead of the blob pool.
type TreePool<'a> = FuturesUnordered<TreeFetch<'a>>;

/// Where the walk reads objects from.
///
/// The crate does no IO of its own: this is the seam the caller reaches its
/// object store through. webgit implements it over the browser's caching repo;
/// the tests implement it over a map in memory, which is what lets the walk's
/// ordering — the thing most worth testing — be exercised natively.
pub trait ObjectSource {
    /// Read one object, by id.
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>>;
}

/// The archive's shape as the walk discovers it.
///
/// Nothing is stored inline: a file node holds the slot its fetch will land in
/// and a directory node the frame its contents will be built in, so the walk
/// never has to wait for anything just to keep going. Both are filled in later,
/// and the order they are filled in doesn't matter — [`flatten`] reads the
/// finished structure, which is what makes the output independent of the order
/// fetches complete in.
enum Node {
    Dir {
        path: String,
        /// Index into [`Walk::frames`] of this directory's own children.
        frame: usize,
    },
    File {
        path: String,
        executable: bool,
        slot: usize,
    },
    Symlink {
        path: String,
        slot: usize,
    },
}

/// How far along the walk is: objects fetched, out of objects requested so far.
struct Progress<'a> {
    fetched: usize,
    queued: usize,
    /// Called on every change, with `(fetched, queued)`
    report: &'a dyn Fn(usize, usize),
}

impl Progress<'_> {
    /// Note that `n` more objects have been asked for.
    fn queued(&mut self, n: usize) {
        self.queued += n;
        self.emit();
    }

    /// Note that one object arrived.
    fn landed(&mut self) {
        self.fetched += 1;
        self.emit();
    }

    fn emit(&self) {
        (self.report)(self.fetched, self.queued);
    }
}

/// Everything the walk shares across frames: where objects come from, the blob
/// fetches in flight, the slots their bytes land in, and the running totals.
///
/// One struct rather than a fistful of `&mut` parameters threaded through every
/// step — each of them needs all of it, and holding it together keeps the fetch
/// bookkeeping (slot filling, the size cap, the progress counters) in one place
/// instead of at each call site.
struct Walk<'a, S: ObjectSource> {
    repo: &'a S,
    /// Blob fetches from every directory, in one pool so they overlap across
    /// the tree: a directory of one file doesn't throttle the walk down to a
    /// single request at a time.
    blobs: BlobPool<'a>,
    /// Subtree fetches in flight, likewise pooled across the whole tree.
    trees: TreePool<'a>,
    /// Subtrees discovered but not yet requested, held back by the tree budget.
    /// A backlog rather than a blocking queue because the only way to make room
    /// in the tree pool is to expand what is in it, which is the walk's job and
    /// not something a directory being read can do halfway through.
    backlog: VecDeque<(Pending, ObjectId)>,
    /// Subtrees that arrived while the walk was waiting on a blob. Expanding
    /// one means reading a directory, which is what the caller is already in
    /// the middle of, so they are set aside for the main loop to pick up.
    landed: VecDeque<(Pending, Tree)>,
    /// One entry per directory: its children in tree order. `Node::Dir` names
    /// the frame holding its contents, so a directory can be filled in whenever
    /// its fetch happens to land, with no unwinding and no parent bookkeeping.
    frames: Vec<Vec<Node>>,
    /// One slot per blob, in the order the walk queued them, filled in as the
    /// fetches land.
    fetched: Vec<Option<Vec<u8>>>,
    /// Blob bytes accumulated so far, against [`MAX_ARCHIVE_BYTES`].
    bytes: usize,
    progress: Progress<'a>,
}

/// Walk `tree`, fetching every blob, and return the entries to archive in
/// `git archive` order: a directory immediately before its contents, siblings
/// in git's own tree order (which the tree object already stores them in, so
/// nothing is re-sorted here).
///
/// Fetches are issued as the walk finds them, not in the order results are
/// needed, and directories are expanded breadth-first: whichever subtree lands
/// first is read next, wherever it sits in the tree. That is the opposite of
/// the order the output is in, and deliberately so. Depth-first expansion can
/// only uncover one directory at a time along a single spine, which discovers
/// blobs far too slowly to keep `MAX_IN_FLIGHT` of them in flight — simulated
/// against this repository's mediawiki test fixture it managed 31 of the 48,
/// and raising the cap barely helped because discovery, not the cap, was the
/// limit. Breadth-first fills the pool and keeps it full.
///
/// None of that is observable in the result: the output order is fixed by the
/// node tree, not by completion or expansion order, which is what keeps the
/// archive byte-identical to `git archive` regardless of what finishes when.
///
/// A `.gitattributes` in any directory is read before that directory's entries
/// are queued, and anything it marks `export-ignore` is left out — a file is
/// never fetched, and a directory is never even walked into. That is one extra
/// round trip in each directory carrying such a file, and the reason the check
/// is not simply a filter over the finished entry list: the point of it is to
/// not fetch what is not going to be archived.
///
/// `on_progress` is called with `(fetched, queued)` object counts as they
/// change; see `Progress` for why the second number is not a fixed total.
/// Attributes files count towards both, being objects the walk asks for.
///
/// Errors if the accumulated file content exceeds [`MAX_ARCHIVE_BYTES`].
///
/// Paths are decoded as UTF-8, lossily, matching how the tree view renders
/// them: the tar crate cannot encode a non-UTF-8 path on a target without
/// `OsStrExt` (which wasm is), and dropping the file outright would be a worse
/// answer than archiving it under a replacement-character name.
pub async fn collect_entries<'a, S: ObjectSource>(
    repo: &'a S,
    tree: &Tree,
    prefix: &str,
    on_progress: &'a dyn Fn(usize, usize),
) -> anyhow::Result<Vec<ArchiveEntry>> {
    let mut walk = Walk {
        repo,
        blobs: FuturesUnordered::new(),
        trees: FuturesUnordered::new(),
        backlog: VecDeque::new(),
        landed: VecDeque::new(),
        // Frame 0 is the directory being archived; every other frame is created
        // when the subtree that fills it is discovered.
        frames: vec![Vec::new()],
        fetched: Vec::new(),
        bytes: 0,
        progress: Progress {
            fetched: 0,
            queued: 0,
            report: on_progress,
        },
    };

    walk.read_dir(tree, prefix, 0, Stack::new()).await?;
    loop {
        let next = walk.next_tree().await?;
        let Some((dir, subtree)) = next else {
            break;
        };
        walk.read_dir(&subtree, &dir.path, dir.frame, dir.attrs)
            .await?;
    }
    walk.drain().await?;

    let mut out = Vec::with_capacity(walk.fetched.len());
    let root = std::mem::take(&mut walk.frames[0]);
    flatten(root, &mut walk.frames, &mut walk.fetched, &mut out);
    Ok(out)
}

impl<'a, S: ObjectSource> Walk<'a, S> {
    /// Read one directory into frame `frame`: queue a fetch for everything in
    /// it, and record the nodes its entries become.
    ///
    /// Blobs go straight into the shared pool; subtrees go onto the backlog and
    /// are issued from there as the tree budget allows. By the time this
    /// returns, all of the directory's work is either in flight or waiting its
    /// turn.
    ///
    /// `attrs` is the attributes stack the directory inherits; its own
    /// `.gitattributes`, if it has one, is read before anything else here and
    /// can keep entries out of the archive entirely.
    async fn read_dir(
        &mut self,
        tree: &Tree,
        prefix: &str,
        frame: usize,
        attrs: Stack,
    ) -> anyhow::Result<()> {
        // Copied out so the futures pushed below borrow the source rather than
        // `self`, which the pools they go into are part of.
        let repo = self.repo;
        let mut children = Vec::new();
        let (attrs, mut attributes_blob) = self.read_attributes(tree, prefix, attrs).await?;

        for entry in tree.entries() {
            let name = String::from_utf8_lossy(entry.name()).into_owned();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            // A gitlink counts as a directory here: git appends the trailing
            // slash before looking its attributes up (`archive.c`,
            // `write_archive_entry`), exactly as it does for a real one, so a
            // `vendor/` pattern has to find a submodule named `vendor` too.
            let is_dir = matches!(
                entry.entry_type(),
                TreeEntryType::Tree | TreeEntryType::Commit
            );
            // An ignored directory is pruned rather than walked: `git archive`
            // never looks inside one, and neither does this — which is the
            // point, since not fetching what won't be archived is the whole
            // reason to consult the attributes before queueing anything.
            if attrs.check(&path, is_dir, EXPORT_IGNORE).is_set() {
                continue;
            }
            match entry.entry_type() {
                TreeEntryType::Tree => {
                    let child = self.frames.len();
                    self.frames.push(Vec::new());
                    children.push(Node::Dir {
                        path: path.clone(),
                        frame: child,
                    });
                    self.backlog.push_back((
                        Pending {
                            frame: child,
                            path,
                            attrs: attrs.clone(),
                        },
                        entry.id(),
                    ));
                    self.issue_trees();
                    self.progress.queued(1);
                }
                // A submodule is a commit id pointing into a repository we
                // don't have, so there is nothing to walk into and nothing to
                // fetch — it never counts towards the progress. git still
                // writes the directory entry itself, with the same mode a real
                // directory gets (`archive-tar.c`, `write_tar_entry`, where
                // `S_ISGITLINK` shares the `TYPEFLAG_DIR` branch), so an empty
                // frame stands in for the contents we can't have.
                TreeEntryType::Commit => {
                    let child = self.frames.len();
                    self.frames.push(Vec::new());
                    children.push(Node::Dir { path, frame: child });
                }
                TreeEntryType::Symlink | TreeEntryType::File | TreeEntryType::Executable => {
                    let slot = self.fetched.len();
                    self.fetched.push(None);
                    children.push(match entry.entry_type() {
                        TreeEntryType::Symlink => Node::Symlink { path, slot },
                        kind => Node::File {
                            path,
                            executable: kind == TreeEntryType::Executable,
                            slot,
                        },
                    });
                    // The attributes file was fetched above to decide this
                    // directory's entries, and is an entry itself: its bytes go
                    // straight into their slot rather than being asked for a
                    // second time.
                    if entry.name() == GITATTRIBUTES.as_bytes()
                        && let Some(data) = attributes_blob.take()
                    {
                        self.fill_slot(slot, data)?;
                        continue;
                    }
                    // Wait for room before adding to the pool, so the number of
                    // outstanding requests stays bounded however wide the tree
                    // is. This is also the walk's throttle: expanding stops
                    // here until blobs land, which is what stops discovery from
                    // running arbitrarily far ahead of the fetching.
                    // Draining the tree pool as well as the blob pool is what
                    // keeps freshly queued subtree fetches from sitting
                    // unpolled — and so unsent — through the wait.
                    while self.blobs.len() >= MAX_IN_FLIGHT {
                        self.drain_one().await?;
                    }
                    let id = entry.id();
                    self.blobs.push(
                        async move {
                            let data = async {
                                Ok(repo
                                    .object(id)
                                    .await?
                                    .blob()
                                    .map_err(wrong_type)?
                                    .data_owned())
                            }
                            .await;
                            (slot, data)
                        }
                        .boxed_local(),
                    );
                    self.progress.queued(1);
                }
            }
        }

        self.frames[frame] = children;
        Ok(())
    }

    /// Read this directory's own `.gitattributes`, if it has one, and return
    /// the stack that applies inside it along with the file's bytes.
    ///
    /// The bytes come back because the attributes file is also an ordinary file
    /// in the archive, and fetching it twice would be silly.
    ///
    /// A `.gitattributes` that is a symlink is left alone, as git leaves it
    /// alone: it reads the file, not what it might point at.
    async fn read_attributes(
        &mut self,
        tree: &Tree,
        prefix: &str,
        attrs: Stack,
    ) -> anyhow::Result<(Stack, Option<Vec<u8>>)> {
        let found = tree.entries().find(|entry| {
            entry.name() == GITATTRIBUTES.as_bytes()
                && matches!(
                    entry.entry_type(),
                    TreeEntryType::File | TreeEntryType::Executable
                )
        });
        let Some(entry) = found else {
            return Ok((attrs, None));
        };
        let named = if prefix.is_empty() {
            GITATTRIBUTES.to_string()
        } else {
            format!("{prefix}/{GITATTRIBUTES}")
        };
        let data = self.fetch_blob_now(entry.id()).await.context_path(&named)?;
        let file = AttributesFile::parse(&data);
        // A file of nothing but comments is not worth a stack frame that every
        // later lookup would walk through.
        let attrs = if file.is_empty() {
            attrs
        } else {
            attrs.push(prefix, file)
        };
        Ok((attrs, Some(data)))
    }

    /// Fetch one blob and wait for it, draining the pools meanwhile.
    ///
    /// Everything else the walk asks for is queued and collected later, because
    /// nothing it does next depends on the answer. An attributes file is the
    /// exception: what it says decides which of its directory's entries are
    /// worth fetching at all, so the walk cannot queue them until it has read
    /// it. The cost is one round trip, and only in directories that carry a
    /// `.gitattributes`; the fetches already in flight keep landing throughout,
    /// so nothing else is held up by the wait.
    async fn fetch_blob_now(&mut self, id: ObjectId) -> anyhow::Result<Vec<u8>> {
        let repo = self.repo;
        let mut fetch = async move {
            Ok::<_, anyhow::Error>(
                repo.object(id)
                    .await?
                    .blob()
                    .map_err(wrong_type)?
                    .data_owned(),
            )
        }
        .boxed_local();
        self.progress.queued(1);
        loop {
            if self.trees.is_empty() && self.blobs.is_empty() {
                let data = fetch.await?;
                self.progress.landed();
                return Ok(data);
            }
            // Dropping the loser is free for the same reason it is in
            // `next_tree`: what either side had got through lives in the pools,
            // not in the future that was waiting on them.
            let (fetched, drained) = match select(&mut fetch, Box::pin(self.drain_one())).await {
                Either::Left((data, _)) => (Some(data), Ok(())),
                Either::Right((drained, _)) => (None, drained),
            };
            drained?;
            if let Some(data) = fetched {
                let data = data?;
                self.progress.landed();
                return Ok(data);
            }
        }
    }

    /// Take delivery of one object from either fetch pool, whichever lands
    /// first. Returns at once if both pools are empty, so every caller checks
    /// that for itself before looping on this.
    async fn drain_one(&mut self) -> anyhow::Result<()> {
        if self.trees.is_empty() {
            if let Some((slot, result)) = self.blobs.next().await {
                self.store_blob(slot, result?)?;
            }
            return Ok(());
        }
        if self.blobs.is_empty() {
            if let Some(landed) = self.trees.next().await {
                self.stash_tree(landed)?;
            }
            return Ok(());
        }
        // Dropping the loser is free: both futures only borrow their stream, so
        // whichever didn't win keeps its progress inside the stream itself.
        // Which one landed is pulled out of the `select` before anything is
        // done with it, so that neither borrow is still live when the result is
        // recorded.
        match select(self.trees.next(), self.blobs.next()).await {
            Either::Left((landed, _)) => {
                if let Some(landed) = landed {
                    self.stash_tree(landed)?;
                }
            }
            Either::Right((landed, _)) => {
                if let Some((slot, result)) = landed {
                    self.store_blob(slot, result?)?;
                }
            }
        }
        Ok(())
    }

    /// Move backlogged subtrees into the fetch pool, up to the tree budget.
    fn issue_trees(&mut self) {
        let repo = self.repo;
        while self.trees.len() < MAX_TREES_IN_FLIGHT {
            let Some((dir, id)) = self.backlog.pop_front() else {
                break;
            };
            self.trees.push(
                async move {
                    let tree = async { repo.object(id).await?.tree().map_err(wrong_type) }.await;
                    (dir, tree)
                }
                .boxed_local(),
            );
        }
    }

    /// The next directory to read, or `None` once every subtree has been
    /// expanded. Blob fetches are drained into their slots while waiting.
    ///
    /// Whichever subtree lands first is the one returned — the walk has no
    /// preferred order, because the output's order doesn't come from here.
    async fn next_tree(&mut self) -> anyhow::Result<Option<(Pending, Tree)>> {
        loop {
            if let Some(ready) = self.landed.pop_front() {
                return Ok(Some(ready));
            }
            self.issue_trees();
            if self.trees.is_empty() {
                return Ok(None);
            }
            // A subtree that lands is set aside rather than expanded on the
            // spot, so it is the next turn of this loop that picks it up.
            self.drain_one().await?;
        }
    }

    /// Set aside a subtree that has arrived, for the main loop to expand, and
    /// refill the slot it just freed in the tree pool.
    fn stash_tree(&mut self, landed: (Pending, anyhow::Result<Tree>)) -> anyhow::Result<()> {
        let (dir, tree) = landed;
        self.progress.landed();
        let tree = tree.context_path(&dir.path)?;
        self.landed.push_back((dir, tree));
        self.issue_trees();
        Ok(())
    }

    /// Put a file's bytes into the slot the walk reserved for them, keeping the
    /// running byte total inside the cap.
    fn fill_slot(&mut self, slot: usize, data: Vec<u8>) -> anyhow::Result<()> {
        self.bytes += data.len();
        if self.bytes > MAX_ARCHIVE_BYTES {
            anyhow::bail!(
                "This snapshot is over the {} MiB limit for archives built in the browser. \
                 Clone the repository instead.",
                MAX_ARCHIVE_BYTES / (1024 * 1024)
            );
        }
        self.fetched[slot] = Some(data);
        Ok(())
    }

    /// As [`fill_slot`], for a blob that came out of the fetch pool: one more
    /// of the objects the progress counts has landed.
    ///
    /// [`fill_slot`]: Walk::fill_slot
    fn store_blob(&mut self, slot: usize, data: Vec<u8>) -> anyhow::Result<()> {
        self.fill_slot(slot, data)?;
        self.progress.landed();
        Ok(())
    }

    /// The walk is over but its last requests may not be; nothing else can
    /// proceed until every slot is filled.
    async fn drain(&mut self) -> anyhow::Result<()> {
        loop {
            let landed = self.blobs.next().await;
            let Some((slot, result)) = landed else { break };
            self.store_blob(slot, result?)?;
        }
        Ok(())
    }
}

/// Walk the finished node tree into the flat, depth-first entry list the tar
/// writer consumes, taking each blob out of the slot its fetch landed in and
/// each directory's contents out of the frame they were read into.
///
/// This is where the archive's order comes from, and the only place it is
/// decided — which is why the walk itself is free to expand directories in
/// whatever order they arrive. Frames are taken as they are visited: each is
/// reachable from exactly one directory node, so nothing is left behind.
fn flatten(
    nodes: Vec<Node>,
    frames: &mut [Vec<Node>],
    fetched: &mut [Option<Vec<u8>>],
    out: &mut Vec<ArchiveEntry>,
) {
    for node in nodes {
        match node {
            Node::Dir { path, frame } => {
                out.push(ArchiveEntry {
                    path,
                    kind: EntryKind::Directory,
                    data: Vec::new(),
                });
                let children = std::mem::take(&mut frames[frame]);
                flatten(children, frames, fetched, out);
            }
            Node::File {
                path,
                executable,
                slot,
            } => out.push(ArchiveEntry {
                path,
                kind: EntryKind::File { executable },
                data: fetched[slot].take().unwrap_or_default(),
            }),
            Node::Symlink { path, slot } => out.push(ArchiveEntry {
                path,
                kind: EntryKind::Symlink {
                    target: fetched[slot].take().unwrap_or_default(),
                },
                data: Vec::new(),
            }),
        }
    }
}

/// Report an object that turned out to be something other than what the tree
/// said it was, which for a tree entry means the repository disagrees with
/// itself.
fn wrong_type(e: UnexpectedObjectType) -> anyhow::Error {
    anyhow::anyhow!(
        "expected {:?} but object {} is a {:?}",
        e.expected,
        e.id,
        e.received
    )
}

/// Name the path in an error that was raised where only the object was known.
trait PathContext<T> {
    fn context_path(self, path: &str) -> anyhow::Result<T>;
}

impl<T> PathContext<T> for anyhow::Result<T> {
    fn context_path(self, path: &str) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{path}: {e}"))
    }
}

#[cfg(test)]
mod walk_tests;
