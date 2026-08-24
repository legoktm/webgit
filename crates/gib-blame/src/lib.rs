//! Which commit last touched each line of a file, the way `git blame` decides.
//!
//! This is a port of git's `blame.c` driven as cgit drives it: `ui-blame.c`
//! calls `assign_blame(&sb, 0)`, which is blame with every optional engine
//! turned off — no `-M`/`-C` move-and-copy detection, no `--ignore-rev`, no
//! `--reverse`. What remains is the part that decides the ordinary answer, and
//! it is ported line for line, because "close to git" is not a useful answer
//! for a view whose whole content is per-line attribution.
//!
//! # How it works
//!
//! The unit of work is an *origin*: one blob, at one path, in one commit, with
//! the lines currently suspected of coming from it. Blame starts with a single
//! origin — the file as the starting commit has it, suspected of every line —
//! and repeatedly picks the newest unfinished commit, diffs its file against
//! each parent's, and passes the lines that are unchanged in a parent on to
//! that parent. Lines that survive with no parent to blame belong to the commit
//! holding them, and are moved to the final list.
//!
//! Two things make that agree with git rather than merely resemble it:
//!
//! * the diff is git's own, through [`gib_xdiff::hunks`] — the same call
//!   `blame.c`'s `diff_hunks` makes, with the same flags, so the hunk
//!   boundaries a line is attributed by are the ones git would have used; and
//! * the commit queue is [`gib_log::Frontier`], which reproduces git's
//!   `prio_queue` ordering (newest commit date first, ties broken by insertion
//!   order). Blame's answer depends on that order: which commit gets to hand
//!   its lines away first decides who ends up guilty for them.
//!
//! # Which git this agrees with
//!
//! cgit's. The diff runs with no flags, because that is what cgit's scoreboard
//! does — it never sets `sb.xdl_opts`. `git blame` on the command line diffs
//! with the indent heuristic instead (`diff.indentHeuristic`, on by default),
//! which slides a changed run to a more readable boundary where several are
//! equally minimal. So a file whose edits could be placed more than one way is
//! attributed here the way cgit attributes it, which can be a line or two from
//! what a terminal shows. Everything else agrees exactly, and the differential
//! tests hold it to that against `git blame` itself.
//!
//! # What is not here
//!
//! Renames. git runs a second pass over each parent with rename detection on
//! (`find_rename`), so a file that was renamed keeps its history; this crate
//! does the first pass only, so blame stops cleanly at the commit that renamed
//! the file rather than following through it. Every `-M`/`-C` variant, the
//! fingerprint engine behind `--ignore-rev`, and reverse blame are out of
//! scope as well — cgit asks for none of them.
//!
//! # IO
//!
//! None, directly: objects arrive through [`CommitSource`], the same trait
//! `gib-log` walks history with, so blame runs unchanged over a browser's
//! IndexedDB-backed object store and over an on-disk one in the tests. Nothing
//! is rendered here either — the result is a list of [`BlameGroup`]s.

#![deny(clippy::all)]

use gib_commitgraph::bloom::BloomSettings;
use gib_log::{CommitSource, Frontier, MetaCache, bloom_says_unchanged};
use gib_object::{Commit, ObjectId, TreeEntryType};
use gib_xdiff::DiffFailed;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::rc::Rc;

#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A run of consecutive lines in the file that all came from the same commit.
///
/// Line numbers are zero-based and the run is `num_lines` long, so it covers
/// `start..start + num_lines` of the file being blamed, which was
/// `orig_start..orig_start + num_lines` of the same file back in `commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameGroup {
    /// The commit that introduced these lines.
    pub commit: ObjectId,
    /// The path the file had in that commit. Always the path being blamed
    /// today, until rename following lands.
    pub path: String,
    /// The first line of the run, zero-based, in the file being blamed.
    pub start: usize,
    /// How many lines the run covers.
    pub num_lines: usize,
    /// The first line of the run, zero-based, as it sat in `commit`'s copy of
    /// the file — what a link to the file at that revision should point at.
    pub orig_start: usize,
    /// `commit`'s first parent, or `None` for a root commit. Carried because
    /// the walk already knows it and a view wants it: cgit puts a `^` beside
    /// each run that blames the same file one revision further back, and
    /// finding that parent again would be a second lookup per run.
    pub parent: Option<ObjectId>,
}

