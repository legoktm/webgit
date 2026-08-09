//! Building a `.tar.gz` of a tree, in the browser.
//!
//! This is `git archive --format=tar.gz` with no server to run it on: the tree
//! is walked object by object, every blob is fetched through the usual caching
//! repo, and the entries are written into a tar which is then gzipped. The tar
//! half is deliberately byte-for-byte what `git archive --format=tar` writes
//! for the same commit — same mode normalisation, same `pax_global_header`
//! carrying the commit id, same record padding — so a snapshot taken here is
//! interchangeable with one taken by git. `test_matches_git_archive` holds that
//! claim to a real `git archive` invocation.
//!
//! Both halves need the browser — the walk for its objects, the gzip for the
//! browser's own encoder — but the tar in between is plain bytes, and that is
//! what the tests exercise: `build_tar` assembles one in memory so it can be
//! compared against git's, and `stream_tar_gz` writes the same bytes a piece at
//! a time into the encoder.

use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::yield_to_browser;
use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, select};
use futures::stream::{FuturesUnordered, StreamExt};
use gib::object::{Object, ObjectId, Tree, TreeEntryType};
use std::collections::VecDeque;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

// The browser's own gzip encoder. Bound here rather than taken from web-sys,
// which still has it behind `--cfg=web_sys_unstable_apis` — a flag that would
// have to be set for every cargo and trunk invocation, and that would switch on
// every other unstable binding along with this one. The class is three members
// wide, and the streams either side of it are ordinary web-sys types.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = CompressionStream)]
    type CompressionStream;

    #[wasm_bindgen(constructor, catch)]
    fn new(format: &str) -> Result<CompressionStream, JsValue>;

    #[wasm_bindgen(method, getter)]
    fn readable(this: &CompressionStream) -> web_sys::ReadableStream;

    #[wasm_bindgen(method, getter)]
    fn writable(this: &CompressionStream) -> web_sys::WritableStream;
}

/// The most file content one snapshot may hold, uncompressed.
///
/// Everything — the blobs, the tar, and the gzip output — is live in wasm's
/// linear memory at once, so a big enough repository doesn't merely take a
/// while, it takes the tab down. This cap turns that into an error message
/// suggesting the obvious alternative (cloning), well before the allocator
/// starts refusing. It counts blob bytes, the part that scales with the
/// repository; the tar's own per-entry overhead is bounded by the file count.
pub(crate) const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

/// One entry to write into the archive: a directory, a file, or a symlink.
///
/// Submodules (gitlinks) have no content to archive and are skipped entirely,
/// as `git archive` skips them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
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
pub(crate) struct ArchiveEntry {
    /// Path within the archive, below the prefix directory. Directories do not
    /// carry the trailing slash; [`append`] adds it.
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

/// A blob fetch in flight: the slot its bytes belong in, and the result.
type BlobFetch<'a> = LocalBoxFuture<'a, (usize, anyhow::Result<Vec<u8>>)>;
/// Every directory's blob fetches, in one pool so they overlap across the tree.
type BlobPool<'a> = FuturesUnordered<BlobFetch<'a>>;
/// A subtree fetch in flight: the frame its contents belong in, the path it was
/// reached by (for error messages), and the result.
type TreeFetch<'a> = LocalBoxFuture<'a, (usize, String, anyhow::Result<Tree>)>;
/// Every directory's subtree fetches, in one pool: the walk expands whichever
/// lands first, which is what keeps discovery ahead of the blob pool.
type TreePool<'a> = FuturesUnordered<TreeFetch<'a>>;

/// Where the walk reads objects from.
///
/// [`CachingRepo`] is the only implementation that ships; the seam exists so
/// the walk can be exercised natively, since `CachingRepo` needs a browser and
/// the walk's ordering is the thing most worth testing.
pub(crate) trait ObjectSource {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>>;
}

impl ObjectSource for CachingRepo {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move { self.lookup_object(id).await.context("read object") }.boxed_local()
    }
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
///
/// The denominator climbs as the walk goes, because how many objects a tree
/// holds isn't known until it has been walked — so this is a floor that rises,
/// not a target that sits still. It's still worth reporting: what it shows is
/// the shape of the work, and the numerator never passes the denominator.
struct Progress<'a> {
    fetched: usize,
    queued: usize,
    /// Called on every change, with `(fetched, queued)`. Rate-limiting is the
    /// caller's business — the walk doesn't know what a repaint costs, and the
    /// only caller that renders anything already throttles on wall time.
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
    backlog: VecDeque<(usize, String, ObjectId)>,
    /// Subtrees that arrived while the walk was waiting on a blob. Expanding
    /// one means reading a directory, which is what the caller is already in
    /// the middle of, so they are set aside for the main loop to pick up.
    landed: VecDeque<(usize, String, Tree)>,
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
/// blobs far too slowly to keep [`MAX_IN_FLIGHT`] of them in flight — simulated
/// against this repository's mediawiki test fixture it managed 31 of the 48,
/// and raising the cap barely helped because discovery, not the cap, was the
/// limit. Breadth-first fills the pool and keeps it full.
///
/// None of that is observable in the result: the output order is fixed by the
/// node tree, not by completion or expansion order, which is what keeps the
/// archive byte-identical to `git archive` regardless of what finishes when.
///
/// `on_progress` is called with `(fetched, queued)` object counts as they
/// change; see [`Progress`] for why the second number is not a fixed total.
///
/// Errors if the accumulated file content exceeds [`MAX_ARCHIVE_BYTES`].
///
/// Paths are decoded as UTF-8, lossily, matching how the tree view renders
/// them: the tar crate cannot encode a non-UTF-8 path on a target without
/// `OsStrExt` (which wasm is), and dropping the file outright would be a worse
/// answer than archiving it under a replacement-character name.
pub(crate) async fn collect_entries<'a, S: ObjectSource>(
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