/// A finished blame: every line of the file, grouped by the commit it came
/// from.
pub struct Blame {
    /// The groups, in file order, covering every line exactly once.
    pub groups: Vec<BlameGroup>,
    /// How many lines the file has, counted as git counts them: a final line
    /// with no newline after it still counts.
    pub num_lines: usize,
    /// What the walk cost; see [`BlameStats`].
    pub stats: BlameStats,
}

/// Why a blame could not be produced.
///
/// Only the starting point fails this way. Once the walk is under way an object
/// that cannot be read stops the search along that edge instead of failing the
/// whole blame: the lines are simply attributed to the oldest commit that could
/// be reached, which is what a partially-fetched repository should show rather
/// than an error page.
#[derive(Debug)]
pub enum BlameError {
    /// The path names nothing in the starting commit, or names something that
    /// is not a file — a directory, or a submodule.
    NotAFile,
    /// The starting commit, its tree, or the file's own blob could not be read.
    Object(anyhow::Error),
    /// xdiff could not diff two revisions of the file. In a browser this means
    /// the tab is out of memory.
    Diff(DiffFailed),
}

impl fmt::Display for BlameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlameError::NotAFile => f.write_str("not a file in this revision"),
            BlameError::Object(e) => write!(f, "{e}"),
            BlameError::Diff(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BlameError {}

impl From<DiffFailed> for BlameError {
    fn from(value: DiffFailed) -> Self {
        BlameError::Diff(value)
    }
}

/// Counters for one [`blame`] call. Blame is the most object-hungry view in the
/// app — a blob per commit that touched the file — so what these say about the
/// commit-graph carrying the traversal, and about the Bloom filters keeping
/// trees unread, is worth putting in front of whoever is looking at it being
/// slow.
#[derive(Default)]
pub struct BlameStats {
    /// Commits whose lines were passed to their parents.
    pub commits: usize,
    /// Commits whose metadata came from the commit-graph (no object fetch).
    pub graph_meta_hits: usize,
    /// Commits whose metadata required fetching the commit object instead.
    pub object_meta_fallbacks: usize,
    /// Parents whose tree was never walked, because the commit's changed-path
    /// filter ruled the path out.
    pub bloom_skips: usize,
    /// Parents whose tree had to be walked to find the file.
    pub tree_walks: usize,
    /// Blobs read, each one an object fetch.
    pub blobs_read: usize,
    /// Diffs run against a parent's copy of the file.
    pub diffs: usize,
}

impl fmt::Display for BlameStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blamed {} commits (graph-meta {}, object-meta {}), \
             parents: {} Bloom-skips / {} tree-walks, {} blobs, {} diffs",
            self.commits,
            self.graph_meta_hits,
            self.object_meta_fallbacks,
            self.bloom_skips,
            self.tree_walks,
            self.blobs_read,
            self.diffs,
        )
    }
}

/// Blame `path` as of `head_commit`, calling `on_progress` with the groups
/// settled so far whenever more lines become final, so a view can fill its
/// gutter in as the walk runs instead of waiting for the oldest commit.
///
/// The groups handed to `on_progress` are final: a line's attribution never
/// changes once it lands there. They do not yet cover the whole file, which is
/// why they carry their own line numbers rather than arriving in file order.
///
/// `on_progress` returns a future, which the walk awaits before going on. In a
/// browser that is what lets the caller hand the renderer a turn between
/// commits: blame is a long serial walk, and a callback that could only write
/// into a state cell would queue every repaint behind the whole of it.
pub async fn blame<S, F, Fut>(
    head_commit: &Commit,
    source: &S,
    path: &str,
    on_progress: F,
) -> Result<Blame, BlameError>
where
    S: CommitSource,
    F: Fn(&[BlameGroup]) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut sb = Scoreboard::new(source, path);
    let num_lines = sb.setup(head_commit).await?;
    if num_lines > 0 {
        sb.assign_blame(&on_progress).await?;
    }
    Ok(Blame {
        groups: sb.finish(),
        num_lines,
        stats: sb.stats,
    })
}

// ---------------------------------------------------------------------------
// Origins and entries
// ---------------------------------------------------------------------------

/// An origin's index in the scoreboard's arena.
///
/// git refcounts origins and frees them as the walk moves on; here they are
/// kept in a `Vec` for the duration, which is what removes the refcounting
/// entirely. An origin is one small struct per (commit, path) actually reached,
/// so the arena is bounded by the number of commits that touched the file —
/// the blobs they hold are the memory that matters, and those are still
/// dropped as soon as they are done with, exactly where git's
/// `drop_origin_blob` drops them.
type OriginId = usize;

/// Whether a tree entry names a file's contents or a symlink's target. git
/// compares the two as `S_IFMT` bits and calls a change between them a type
/// change, which ends the search: the "same" path in the parent is a different
/// kind of object, so its bytes are not an earlier version of these lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlobKind {
    /// A regular file, executable or not — git treats the permission bit as an
    /// ordinary modification, not a type change.
    File,
    Symlink,
}

/// One blob, at one path, in one commit, plus the lines still suspected of
/// having come from it.
struct Origin {
    commit: ObjectId,
    path: Rc<str>,
    blob: ObjectId,
    kind: BlobKind,
    /// The blob's bytes, once read. Dropped as soon as the origin is done with,
    /// so a deep history doesn't hold every revision of the file at once.
    content: Option<Rc<Vec<u8>>>,
    /// Lines suspected of coming from here, sorted by [`Entry::s_lno`].
    suspects: Vec<Entry>,
}

/// A run of lines being blamed: where it sits in the file today (`lno`), where
/// it sits in the suspected origin's copy (`s_lno`), and how long it is.
///
/// git's `blame_entry` carries a `score`, `ignored` and `unblamable` too; all
/// three exist for engines this port leaves out (`-M`/`-C` scoring and
/// `--ignore-rev`), so they are absent rather than carried and never read.
#[derive(Clone, Copy)]
struct Entry {
    /// First line of the run in the final image, zero-based.
    lno: usize,
    num_lines: usize,
    /// First line of the run in the suspect's copy of the file, zero-based.
    s_lno: usize,
    suspect: OriginId,
}

// ---------------------------------------------------------------------------
// The scoreboard
// ---------------------------------------------------------------------------

struct Scoreboard<'a, S> {
    source: &'a S,
    /// The path being blamed, pre-split for tree walks.
    components: Vec<String>,
    path: Rc<str>,
    bloom_settings: Option<BloomSettings>,
    origins: Vec<Origin>,
    /// The origins belonging to each commit. git hangs these off the commit
    /// itself; a map keeps the same lookup without decorating objects.
    by_commit: BTreeMap<ObjectId, Vec<OriginId>>,
    meta: MetaCache,
    /// Commits with lines still to account for, newest first.
    queue: Frontier,
    /// Lines whose commit is settled.
    settled: Vec<Entry>,
    stats: BlameStats,
}

impl<'a, S: CommitSource> Scoreboard<'a, S> {
    fn new(source: &'a S, path: &str) -> Self {
        Self {
            source,
            components: path
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            path: Rc::from(path),
            bloom_settings: source.bloom_settings(),
            origins: Vec::new(),
            by_commit: BTreeMap::new(),
            meta: MetaCache::new(),
            queue: Frontier::new(),
            settled: Vec::new(),
            stats: BlameStats::default(),
        }
    }

    /// Seed the walk with the file as `head_commit` has it: one origin, holding
    /// one entry that suspects it of every line. Returns the file's line count.
    async fn setup(&mut self, head_commit: &Commit) -> Result<usize, BlameError> {
        let meta = self
            .meta
            .get(self.source, head_commit.id(), Some(head_commit))
            .await
            .ok_or_else(|| {
                BlameError::Object(anyhow::anyhow!("cannot read commit {}", head_commit.id()))
            })?;
        let (blob, kind) = self
            .resolve_path(meta.tree)
            .await
            .ok_or(BlameError::NotAFile)?;
        let origin = self.make_origin(head_commit.id(), blob, kind);
        let content = self.read_blob(origin).await.map_err(BlameError::Object)?;
        let num_lines = count_lines(&content);
        self.origins[origin].suspects = vec![Entry {
            lno: 0,
            num_lines,
            s_lno: 0,
            suspect: origin,
        }];
        self.queue.push(meta.time, head_commit.id());
        Ok(num_lines)
    }