    walk.read_dir(tree, prefix, 0).await?;
    loop {
        let next = walk.next_tree().await?;
        let Some((frame, path, subtree)) = next else {
            break;
        };
        walk.read_dir(&subtree, &path, frame).await?;
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
    async fn read_dir(&mut self, tree: &Tree, prefix: &str, frame: usize) -> anyhow::Result<()> {
        // Copied out so the futures pushed below borrow the source rather than
        // `self`, which the pools they go into are part of.
        let repo = self.repo;
        let mut children = Vec::new();

        for entry in tree.entries() {
            let name = String::from_utf8_lossy(entry.name()).into_owned();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            match entry.entry_type() {
                TreeEntryType::Tree => {
                    let child = self.frames.len();
                    self.frames.push(Vec::new());
                    children.push(Node::Dir {
                        path: path.clone(),
                        frame: child,
                    });
                    self.backlog.push_back((child, path, entry.id()));
                    self.issue_trees();
                    self.progress.queued(1);
                }
                // A submodule is a commit id pointing into a repository we
                // don't have; `git archive` leaves it out and so do we. It is
                // never fetched, so it never counts towards the progress.
                TreeEntryType::Commit => {}
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
                    // Wait for room before adding to the pool, so the number of
                    // outstanding requests stays bounded however wide the tree
                    // is. This is also the walk's throttle: expanding stops
                    // here until blobs land, which is what stops discovery from
                    // running arbitrarily far ahead of the fetching.
                    while self.blobs.len() >= MAX_IN_FLIGHT {
                        self.await_blob().await?;
                    }
                    let id = entry.id();
                    self.blobs.push(
                        async move {
                            let data = async {
                                Ok(repo
                                    .object(id)
                                    .await?
                                    .blob()
                                    .map_err(gib::error::Error::from)
                                    .context("expected a blob")?
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

    /// Move backlogged subtrees into the fetch pool, up to the tree budget.
    fn issue_trees(&mut self) {
        let repo = self.repo;
        while self.trees.len() < MAX_TREES_IN_FLIGHT {
            let Some((frame, path, id)) = self.backlog.pop_front() else {
                break;
            };
            self.trees.push(
                async move {
                    let tree = async {
                        repo.object(id)
                            .await?
                            .tree()
                            .map_err(gib::error::Error::from)
                            .context("expected a tree")
                    }
                    .await;
                    (frame, path, tree)
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
    async fn next_tree(&mut self) -> anyhow::Result<Option<(usize, String, Tree)>> {
        loop {
            if let Some(ready) = self.landed.pop_front() {
                return Ok(Some(ready));
            }
            self.issue_trees();
            if self.trees.is_empty() {
                return Ok(None);
            }
            if self.blobs.is_empty() {
                let landed = self.trees.next().await;
                match landed {
                    Some(landed) => self.stash_tree(landed)?,
                    None => return Ok(None),
                }
                continue;
            }
            // Dropping the loser is free: both futures only borrow their
            // stream, so whichever didn't win keeps its progress inside the
            // stream itself. Which one landed is pulled out of the `select`
            // before anything is done with it, so that neither borrow is still
            // live when the result is recorded.
            let (tree, blob) = match select(self.trees.next(), self.blobs.next()).await {
                Either::Left((tree, _)) => (tree, None),
                Either::Right((blob, _)) => (None, blob),
            };
            if let Some(landed) = tree {
                self.stash_tree(landed)?;
                continue;
            }
            if let Some((slot, result)) = blob {
                self.store_blob(slot, result?)?;
            }
        }
    }

    /// Wait for one blob to land, keeping subtree fetches polled meanwhile.
    ///
    /// The polling is the point: a future that has never been polled has never
    /// issued its request, so a bare await on the blob pool would leave freshly
    /// queued subtree fetches sitting idle — which is exactly the discovery
    /// stall the breadth-first walk is shaped to avoid. A subtree that lands
    /// here is set aside rather than expanded, since expanding it means reading
    /// a directory and the caller is already part-way through one.
    async fn await_blob(&mut self) -> anyhow::Result<()> {
        loop {
            if self.trees.is_empty() {
                let landed = self.blobs.next().await;
                if let Some((slot, result)) = landed {
                    self.store_blob(slot, result?)?;
                }
                return Ok(());
            }
            let (tree, blob) = match select(self.trees.next(), self.blobs.next()).await {
                Either::Left((tree, _)) => (tree, None),
                Either::Right((blob, _)) => (None, blob),
            };
            if let Some(landed) = tree {
                self.stash_tree(landed)?;
                continue;
            }
            if let Some((slot, result)) = blob {
                self.store_blob(slot, result?)?;
            }
            return Ok(());
        }
    }

    /// Set aside a subtree that has arrived, for the main loop to expand, and
    /// refill the slot it just freed in the tree pool.
    fn stash_tree(&mut self, landed: (usize, String, anyhow::Result<Tree>)) -> anyhow::Result<()> {
        let (frame, path, tree) = landed;
        self.progress.landed();
        let tree = tree.context_path(&path)?;
        self.landed.push_back((frame, path, tree));
        self.issue_trees();
        Ok(())
    }

    /// Record one finished blob, keeping the running byte total inside the cap.
    fn store_blob(&mut self, slot: usize, data: Vec<u8>) -> anyhow::Result<()> {
        self.bytes += data.len();
        if self.bytes > MAX_ARCHIVE_BYTES {
            anyhow::bail!(
                "This snapshot is over the {} MiB limit for archives built in the browser. \
                 Clone the repository instead.",
                MAX_ARCHIVE_BYTES / (1024 * 1024)
            );
        }
        self.fetched[slot] = Some(data);
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

/// Name the path in an error that was raised where only the object was known.
trait PathContext<T> {
    fn context_path(self, path: &str) -> anyhow::Result<T>;
}

impl<T> PathContext<T> for anyhow::Result<T> {
    fn context_path(self, path: &str) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{path}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// tar
// ---------------------------------------------------------------------------

/// Tar's record size: archives are padded out to a multiple of this, as git's
/// (and GNU tar's) are.
const RECORD_SIZE: usize = 512 * 20;

/// `git archive` normalises every mode to one of these before writing it —
/// permissions in a git tree only really record the executable bit, so the rest
/// is invented, and git invents 0666/0777 minus its default `tar.umask` of 002.
/// Reproducing that is what makes the output comparable to git's.
const MODE_FILE: u32 = 0o664;
const MODE_EXEC: u32 = 0o775;
const MODE_DIR: u32 = 0o775;
const MODE_LINK: u32 = 0o777;

/// How much tar to accumulate before handing it to the encoder.
///
/// Small enough that no single slice is perceptible, large enough that a big
/// archive isn't thousands of promises.
const FLUSH_BYTES: usize = 1024 * 1024;

/// The archive's content type, on the blob the browser hands back.
pub(crate) const GZIP_MIME: &str = "application/gzip";

/// How long, in milliseconds of wall time, to go between letting the page
/// repaint while an archive is being written.
///
/// Awaiting the encoder is *not* enough on its own: a resolved promise is a
/// microtask, so the DOM updates but the browser never gets to paint it — see
/// [`yield_to_browser`], which is a real timer and therefore a real macrotask
/// boundary. Without one of those the progress bar is updated and never seen.
/// Same interval as the commit view's streamed diff.
const PAINT_INTERVAL_MS: f64 = 50.0;

/// Write the archive's opening records: the pax global header carrying the
/// commit id, then the prefix directory itself.
///
/// The global header is the one `git archive` writes and `git get-tar-commit-id`
/// reads back out. Its record is a pax keyword line whose leading number counts
/// its own bytes: `comment=<id>\n` plus the digits of the length plus the
/// separating space.
fn open_tar(
    builder: &mut tar::Builder<Vec<u8>>,
    prefix: &str,
    commit: &str,
    mtime: u64,
) -> std::io::Result<()> {
    let comment = format!("comment={commit}\n");
    let record = format!("{} {}", comment.len() + 3, comment);
    let mut header = tar_header(mtime);
    header.set_mode(0o666);
    header.set_size(record.len() as u64);
    header.set_entry_type(tar::EntryType::XGlobalHeader);
    builder.append_data(&mut header, "pax_global_header", record.as_bytes())?;

    append(builder, prefix, &EntryKind::Directory, &[], mtime)
}

/// Finish the archive, returning whatever is left to write.
///
/// `emitted` is how much of the archive has already gone out, which the padding
/// needs because git (like GNU tar) pads the *whole* archive to a multiple of
/// [`RECORD_SIZE`] — a property of the total length, not of this last piece.
fn finish_tar(builder: tar::Builder<Vec<u8>>, emitted: usize) -> std::io::Result<Vec<u8>> {
    let mut tail = builder.into_inner()?;
    let remainder = (emitted + tail.len()) % RECORD_SIZE;
    if remainder != 0 {
        tail.resize(tail.len() + (RECORD_SIZE - remainder), 0);
    }
    Ok(tail)
}

/// Build the whole tar in memory. Only the tests want this — what ships streams
/// it through [`stream_tar_gz`] instead — but it is what pins the byte-for-byte
/// agreement with `git archive`.
#[cfg(test)]
fn build_tar(
    entries: &[ArchiveEntry],
    prefix: &str,
    commit: &str,
    mtime: u64,
) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    open_tar(&mut builder, prefix, commit, mtime)?;
    for entry in entries {
        append(
            &mut builder,
            &format!("{prefix}{}", entry.path),
            &entry.kind,
            &entry.data,
            mtime,
        )?;
    }
    let mut out = std::mem::take(builder.get_mut());
    out.extend_from_slice(&finish_tar(builder, out.len())?);
    Ok(out)
}

/// Write `entries` as a tar, gzip it, and hand back the archive as a [`Blob`].
///
/// `prefix` is the directory every entry is placed under (git's `--prefix`),
/// `commit` the id recorded in the archive's global header, and `mtime` the
/// timestamp stamped on every entry — the commit's own time, so that archiving
/// the same commit twice yields the same bytes. `on_progress` is called with
/// `(entries written, total)`.
///
/// Nothing here holds the archive. The tar is fed to the browser's own gzip
/// encoder a piece at a time rather than assembled whole and compressed in one
/// call, `entries` is consumed as it is written so each blob is dropped once it
/// has gone in, and the compressed side is drained by the browser into a `Blob`
/// — which it owns, and can back with disk — instead of being reassembled into
/// a `Vec` on our side and then copied again to hand over.
///
/// That is also what keeps the page alive: compressing a large repository is
/// seconds of work, and a single synchronous call to a compressor in wasm
/// freezes the tab for all of it. The encoder being the browser's puts the
/// compression off our thread; [`PAINT_INTERVAL_MS`] does the rest.
pub(crate) async fn stream_tar_gz(
    entries: Vec<ArchiveEntry>,
    prefix: &str,
    commit: &str,
    mtime: u64,
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<web_sys::Blob> {
    let gzip = CompressionStream::new("gzip").map_err(|e| js_error("start the gzip encoder", e))?;
    let writer = gzip
        .writable()
        .get_writer()
        .map_err(|e| js_error("open the gzip encoder", e))?;
    // A `Response` over the encoder's output side, purely to get at `.blob()`:
    // it is the one API that will drain a stream into a `Blob` for us. The
    // stream must be left alone here — taking a reader would lock it and the
    // `Response` would have nothing to read.
    let headers = web_sys::Headers::new().map_err(|e| js_error("build a response", e))?;
    headers
        .set("Content-Type", GZIP_MIME)
        .map_err(|e| js_error("build a response", e))?;
    let init = web_sys::ResponseInit::new();
    init.set_headers(&headers);
    let response =
        web_sys::Response::new_with_opt_readable_stream_and_init(Some(&gzip.readable()), &init)
            .map_err(|e| js_error("build a response", e))?;

    let total = entries.len();
    let write = async {
        let mut builder = tar::Builder::new(Vec::new());
        open_tar(&mut builder, prefix, commit, mtime)?;
        let mut emitted = 0usize;

        let mut last_paint = js_sys::Date::now();

        for (written, entry) in entries.into_iter().enumerate() {
            append(
                &mut builder,
                &format!("{prefix}{}", entry.path),
                &entry.kind,
                &entry.data,
                mtime,
            )?;
            // `entry`, and with it this file's bytes, is dropped here.
            if builder.get_ref().len() >= FLUSH_BYTES {
                let chunk = std::mem::take(builder.get_mut());
                emitted += chunk.len();
                push(&writer, chunk).await?;
                // Report and repaint on a wall-clock budget rather than per
                // flush: a flush is only a megabyte, and `yield_to_browser` is a
                // real timer whose cost would otherwise scale with the archive.
                let now = js_sys::Date::now();
                if now - last_paint >= PAINT_INTERVAL_MS {
                    last_paint = now;
                    on_progress(written + 1, total);
                    yield_to_browser().await;
                }
            }
        }

        let tail = finish_tar(builder, emitted)?;
        push(&writer, tail).await?;
        on_progress(total, total);
        JsFuture::from(writer.close())
            .await
            .map_err(|e| js_error("finish the gzip stream", e))?;
        Ok::<(), anyhow::Error>(())
    };

    // Collected concurrently with the writing above, not after it: the encoder's
    // output has to be drained as it is produced or its backpressure stalls the
    // very writes we are waiting on.
    let collect = async {
        let blob = JsFuture::from(
            response
                .blob()
                .map_err(|e| js_error("collect the archive", e))?,
        )
        .await
        .map_err(|e| js_error("collect the archive", e))?;
        blob.dyn_into::<web_sys::Blob>()
            .map_err(|_| anyhow::anyhow!("the archive came back as something other than a blob"))
    };

    let ((), archive) = futures::try_join!(write, collect)?;
    Ok(archive)
}

/// Hand one chunk of tar to the encoder. Awaiting the write is both the
/// backpressure and the yield that keeps the page responsive.
async fn push(writer: &web_sys::WritableStreamDefaultWriter, chunk: Vec<u8>) -> anyhow::Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    let bytes = js_sys::Uint8Array::from(chunk.as_slice());
    JsFuture::from(writer.write_with_chunk(&bytes))
        .await
        .map_err(|e| js_error("write to the gzip encoder", e))?;
    Ok(())
}

/// Describe a rejected promise or a failed constructor. `JsValue` is only
/// sometimes a string, so fall back to its debug form.
fn js_error(what: &str, e: JsValue) -> anyhow::Error {
    anyhow::anyhow!(
        "Failed to {what}: {}",
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    )
}

/// A header with the fields git fills in identically for every entry: no owner,
/// named `root`, and explicit (rather than left-empty) device numbers.
fn tar_header(mtime: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_uid(0);
    header.set_gid(0);
    // Both names fit, and the header is ustar, so neither call can fail.
    header.set_username("root").expect("username fits");
    header.set_groupname("root").expect("groupname fits");
    header.set_mtime(mtime);
    header.set_device_major(0).expect("ustar header");
    header.set_device_minor(0).expect("ustar header");
    header
}

fn append(
    builder: &mut tar::Builder<Vec<u8>>,
    path: &str,
    kind: &EntryKind,
    data: &[u8],
    mtime: u64,
) -> std::io::Result<()> {
    let mut header = tar_header(mtime);
    match kind {
        EntryKind::Directory => {
            header.set_mode(MODE_DIR);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            // A directory member's name carries the trailing slash.
            builder.append_data(&mut header, format!("{path}/"), &[][..])
        }
        EntryKind::Symlink { target } => {
            header.set_mode(MODE_LINK);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name(String::from_utf8_lossy(target).as_ref())?;
            builder.append_data(&mut header, path, &[][..])
        }
        EntryKind::File { executable } => {
            header.set_mode(if *executable { MODE_EXEC } else { MODE_FILE });
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, path, data)
        }
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use gib::object::{ObjectType, RawObject};
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    /// An object store that answers from memory, one poll later than asked.
    ///
    /// The delay is the point: a fetch that completed immediately would hide
    /// whether the walk actually overlaps its requests, so every lookup yields
    /// once before returning, and the source records how many were outstanding
    /// at the high-water mark.
    struct FakeSource {
        objects: BTreeMap<ObjectId, (ObjectType, Vec<u8>)>,
        live: Cell<usize>,
        peak: Cell<usize>,
        /// Every object in the order its fetch was first polled, which is when
        /// a real one would have put its request on the wire.
        started: RefCell<Vec<ObjectId>>,
    }

    impl FakeSource {
        fn new(objects: BTreeMap<ObjectId, (ObjectType, Vec<u8>)>) -> Self {
            Self {
                objects,
                live: Cell::new(0),
                peak: Cell::new(0),
                started: RefCell::new(Vec::new()),
            }
        }

        /// When `id` was first asked for, as a position in the fetch order.
        fn started_at(&self, id: ObjectId) -> usize {
            self.started
                .borrow()
                .iter()
                .position(|&started| started == id)
                .expect("object was fetched")
        }
    }

    impl ObjectSource for FakeSource {
        fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
            async move {
                self.live.set(self.live.get() + 1);
                self.peak.set(self.peak.get().max(self.live.get()));
                self.started.borrow_mut().push(id);
                yield_once().await;
                self.live.set(self.live.get() - 1);
                let (object_type, body) = self
                    .objects
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("missing object {id}"))?;
                Object::from_raw(
                    id,
                    RawObject {
                        object_type: *object_type,
                        body: body.clone(),
                    },
                )
                .map_err(|e| anyhow::anyhow!("{e:?}"))
            }
            .boxed_local()
        }
    }

    /// Pend exactly once, waking immediately, so other queued futures get a
    /// chance to run before this one finishes.
    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(move |cx| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await
    }

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_bytes([n; 20])
    }

    /// Serialise tree entries into a git tree object body.
    fn tree_body(entries: &[(&str, &str, u8)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (mode, name, id) in entries {
            body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            body.extend_from_slice(&[*id; 20]);
        }
        body
    }

    struct Fixture {
        source: FakeSource,
        root: Tree,
    }

    /// A repository with a nested directory in the middle of its root, which is
    /// what makes the ordering interesting: `src/`'s contents have to land
    /// between `src` and the sibling that follows it.
    fn fixture() -> Fixture {
        let root = tree_body(&[
            ("100644", "README.md", 10),
            ("40000", "src", 2),
            ("100755", "run.sh", 11),
            ("120000", "link.md", 12),
            ("160000", "vendor", 13),
        ]);
        let src = tree_body(&[("100644", "lib.rs", 14), ("40000", "render", 3)]);
        let render = tree_body(&[("100644", "mod.rs", 15)]);

        let mut objects = BTreeMap::new();
        objects.insert(oid(1), (ObjectType::Tree, root.clone()));
        objects.insert(oid(2), (ObjectType::Tree, src));
        objects.insert(oid(3), (ObjectType::Tree, render));
        objects.insert(oid(10), (ObjectType::Blob, b"hi\n".to_vec()));
        objects.insert(oid(11), (ObjectType::Blob, b"#!/bin/sh\n".to_vec()));
        objects.insert(oid(12), (ObjectType::Blob, b"README.md".to_vec()));
        objects.insert(oid(14), (ObjectType::Blob, b"pub mod x;\n".to_vec()));
        objects.insert(oid(15), (ObjectType::Blob, b"// mod\n".to_vec()));

        Fixture {
            root: Object::from_raw(
                oid(1),
                RawObject {
                    object_type: ObjectType::Tree,
                    body: root,
                },
            )
            .unwrap()
            .tree()
            .unwrap(),
            source: FakeSource::new(objects),
        }
    }

    fn collect(f: &Fixture) -> Vec<ArchiveEntry> {
        futures::executor::block_on(collect_entries(&f.source, &f.root, "", &|_, _| {})).unwrap()
    }

    /// The output is in depth-first tree order, with each directory's contents
    /// between it and its next sibling — even though the walk itself expands
    /// directories breadth-first and its fetches finish in whatever order they
    /// please. Order comes from [`flatten`], not from the walk.
    #[test]
    fn test_walk_is_depth_first() {
        let f = fixture();
        let entries = collect(&f);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "README.md",
                "src",
                "src/lib.rs",
                "src/render",
                "src/render/mod.rs",
                "run.sh",
                "link.md",
            ]
        );
    }

    #[test]
    fn test_walk_kinds_and_content() {
        let f = fixture();
        let entries = collect(&f);
        let by_path = |p: &str| entries.iter().find(|e| e.path == p).expect("entry present");

        assert_eq!(by_path("README.md").data, b"hi\n");
        assert_eq!(by_path("run.sh").kind, EntryKind::File { executable: true });
        assert_eq!(
            by_path("src").kind,
            EntryKind::Directory,
            "a directory entry carries no content of its own"
        );
        assert_eq!(
            by_path("link.md").kind,
            EntryKind::Symlink {
                target: b"README.md".to_vec()
            },
            "a symlink's blob is its target, not its content"
        );
    }

    /// A submodule points into a repository we don't have, so it is left out
    /// entirely — and, unlike every other entry, never fetched.
    #[test]
    fn test_walk_skips_submodules() {
        let f = fixture();
        assert!(collect(&f).iter().all(|e| e.path != "vendor"));
    }

    /// The point of the whole arrangement: requests overlap. Sequentially this
    /// would never exceed one outstanding fetch.
    #[test]
    fn test_walk_overlaps_fetches() {
        let f = fixture();
        collect(&f);
        // The root queues three blobs and a subtree before anything is awaited,
        // so all four should be outstanding together.
        assert!(
            f.source.peak.get() >= 4,
            "expected overlapping fetches, peaked at {}",
            f.source.peak.get()
        );
    }

    /// Directories are expanded breadth-first, which is what keeps the blob
    /// pool fed. The fixture is the shape that tells the two orders apart: a
    /// deep, narrow chain of directories next to a wide one. Depth-first has to
    /// walk the whole chain before it ever looks inside `wide`, leaving almost
    /// nothing in flight the entire way down; breadth-first reaches both at the
    /// same level and has `wide`'s files on the wire immediately.
    #[test]
    fn test_walk_expands_breadth_first() {
        let wide_blob = oid(30);
        let deep_blob = oid(31);
        let names: Vec<String> = (0..40).map(|i| format!("f-{i:02}")).collect();
        let wide: Vec<(&str, &str, u8)> =
            names.iter().map(|n| ("100644", n.as_str(), 30u8)).collect();

        let mut objects = BTreeMap::new();
        objects.insert(
            oid(1),
            (
                ObjectType::Tree,
                tree_body(&[("40000", "chain", 2), ("40000", "wide", 3)]),
            ),
        );
        objects.insert(oid(3), (ObjectType::Tree, tree_body(&wide)));
        // chain/c1/c2/c3/deep.txt — four levels before a single file.
        objects.insert(oid(2), (ObjectType::Tree, tree_body(&[("40000", "c1", 4)])));
        objects.insert(oid(4), (ObjectType::Tree, tree_body(&[("40000", "c2", 5)])));
        objects.insert(oid(5), (ObjectType::Tree, tree_body(&[("40000", "c3", 6)])));
        objects.insert(
            oid(6),
            (ObjectType::Tree, tree_body(&[("100644", "deep.txt", 31)])),
        );
        objects.insert(oid(30), (ObjectType::Blob, b"w\n".to_vec()));
        objects.insert(oid(31), (ObjectType::Blob, b"d\n".to_vec()));

        let source = FakeSource::new(objects);
        let root = Object::from_raw(
            oid(1),
            RawObject {
                object_type: ObjectType::Tree,
                body: tree_body(&[("40000", "chain", 2), ("40000", "wide", 3)]),
            },
        )
        .unwrap()
        .tree()
        .unwrap();

        let entries =
            futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
        assert_eq!(entries.len(), 40 + 6, "every entry is still archived");
        assert!(
            source.started_at(wide_blob) < source.started_at(deep_blob),
            "the wide directory's files should be requested before the chain is \
             walked to the bottom; wide started at {}, the deep file at {}",
            source.started_at(wide_blob),
            source.started_at(deep_blob),
        );
    }

    /// ...but not without limit: a wide directory still keeps the number of
    /// outstanding requests bounded.
    #[test]
    fn test_walk_bounds_in_flight() {
        let count = MAX_IN_FLIGHT * 3;
        let names: Vec<String> = (0..count).map(|i| format!("file-{i:04}")).collect();
        let entries: Vec<(&str, &str, u8)> =
            names.iter().map(|n| ("100644", n.as_str(), 20u8)).collect();
        let body = tree_body(&entries);

        let mut objects = BTreeMap::new();
        objects.insert(oid(20), (ObjectType::Blob, b"x\n".to_vec()));
        let source = FakeSource::new(objects);
        let root = Object::from_raw(
            oid(1),
            RawObject {
                object_type: ObjectType::Tree,
                body,
            },
        )
        .unwrap()
        .tree()
        .unwrap();

        let entries =
            futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
        assert_eq!(entries.len(), count);
        assert!(
            source.peak.get() <= MAX_IN_FLIGHT,
            "{} fetches were outstanding at once, cap is {MAX_IN_FLIGHT}",
            source.peak.get()
        );
        // And the cap is a ceiling, not the working level: the pipeline should
        // sit near it rather than trickling.
        assert!(
            source.peak.get() > MAX_IN_FLIGHT / 2,
            "only {} fetches overlapped, well under the {MAX_IN_FLIGHT} cap",
            source.peak.get()
        );
    }

    /// What the snapshot view's bar is drawn from: counts that only ever go
    /// up, that end with every requested object accounted for, and whose
    /// denominator is still growing after objects have started landing — the
    /// walk discovers the tree as it fetches it, so the total is not known in
    /// advance and the bar has to tolerate it moving.
    #[test]
    fn test_walk_reports_progress() {
        let f = fixture();
        let ticks = std::cell::RefCell::new(Vec::new());
        let report = |fetched, queued| ticks.borrow_mut().push((fetched, queued));
        futures::executor::block_on(collect_entries(&f.source, &f.root, "", &report)).unwrap();
        let ticks = ticks.into_inner();

        // Two subtrees (src, render) and five blobs; the submodule is skipped
        // and the root tree was handed in already fetched.
        assert_eq!(
            ticks.last().copied(),
            Some((7, 7)),
            "the walk should finish with every requested object fetched"
        );
        for pair in ticks.windows(2) {
            let ((was_fetched, was_queued), (fetched, queued)) = (pair[0], pair[1]);
            assert!(
                fetched >= was_fetched && queued >= was_queued,
                "counts went backwards: {:?} then {:?}",
                pair[0],
                pair[1]
            );
            assert!(fetched <= queued, "fetched {fetched} of only {queued}");
        }
        assert!(
            ticks
                .windows(2)
                .any(|pair| pair[0].0 > 0 && pair[1].1 > pair[0].1),
            "expected the total to keep rising after objects began landing"
        );
    }

    /// Subtree fetches are bounded too, by their own budget: breadth-first
    /// expansion reaches far more directories at once than depth-first ever
    /// did, so a directory of directories must not put every one of them on the
    /// wire together.
    #[test]
    fn test_walk_bounds_trees_in_flight() {
        let count = MAX_TREES_IN_FLIGHT * 3;
        let names: Vec<String> = (0..count).map(|i| format!("dir-{i:04}")).collect();
        let dirs: Vec<(&str, &str, u8)> =
            names.iter().map(|n| ("40000", n.as_str(), 2u8)).collect();
        let body = tree_body(&dirs);

        let mut objects = BTreeMap::new();
        // Every subdirectory is the same tree, holding one file.
        objects.insert(
            oid(2),
            (ObjectType::Tree, tree_body(&[("100644", "f", 20)])),
        );
        objects.insert(oid(20), (ObjectType::Blob, b"x\n".to_vec()));
        let source = FakeSource::new(objects);
        let root = Object::from_raw(
            oid(1),
            RawObject {
                object_type: ObjectType::Tree,
                body,
            },
        )
        .unwrap()
        .tree()
        .unwrap();

        let entries =
            futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
        assert_eq!(entries.len(), count * 2, "a directory and a file for each");
        // The two budgets are separate, so the ceiling is their sum.
        assert!(
            source.peak.get() <= MAX_IN_FLIGHT + MAX_TREES_IN_FLIGHT,
            "{} fetches were outstanding at once, cap is {}",
            source.peak.get(),
            MAX_IN_FLIGHT + MAX_TREES_IN_FLIGHT,
        );
        assert!(
            source.peak.get() > MAX_TREES_IN_FLIGHT,
            "only {} fetches overlapped; the tree and blob budgets should both \
             be in use at once",
            source.peak.get()
        );
    }

    /// A repository too big to archive fails with the size cap's message
    /// rather than by exhausting memory.
    #[test]
    fn test_walk_enforces_size_cap() {
        let names: Vec<String> = (0..4).map(|i| format!("big-{i}")).collect();
        let entries: Vec<(&str, &str, u8)> =
            names.iter().map(|n| ("100644", n.as_str(), 21u8)).collect();
        let body = tree_body(&entries);

        let mut objects = BTreeMap::new();
        // Four of these overrun the cap; one does not.
        objects.insert(
            oid(21),
            (ObjectType::Blob, vec![0u8; MAX_ARCHIVE_BYTES / 3]),
        );
        let source = FakeSource::new(objects);
        let root = Object::from_raw(
            oid(1),
            RawObject {
                object_type: ObjectType::Tree,
                body,
            },
        )
        .unwrap()
        .tree()
        .unwrap();

        let err = futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {}))
            .expect_err("expected the size cap to reject this");
        assert!(
            err.to_string().contains("limit for archives"),
            "unexpected error: {err}"
        );
    }

    /// A missing object is reported with the path it was reached by, not just
    /// its id.
    #[test]
    fn test_walk_reports_missing_subtree_path() {
        let mut f = fixture();
        f.source.objects.remove(&oid(2));
        let err = futures::executor::block_on(collect_entries(&f.source, &f.root, "", &|_, _| {}))
            .expect_err("expected the missing subtree to fail the walk");
        assert!(err.to_string().contains("src"), "unexpected error: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn file(path: &str, data: &str) -> ArchiveEntry {
        ArchiveEntry {
            path: path.to_string(),
            kind: EntryKind::File { executable: false },
            data: data.as_bytes().to_vec(),
        }
    }

    fn fixture() -> Vec<ArchiveEntry> {
        vec![
            file("README.md", "hi\n"),
            ArchiveEntry {
                path: "link.md".to_string(),
                kind: EntryKind::Symlink {
                    target: b"README.md".to_vec(),
                },
                data: Vec::new(),
            },
            ArchiveEntry {
                path: "run.sh".to_string(),
                kind: EntryKind::File { executable: true },
                data: b"#!/bin/sh\n".to_vec(),
            },
            ArchiveEntry {
                path: "sub".to_string(),
                kind: EntryKind::Directory,
                data: Vec::new(),
            },
            file("sub/a.txt", "x\n"),
        ]
    }

    /// Read an archive back into `(path, mode, type, link target, size)` rows.
    fn entries_of(tar_bytes: &[u8]) -> Vec<(String, u32, u8, Option<String>, u64)> {
        let mut archive = tar::Archive::new(tar_bytes);
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let h = e.header();
                (
                    e.path().unwrap().to_string_lossy().into_owned(),
                    h.mode().unwrap(),
                    h.entry_type().as_byte(),
                    h.link_name()
                        .unwrap()
                        .map(|p| p.to_string_lossy().into_owned()),
                    h.size().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn test_tar_entries() {
        let tar = build_tar(&fixture(), "demo-main/", &"a".repeat(40), 1_700_000_000).unwrap();
        let got = entries_of(&tar);
        let names: Vec<&str> = got.iter().map(|(p, ..)| p.as_str()).collect();
        assert_eq!(
            names,
            [
                "pax_global_header",
                "demo-main/",
                "demo-main/README.md",
                "demo-main/link.md",
                "demo-main/run.sh",
                "demo-main/sub/",
                "demo-main/sub/a.txt",
            ]
        );
        // Modes and types: dir, regular, symlink (with its target), executable.
        assert_eq!(got[1].1, MODE_DIR);
        assert_eq!(got[1].2, b'5');
        assert_eq!(got[2].1, MODE_FILE);
        assert_eq!(got[3].2, b'2');
        assert_eq!(got[3].3.as_deref(), Some("README.md"));
        assert_eq!(got[4].1, MODE_EXEC);
        assert_eq!(got[2].4, 3);
    }

    /// A path too long for a ustar header still round-trips, via the GNU
    /// long-name entry the tar crate falls back to.
    #[test]
    fn test_long_path_round_trips() {
        let long = format!("{}/deep.txt", "a-long-directory-name".repeat(8));
        let tar = build_tar(
            &[file(&long, "deep\n")],
            "demo-main/",
            &"b".repeat(40),
            1_700_000_000,
        )
        .unwrap();
        let got = entries_of(&tar);
        assert!(
            got.iter().any(|(p, ..)| *p == format!("demo-main/{long}")),
            "long path missing from {:?}",
            got.iter().map(|(p, ..)| p).collect::<Vec<_>>()
        );
    }

    /// The whole point of the mode normalisation, the global header and the
    /// record padding: what we write is what `git archive --format=tar` writes,
    /// byte for byte, for the same commit.
    #[test]
    fn test_matches_git_archive() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_DATE", "2023-11-14T17:13:20Z")
                .env("GIT_COMMITTER_DATE", "2023-11-14T17:13:20Z")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.org")
                .env("GIT_COMMITTER_EMAIL", "t@example.org")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
            out.stdout
        };

        git(&["init", "-q", "."]);
        std::fs::write(path.join("README.md"), "hi\n").unwrap();
        std::fs::write(path.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            path.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("README.md", path.join("link.md")).unwrap();
        std::fs::create_dir(path.join("sub")).unwrap();
        std::fs::write(path.join("sub/a.txt"), "x\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "t"]);

        let sha = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        let sha = sha.trim();
        let mtime: u64 = String::from_utf8(git(&["log", "-1", "--format=%ct"]))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let theirs = git(&["archive", "--format=tar", "--prefix=demo-main/", "HEAD"]);

        let ours = build_tar(&fixture(), "demo-main/", sha, mtime).unwrap();
        assert_eq!(
            ours.len(),
            theirs.len(),
            "archive length differs from git's ({} vs {})",
            ours.len(),
            theirs.len()
        );
        assert!(
            ours == theirs,
            "archive bytes differ from `git archive`; ours: {:?}",
            entries_of(&ours)
        );
    }

    /// Streaming the tar out in flush-sized pieces must produce exactly the
    /// archive [`build_tar`] produces in one go — the padding in particular,
    /// which is a property of the whole archive's length rather than of the
    /// last piece, and so is the part a chunked writer would get wrong.
    ///
    /// The gzip layer itself isn't covered here: `CompressionStream` is the
    /// browser's, so there is nothing to exercise natively. What is covered is
    /// everything fed *into* it.
    #[test]
    fn test_streamed_tar_matches_whole_tar() {
        let prefix = "demo-main/";
        let commit = "c".repeat(40);
        let mtime = 1_700_000_000;
        let whole = build_tar(&fixture(), prefix, &commit, mtime).unwrap();

        // What `stream_tar_gz` writes, with the flush threshold dropped to a
        // single byte so that every entry lands in its own chunk.
        let mut builder = tar::Builder::new(Vec::new());
        open_tar(&mut builder, prefix, &commit, mtime).unwrap();
        let mut streamed = Vec::new();
        for entry in fixture() {
            append(
                &mut builder,
                &format!("{prefix}{}", entry.path),
                &entry.kind,
                &entry.data,
                mtime,
            )
            .unwrap();
            streamed.append(builder.get_mut());
        }
        streamed.extend_from_slice(&finish_tar(builder, streamed.len()).unwrap());

        assert_eq!(
            streamed, whole,
            "streamed archive differs from the whole one"
        );
        assert_eq!(
            streamed.len() % RECORD_SIZE,
            0,
            "a streamed archive is still padded to a whole record"
        );
    }
}