    /// The main loop, `blame.c`'s `assign_blame`: take the newest commit that
    /// still has lines pinned on it, let it hand away everything it can prove
    /// came from a parent, and keep what is left.
    async fn assign_blame<F, Fut>(&mut self, on_progress: &F) -> Result<(), BlameError>
    where
        F: Fn(&[BlameGroup]) -> Fut,
        Fut: Future<Output = ()>,
    {
        while let Some(commit) = self.queue.pop() {
            // A commit is queued once per batch of lines passed to it, so the
            // same commit can come up again with nothing left to do.
            let Some(origin) = self.unfinished_origin(commit) else {
                continue;
            };
            self.pass_blame(origin).await?;
            // Whatever no parent would take is this commit's doing.
            let remaining = std::mem::take(&mut self.origins[origin].suspects);
            if !remaining.is_empty() {
                self.settled.extend(remaining);
                on_progress(&self.groups()).await;
            }
            self.origins[origin].content = None;
        }
        Ok(())
    }

    /// The commit's first origin that still has lines pinned on it.
    fn unfinished_origin(&self, commit: ObjectId) -> Option<OriginId> {
        self.by_commit
            .get(&commit)?
            .iter()
            .copied()
            .find(|&o| !self.origins[o].suspects.is_empty())
    }

    /// Hand `origin`'s lines to whichever parents can be shown to have had
    /// them. `blame.c`'s `pass_blame`, with the move/copy and ignore-rev arms
    /// removed.
    async fn pass_blame(&mut self, origin: OriginId) -> Result<(), BlameError> {
        let commit = self.origins[origin].commit;
        let Some(meta) = self.meta.get(self.source, commit, None).await else {
            return Ok(());
        };
        if meta.parents.is_empty() {
            return Ok(());
        }

        // Find each parent's copy of the file first. A parent holding the very
        // same blob means this commit changed nothing here, and the whole
        // question moves to it untouched.
        let mut scapegoats: Vec<Option<OriginId>> = vec![None; meta.parents.len()];
        for (i, parent) in meta.parents.iter().copied().enumerate() {
            let Some(porigin) = self.find_origin(parent, origin, i == 0).await else {
                continue;
            };
            if self.origins[porigin].blob == self.origins[origin].blob {
                self.pass_whole_blame(origin, porigin);
                return Ok(());
            }
            // Two parents carrying the same blob are one question, not two.
            let seen = scapegoats[..i]
                .iter()
                .flatten()
                .any(|&other| self.origins[other].blob == self.origins[porigin].blob);
            if !seen {
                scapegoats[i] = Some(porigin);
            }
        }

        self.stats.commits += 1;
        for porigin in scapegoats.iter().flatten().copied() {
            self.pass_blame_to_parent(origin, porigin).await?;
            if self.origins[origin].suspects.is_empty() {
                break;
            }
        }
        // A parent nothing was passed to won't be visited, so its blob is dead
        // weight; git drops it here for the same reason.
        for porigin in scapegoats.iter().flatten().copied() {
            if self.origins[porigin].suspects.is_empty() {
                self.origins[porigin].content = None;
            }
        }
        Ok(())
    }

    /// The origin for `origin`'s path in `parent`, or `None` if the file wasn't
    /// there — added by this commit, or a different kind of object.
    ///
    /// `first_parent` enables the changed-path Bloom filter shortcut: the
    /// filter describes the commit against its first parent alone, so a
    /// conclusive "unchanged" there means the parent has the identical blob and
    /// no tree needs walking at all. git takes exactly this shortcut, in
    /// `find_origin` via `maybe_changed_path`.
    async fn find_origin(
        &mut self,
        parent: ObjectId,
        origin: OriginId,
        first_parent: bool,
    ) -> Option<OriginId> {
        // An origin already made for this (commit, path) is the one to use, so
        // that lines arriving from two children pile up on one origin.
        if let Some(existing) = self.by_commit.get(&parent).and_then(|origins| {
            origins
                .iter()
                .copied()
                .find(|&o| self.origins[o].path == self.origins[origin].path)
        }) {
            return Some(existing);
        }

        let commit = self.origins[origin].commit;
        let bloom_unchanged = first_parent
            && self.meta.cached(&commit).is_some_and(|meta| {
                bloom_says_unchanged(
                    self.bloom_settings.as_ref(),
                    meta.bloom.as_deref(),
                    &self.path,
                )
            });

        // The parent's own metadata is needed either way: an origin that is
        // queued has to be queued at its commit's date.
        let parent_meta = self.meta.get(self.source, parent, None).await?;

        let (blob, kind) = if bloom_unchanged {
            self.stats.bloom_skips += 1;
            (self.origins[origin].blob, self.origins[origin].kind)
        } else {
            self.stats.tree_walks += 1;
            let (blob, kind) = self.resolve_path(parent_meta.tree).await?;
            // A file where the parent had a symlink (or the reverse) is git's
            // 'T', which ends the search rather than diffing the two.
            if kind != self.origins[origin].kind {
                return None;
            }
            (blob, kind)
        };
        Some(self.make_origin(parent, blob, kind))
    }

    /// The parent's file is byte-identical, so this commit is not responsible
    /// for any of these lines: move them across whole. `blame.c`'s
    /// `pass_whole_blame`.
    fn pass_whole_blame(&mut self, origin: OriginId, porigin: OriginId) {
        if self.origins[porigin].content.is_none() {
            self.origins[porigin].content = self.origins[origin].content.take();
        }
        let mut suspects = std::mem::take(&mut self.origins[origin].suspects);
        for entry in &mut suspects {
            entry.suspect = porigin;
        }
        self.queue_blames(porigin, suspects);
    }

    /// Diff the parent's copy of the file against this one and pass on every
    /// line the diff says is unchanged. `blame.c`'s `pass_blame_to_parent`,
    /// including its closing `blame_chunk` over the common tail.
    async fn pass_blame_to_parent(
        &mut self,
        target: OriginId,
        parent: OriginId,
    ) -> Result<(), BlameError> {
        if self.origins[target].suspects.is_empty() {
            return Ok(());
        }
        // A blob that cannot be read stops the search here rather than failing
        // the blame: these lines stay pinned on the commit we are looking at.
        let Ok(file_p) = self.read_blob(parent).await else {
            return Ok(());
        };
        let Ok(file_o) = self.read_blob(target).await else {
            return Ok(());
        };
        let hunks = gib_xdiff::hunks(&file_p, &file_o)?;
        self.stats.diffs += 1;

        let mut src: VecDeque<Entry> = std::mem::take(&mut self.origins[target].suspects).into();
        let mut passed: Vec<Entry> = Vec::new();
        let mut kept: Vec<Entry> = Vec::new();
        let mut offset: isize = 0;
        for hunk in &hunks {
            // xdiff walks both files forwards, so the running offset between
            // the two sides has to be what this hunk starts at; git asserts the
            // same thing in `blame_chunk_cb`.
            debug_assert_eq!(
                hunk.before.start as isize - hunk.after.start as isize,
                offset
            );
            blame_chunk(
                &mut passed,
                &mut src,
                &mut kept,
                hunk.after.start,
                hunk.before.start as isize - hunk.after.start as isize,
                hunk.after.end,
                parent,
            );
            offset = hunk.before.end as isize - hunk.after.end as isize;
        }
        // Everything past the last hunk is common to both files.
        blame_chunk(
            &mut passed,
            &mut src,
            &mut kept,
            usize::MAX,
            offset,
            usize::MAX,
            parent,
        );

        self.origins[target].suspects = kept;
        self.queue_blames(parent, passed);
        Ok(())
    }

    /// Merge a batch of lines into a parent's origin, queueing its commit if
    /// nothing was waiting on it yet. `blame.c`'s `queue_blames`.
    ///
    /// The queueing happens even when the batch is empty, as it does in git: a
    /// commit that turns out to have nothing to do is simply skipped when it
    /// comes up, and mirroring the queue exactly keeps the order commits are
    /// examined in — which is what decides ties — identical to git's.
    fn queue_blames(&mut self, porigin: OriginId, sorted: Vec<Entry>) {
        if !self.origins[porigin].suspects.is_empty() {
            let existing = std::mem::take(&mut self.origins[porigin].suspects);
            self.origins[porigin].suspects = blame_merge(existing, sorted);
            return;
        }
        let commit = self.origins[porigin].commit;
        let waiting = self.unfinished_origin(commit).is_some();
        self.origins[porigin].suspects = sorted;
        if !waiting {
            let time = self.meta.cached(&commit).map_or(0, |meta| meta.time);
            self.queue.push(time, commit);
        }
    }

    /// The settled lines as groups, in file order, with adjacent runs from the
    /// same origin merged — `blame_sort_final` followed by `blame_coalesce`.
    fn groups(&self) -> Vec<BlameGroup> {
        let mut entries: Vec<Entry> = self.settled.clone();
        entries.sort_by_key(|e| e.lno);
        let mut groups: Vec<BlameGroup> = Vec::with_capacity(entries.len());
        for entry in entries {
            let origin = &self.origins[entry.suspect];
            // Two entries coalesce when they are the same origin's lines and
            // are contiguous on both sides — in the file today and in the
            // origin's copy of it.
            if let Some(last) = groups.last_mut()
                && last.commit == origin.commit
                && last.path == *origin.path
                && last.start + last.num_lines == entry.lno
                && last.orig_start + last.num_lines == entry.s_lno
            {
                last.num_lines += entry.num_lines;
                continue;
            }
            groups.push(BlameGroup {
                commit: origin.commit,
                path: origin.path.to_string(),
                start: entry.lno,
                num_lines: entry.num_lines,
                orig_start: entry.s_lno,
                // Settled lines have been through `pass_blame`, so their
                // commit's metadata is already in the cache.
                parent: self
                    .meta
                    .cached(&origin.commit)
                    .and_then(|meta| meta.parents.first().copied()),
            });
        }
        groups
    }

    fn finish(&mut self) -> Vec<BlameGroup> {
        self.stats.graph_meta_hits = self.meta.graph_hits;
        self.stats.object_meta_fallbacks = self.meta.object_fallbacks;
        self.groups()
    }

    // -- object access ------------------------------------------------------

    /// Register a new origin for `commit`.
    fn make_origin(&mut self, commit: ObjectId, blob: ObjectId, kind: BlobKind) -> OriginId {
        let id = self.origins.len();
        self.origins.push(Origin {
            commit,
            path: Rc::clone(&self.path),
            blob,
            kind,
            content: None,
            suspects: Vec::new(),
        });
        self.by_commit.entry(commit).or_default().push(id);
        id
    }

    /// An origin's blob, read once and held until the origin is done with.
    async fn read_blob(&mut self, origin: OriginId) -> anyhow::Result<Rc<Vec<u8>>> {
        if let Some(content) = &self.origins[origin].content {
            return Ok(Rc::clone(content));
        }
        let id = self.origins[origin].blob;
        let object = self.source.object(id).await?;
        let blob = object
            .blob()
            .map_err(|e| anyhow::anyhow!("{id} is not a blob: {e:?}"))?;
        self.stats.blobs_read += 1;
        let content = Rc::new(blob.data_owned());
        self.origins[origin].content = Some(Rc::clone(&content));
        Ok(content)
    }

    /// Walk `tree` down to the blamed path, reporting what kind of blob sits
    /// there. `None` if the path is absent, or names a directory or submodule.
    async fn resolve_path(&self, tree: ObjectId) -> Option<(ObjectId, BlobKind)> {
        let (last, dirs) = self.components.split_last()?;
        let mut current = self.source.object(tree).await.ok()?.tree().ok()?;
        for component in dirs {
            let entry = current
                .entries()
                .find(|e| e.name() == component.as_bytes())?;
            if entry.entry_type() != TreeEntryType::Tree {
                return None;
            }
            current = self.source.object(entry.id()).await.ok()?.tree().ok()?;
        }
        let entry = current.entries().find(|e| e.name() == last.as_bytes())?;
        let kind = match entry.entry_type() {
            TreeEntryType::File | TreeEntryType::Executable => BlobKind::File,
            TreeEntryType::Symlink => BlobKind::Symlink,
            TreeEntryType::Tree | TreeEntryType::Commit => return None,
        };
        Some((entry.id(), kind))
    }
}

// ---------------------------------------------------------------------------
// The chunk walk
// ---------------------------------------------------------------------------

/// Move the lines one diff hunk accounts for, `blame.c`'s `blame_chunk`.
///
/// `src` holds the target's remaining suspected lines, sorted and consumed in
/// order as the hunks are walked; the diff says that everything before `tlno`
/// is unchanged (sitting `offset` lines away in the parent) and that
/// `tlno..same` is the changed part. So the lines before `tlno` go to `passed`,
/// re-based onto the parent, and the lines inside the changed part go to
/// `kept`, where they stay pinned on the target. Runs that straddle either
/// boundary are split, and the far half is pushed back for the next hunk.
///
/// git achieves the same with three levels of pointer-to-pointer and lists
/// built backwards and reversed; the queues here are the same lists, in order.
fn blame_chunk(
    passed: &mut Vec<Entry>,
    src: &mut VecDeque<Entry>,
    kept: &mut Vec<Entry>,
    tlno: usize,
    offset: isize,
    same: usize,
    parent: OriginId,
) {
    // Everything before the hunk came from the parent unchanged.
    let mut straddling: Vec<Entry> = Vec::new();
    while src.front().is_some_and(|e| e.s_lno < tlno) {
        let mut entry = src.pop_front().expect("checked above");
        if entry.s_lno + entry.num_lines > tlno {
            // The run reaches into the changed part: only its head came from
            // the parent, so the tail goes back for the loop below.
            let (len, suspect) = (tlno - entry.s_lno, entry.suspect);
            straddling.push(split_at(&mut entry, len, suspect));
        }
        entry.suspect = parent;
        // The diff never moves a line to a negative position: `offset` is the
        // running difference between two walks over the same lines.
        debug_assert!(entry.s_lno.checked_add_signed(offset).is_some());
        entry.s_lno = entry.s_lno.wrapping_add_signed(offset);
        passed.push(entry);
    }
    push_front_in_order(src, straddling);

    // The hunk's own lines are not in the parent, so they stay here.
    let mut straddling: Vec<Entry> = Vec::new();
    while src.front().is_some_and(|e| e.s_lno < same) {
        let mut entry = src.pop_front().expect("checked above");
        if entry.s_lno + entry.num_lines > same {
            // The run continues past the hunk; that part is a later hunk's (or
            // the closing chunk's) business.
            let (len, suspect) = (same - entry.s_lno, entry.suspect);
            straddling.push(split_at(&mut entry, len, suspect));
        }
        kept.push(entry);
    }
    push_front_in_order(src, straddling);
}

/// Cut `entry` down to its first `len` lines and return the rest, blamed on
/// `suspect`. `blame.c`'s `split_blame_at`.
fn split_at(entry: &mut Entry, len: usize, suspect: OriginId) -> Entry {
    let rest = Entry {
        lno: entry.lno + len,
        num_lines: entry.num_lines - len,
        s_lno: entry.s_lno + len,
        suspect,
    };
    entry.num_lines = len;
    rest
}

/// Put split-off runs back at the head of the queue, keeping their order.
fn push_front_in_order(src: &mut VecDeque<Entry>, entries: Vec<Entry>) {
    for entry in entries.into_iter().rev() {
        src.push_front(entry);
    }
}

/// Merge two runs of lines that are each sorted by `s_lno` into one that is.
/// `blame.c`'s `blame_merge`, keeping `a` ahead of `b` where they tie.
fn blame_merge(a: Vec<Entry>, b: Vec<Entry>) -> Vec<Entry> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    loop {
        let take_a = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => x.s_lno <= y.s_lno,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        out.push(if take_a {
            a.next().expect("peeked")
        } else {
            b.next().expect("peeked")
        });
    }
    out
}

/// How many lines `data` holds, counted as git's `find_line_starts` counts
/// them: one per newline, plus one more for a final line that isn't
/// newline-terminated. This has to agree with how xdiff splits the same bytes
/// into records, since every line number in a blame indexes that split.
fn count_lines(data: &[u8]) -> usize {
    let newlines = data.iter().filter(|&&b| b == b'\n').count();
    if data.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}
