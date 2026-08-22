//! Walking a repository's history, the way `git log` does.
//!
//! Two walks live here. [`walk_commits`] is the log view's: a commit-time
//! ordered frontier, a `skip`/`limit` window over the commits that match an
//! optional path filter, and the knowledge of whether a further page exists.
//! [`recent_commits`] is the cheap one the summary page wants — the newest
//! handful of commits, unfiltered, read straight from commit objects.
//!
//! Neither does any IO of its own. Objects and commit-graph records arrive
//! through the caller's [`CommitSource`], which is what lets the same walk run
//! over a browser's IndexedDB-backed object store and over a plain on-disk one
//! in the differential tests. Neither renders anything either: a walk hands
//! back [`Commit`]s and a [`WalkStats`], and the caller turns those into
//! whatever its UI wants.
//!
//! The commit-graph is what makes traversal cheap: a commit's tree, parents and
//! commit time come from it with no object fetch at all, and its changed-path
//! Bloom filters discard most commits on a filtered log before a tree is read.
//! Anything the graph cannot answer falls back to reading the commit object, so
//! a repository without a commit-graph still walks, only slower.

#![deny(clippy::all)]

use futures::future::LocalBoxFuture;
use gib_commitgraph::bloom::{BloomSettings, path_maybe_changed};
use gib_object::{Commit, Object, ObjectId, Tree, TreeEntryType};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fmt;
use std::rc::Rc;

#[cfg(test)]
mod differential;

/// What the history walk needs about one commit, derived from the commit-graph
/// and cached per OID. `bloom` is the changed-path filter (`None` ⇒ treat as
/// "maybe", i.e. fall back to a real diff).
pub struct GraphRecord {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit_time: i64,
    pub bloom: Option<Vec<u8>>,
}

/// Where a walk reads a repository from.
///
/// Implementors decide how objects are fetched and how (or whether) they are
/// cached; the walk only ever asks. [`graph_record`](CommitSource::graph_record)
/// and [`bloom_settings`](CommitSource::bloom_settings) are the commit-graph
/// accelerators — an implementation with no commit-graph returns `None` from
/// both and every walk still works, reading commit objects instead.
pub trait CommitSource {
    /// Read one object, by id.
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>>;

    /// A commit's commit-graph record, or `None` if there is no commit-graph or
    /// the commit isn't in it — the walk then falls back to the object.
    fn graph_record(&self, id: ObjectId) -> LocalBoxFuture<'_, Option<Rc<GraphRecord>>>;

    /// The changed-path Bloom settings of the repository's commit-graph, needed
    /// to read the filters carried by [`GraphRecord::bloom`]. `None` disables
    /// Bloom skipping, leaving every candidate to a real tree diff.
    fn bloom_settings(&self) -> Option<BloomSettings>;
}

/// The id of the entry named `name` in `tree`, or `None` if absent.
fn entry_id(tree: &Tree, name: &str) -> Option<ObjectId> {
    tree.entries()
        .find(|e| e.name() == name.as_bytes())
        .map(|e| e.id())
}

/// Resolve a slash-separated path to the [`ObjectId`] it points at within the
/// tree `tree_id` — a blob id for a file, a tree id for a directory. Returns
/// `None` if the path does not exist in that tree. Only used for root commits,
/// which have no parent to diff against.
async fn path_object_id<S: CommitSource>(
    tree_id: ObjectId,
    components: &[&str],
    source: &S,
) -> Option<ObjectId> {
    let (last, dirs) = components.split_last()?;
    let mut current = source.object(tree_id).await.ok()?.tree().ok()?;
    for component in dirs {
        let entry = current
            .entries()
            .find(|e| e.name() == component.as_bytes())?;
        if entry.entry_type() != TreeEntryType::Tree {
            return None;
        }
        current = source.object(entry.id()).await.ok()?.tree().ok()?;
    }
    entry_id(&current, last)
}

/// Whether the object at `components` differs between trees `a` and `b`.
///
/// Walks both trees in lockstep, comparing the entry id for each path component.
/// As soon as the two ids match, the entire subtree below is byte-identical, so
/// the path is unchanged and we stop — the deep trees are never fetched. We only
/// descend as far as the path actually diverged between the two commits, which
/// for most commits is zero levels (they touched a different part of the tree).
async fn path_differs<S: CommitSource>(
    a: ObjectId,
    b: ObjectId,
    components: &[&str],
    source: &S,
) -> bool {
    let Some((last, dirs)) = components.split_last() else {
        return false;
    };
    // Identical (sub)tree id ⇒ everything beneath is identical ⇒ no change.
    let mut a = a;
    let mut b = b;
    for component in dirs {
        if a == b {
            return false;
        }
        // Fetch both sides' trees concurrently to halve this level's latency.
        let (ta, tb) = match futures::join!(source.object(a), source.object(b)) {
            (Ok(oa), Ok(ob)) => match (oa.tree(), ob.tree()) {
                (Ok(ta), Ok(tb)) => (ta, tb),
                _ => return true,
            },
            _ => return true,
        };
        let (ea, eb) = (entry_id(&ta, component), entry_id(&tb, component));
        match (ea, eb) {
            // Subtree present on both sides with the same id: pruned, unchanged.
            (Some(x), Some(y)) if x == y => return false,
            // Present on both but different: descend into the two subtrees.
            (Some(x), Some(y)) => (a, b) = (x, y),
            // Present on only one side (added/removed dir): the path changed.
            _ => return true,
        }
    }
    if a == b {
        return false;
    }
    let (ta, tb) = match futures::join!(source.object(a), source.object(b)) {
        (Ok(oa), Ok(ob)) => match (oa.tree(), ob.tree()) {
            (Ok(ta), Ok(tb)) => (ta, tb),
            _ => return true,
        },
        _ => return true,
    };
    entry_id(&ta, last) != entry_id(&tb, last)
}

/// Whether `bloom` (a commit's changed-path filter, read with `settings`)
/// definitively says the path did not change. `false` means "unknown" — no
/// filter, no settings, or a possible match — so the caller must diff.
fn bloom_says_unchanged(
    settings: Option<&BloomSettings>,
    bloom: Option<&[u8]>,
    path: &str,
) -> bool {
    let (Some(bytes), Some(settings)) = (bloom, settings) else {
        return false;
    };
    !path_maybe_changed(bytes, settings, path.as_bytes())
}

/// Counters for one [`walk_commits`] call, worth logging so it's visible
/// whether the commit-graph (and its Bloom filters) is actually doing the work:
/// graph hits should dominate fallbacks, and on a filtered log most commits
/// should be Bloom-skipped rather than tree-diffed.
///
/// The [`Display`](fmt::Display) impl is the one-line summary; callers wrap it
/// in whatever context (which path, how many rows) their log line wants.
#[derive(Default)]
pub struct WalkStats {
    /// Commits popped from the frontier, matching or not.
    pub traversed: usize,
    /// Commits whose metadata came from the commit-graph (no object fetch).
    pub graph_meta_hits: usize,
    /// Commits whose metadata required fetching the commit object instead.
    pub object_meta_fallbacks: usize,
    /// Filtered commits skipped via the Bloom filter with no tree fetch.
    pub bloom_skips: usize,
    /// Filtered commits that needed a real tree diff.
    pub tree_diffs: usize,
}

impl fmt::Display for WalkStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "traversed {} commits (graph-meta {}, object-meta {}), \
             filter: {} Bloom-skips / {} tree-diffs",
            self.traversed,
            self.graph_meta_hits,
            self.object_meta_fallbacks,
            self.bloom_skips,
            self.tree_diffs,
        )
    }
}

/// One page of history, as [`walk_commits`] found it.
pub struct LogPage {
    /// The page's commits, newest first — at most the `limit` asked for.
    pub commits: Vec<Commit>,
    /// Whether a further matching commit exists past this page.
    pub has_more: bool,
    /// What the walk cost; see [`WalkStats`].
    pub stats: WalkStats,
}

/// The traversal data a single commit contributes to a log walk: enough to
/// order the frontier (`time`), continue it (`parents`), filter by path
/// (`tree`, plus `bloom` for the Bloom filter), all without re-parsing objects.
struct WalkNode {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    time: i64,
    /// The commit's changed-path Bloom filter, if the commit-graph has one.
    bloom: Option<Vec<u8>>,
}

/// Look up a commit's [`WalkNode`], memoised in `cache`. Prefers the
/// commit-graph (no object fetch); otherwise falls back to the commit object,
/// using `known` when the caller already holds it (the walk's starting commit).
/// `None` only if the commit can be found neither way.
///
/// Whether [`CommitSource::graph_record`] is a cheap in-memory hit is the
/// implementation's business — in webgit the graph is bulk-loaded once and
/// persisted, so traversal is in-memory and survives reloads.
async fn ensure_node<S: CommitSource>(
    source: &S,
    cache: &mut BTreeMap<ObjectId, Rc<WalkNode>>,
    id: ObjectId,
    known: Option<&Commit>,
    stats: &mut WalkStats,
) -> Option<Rc<WalkNode>> {
    if let Some(node) = cache.get(&id) {
        return Some(Rc::clone(node));
    }
    let node = if let Some(rec) = source.graph_record(id).await {
        stats.graph_meta_hits += 1;
        WalkNode {
            tree: rec.tree,
            parents: rec.parents.clone(),
            time: rec.commit_time,
            bloom: rec.bloom.clone(),
        }
    } else {
        stats.object_meta_fallbacks += 1;
        let commit = match known {
            Some(c) => c.clone(),
            None => source.object(id).await.ok()?.commit().ok()?,
        };
        WalkNode {
            tree: commit.tree(),
            parents: commit.parents().to_vec(),
            time: commit.commit_date().timestamp().as_second(),
            bloom: None,
        }
    };
    let node = Rc::new(node);
    cache.insert(id, Rc::clone(&node));
    Some(node)
}

/// A Bloom candidate awaiting confirmation by a real tree diff. It carries the
/// commit's tree and its parents' trees (all resolved up front from the
/// in-memory graph), so [`confirm_task`] touches no shared mutable state and a
/// batch of them can run concurrently.
struct ConfirmTask {
    id: ObjectId,
    tree: ObjectId,
    parent_trees: Vec<ObjectId>,
    root: bool,
}

/// Whether a candidate actually changed the path, mirroring git's default
/// simplification: a root is shown when the path exists; otherwise a commit is
/// shown unless it is TREESAME to (same object at the path as) some parent.
/// Read-only, so many of these can be awaited together.
async fn confirm_task<S: CommitSource>(
    source: &S,
    task: &ConfirmTask,
    components: &[&str],
) -> bool {
    if task.root {
        return path_object_id(task.tree, components, source)
            .await
            .is_some();
    }
    for parent_tree in &task.parent_trees {
        if !path_differs(task.tree, *parent_tree, components, source).await {
            return false;
        }
    }
    true
}

/// How many candidate tree-diffs to confirm concurrently. Traversal is in-memory
/// so the only latency is these diffs; batching turns ~N serial round-trips into
/// ~N/BATCH waves (relies on HTTP/2 multiplexing the per-diff object fetches).
///
/// Each diff fetches its two trees concurrently (see [`path_differs`]), so peak
/// in-flight streams are ~`2 × CONFIRM_BATCH` — kept under typical HTTP/2 limits
/// (servers cap concurrent streams around 100–128). The batch that crosses the
/// page boundary is fully confirmed, so a larger value also fetches up to
/// `BATCH` candidates of slack past the last shown commit; 64 keeps that cost
/// negligible against a deep walk while roughly halving the wave count vs 32.
const CONFIRM_BATCH: usize = 64;

/// How many commit objects to fetch (concurrently) before emitting a partial
/// page during a streamed walk. Small enough that the first rows paint quickly,
/// large enough to keep the round-trips well overlapped.
const STREAM_BATCH: usize = 10;

/// Walk history a page at a time, calling `on_batch` with the commits gathered
/// so far after each chunk of commit objects is fetched, so a log view can
/// render progressively instead of waiting for the whole page. The return value
/// is the complete page plus whether a further page exists.
///
/// `path` filters to commits that changed that path (git's default
/// simplification); `None` walks the full history. `skip` and `limit` are the
/// pagination window over the matching commits.
pub async fn walk_commits<S: CommitSource>(
    head_commit: &Commit,
    source: &S,
    path: Option<&str>,
    skip: usize,
    limit: usize,
    on_batch: impl Fn(&[Commit]),
) -> LogPage {
    // Pre-split the pathspec once; `None` walks the full history unfiltered.
    let path_components: Option<Vec<&str>> =
        path.map(|p| p.split('/').filter(|s| !s.is_empty()).collect());

    // The frontier is ordered by commit time (newest first), tie-broken by id;
    // nodes carry only an id, with metadata memoised in `meta`.
    let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
    let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
    let mut meta: BTreeMap<ObjectId, Rc<WalkNode>> = BTreeMap::new();
    let mut stats = WalkStats::default();

    if let Some(node) = ensure_node(
        source,
        &mut meta,
        head_commit.id(),
        Some(head_commit),
        &mut stats,
    )
    .await
    {
        heap.push((node.time, head_commit.id()));
        visited.insert(head_commit.id());
    }

    // Collect the ids of matching commits in order. We need `skip + limit` to
    // fill the page, plus the detection of one more to know whether a next page
    // exists. `skip` comes straight from `?offset=` in the URL, so the sum is
    // saturating: near `usize::MAX` (reachable on wasm32, where that is only
    // 4 GiB) it would otherwise panic in debug builds and wrap in release,
    // turning a nonsense offset into a page of the wrong commits.
    let want = skip.saturating_add(limit);
    let mut matched: Vec<ObjectId> = Vec::new();
    let mut has_more = false;

    match &path_components {
        // Unfiltered: every traversed commit matches; pure in-memory traversal.
        None => {
            while let Some((_, id)) = heap.pop() {
                stats.traversed += 1;
                let node = meta.get(&id).map(Rc::clone).unwrap();
                if matched.len() == want {
                    has_more = true;
                    break;
                }
                matched.push(id);
                for parent in node.parents.iter().copied() {
                    if visited.insert(parent)
                        && let Some(parent_node) =
                            ensure_node(source, &mut meta, parent, None, &mut stats).await
                    {
                        heap.push((parent_node.time, parent));
                    }
                }
            }
        }
        // Filtered: traverse in memory to gather Bloom candidates in order, then
        // confirm them with tree diffs in concurrent batches.
        Some(components) => {
            let path_str = path.unwrap_or("");
            let bloom_settings = source.bloom_settings();
            let mut pending: Vec<ConfirmTask> = Vec::new();
            'outer: loop {
                // Refill the candidate buffer by traversing (in-memory) commits,
                // enqueueing parents and Bloom-skipping non-matches as we go.
                while pending.len() < CONFIRM_BATCH {
                    let Some((_, id)) = heap.pop() else { break };
                    stats.traversed += 1;
                    let node = meta.get(&id).map(Rc::clone).unwrap();
                    let mut parent_trees = Vec::with_capacity(node.parents.len());
                    for parent in node.parents.iter().copied() {
                        if let Some(parent_node) =
                            ensure_node(source, &mut meta, parent, None, &mut stats).await
                        {
                            if visited.insert(parent) {
                                heap.push((parent_node.time, parent));
                            }
                            parent_trees.push(parent_node.tree);
                        }
                    }
                    if node.parents.is_empty() {
                        pending.push(ConfirmTask {
                            id,
                            tree: node.tree,
                            parent_trees,
                            root: true,
                        });
                    } else if bloom_says_unchanged(
                        bloom_settings.as_ref(),
                        node.bloom.as_deref(),
                        path_str,
                    ) {
                        stats.bloom_skips += 1;
                    } else {
                        pending.push(ConfirmTask {
                            id,
                            tree: node.tree,
                            parent_trees,
                            root: false,
                        });
                    }
                }
                if pending.is_empty() {
                    break;
                }
                let batch_len = pending.len().min(CONFIRM_BATCH);
                let batch: Vec<ConfirmTask> = pending.drain(..batch_len).collect();
                stats.tree_diffs += batch.len();
                let results = futures::future::join_all(
                    batch
                        .iter()
                        .map(|task| confirm_task(source, task, components)),
                )
                .await;
                for (task, is_match) in batch.iter().zip(results) {
                    if is_match {
                        if matched.len() == want {
                            has_more = true;
                            break 'outer;
                        }
                        matched.push(task.id);
                    }
                }
            }
        }
    }

    // Fetch objects only for the commits actually shown — at most `limit`. Work
    // in chunks (each chunk fetched concurrently) so the first rows can render
    // while the rest are still in flight, emitting the commits so far after each.
    let window: Vec<ObjectId> = matched.into_iter().skip(skip).take(limit).collect();
    let mut commits: Vec<Commit> = Vec::with_capacity(window.len());
    for chunk in window.chunks(STREAM_BATCH) {
        let objects = futures::future::join_all(chunk.iter().map(|id| source.object(*id))).await;
        for object in objects {
            let Some(commit) = object.ok().and_then(|o| o.commit().ok()) else {
                continue;
            };
            commits.push(commit);
        }
        on_batch(&commits);
    }

    LogPage {
        commits,
        has_more,
        stats,
    }
}

/// Walk the most recent `limit` commits reachable from `head_commit` by fetching
/// commit objects directly, deliberately bypassing the commit-graph. For a small
/// bounded preview (the summary) this avoids the whole-file bulk load that
/// [`walk_commits`] may trigger on its first [`CommitSource::graph_record`] call
/// — a handful of cheap object reads (the same path ref rows use) instead of
/// downloading and persisting every commit's metadata just to show a teaser.
/// History is unfiltered, so there is no path/Bloom work; `on_batch` is called
/// with the commits so far as each commit object resolves, so they stream in
/// newest-first.
pub async fn recent_commits<S: CommitSource>(
    head_commit: &Commit,
    source: &S,
    limit: usize,
    on_batch: impl Fn(&[Commit]),
) -> Vec<Commit> {
    // Same frontier discipline as `walk_commits`'s unfiltered arm — a heap
    // ordered by commit time (newest first), tie-broken by id — but we hold the
    // resolved `Commit` for each frontier entry so popping a node both emits its
    // row and yields its parents to fetch, with no commit-graph in the loop.
    let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
    let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
    let mut frontier: BTreeMap<ObjectId, Commit> = BTreeMap::new();

    let head_id = head_commit.id();
    heap.push((head_commit.commit_date().timestamp().as_second(), head_id));
    visited.insert(head_id);
    frontier.insert(head_id, head_commit.clone());

    let mut commits: Vec<Commit> = Vec::with_capacity(limit);
    while let Some((_, id)) = heap.pop() {
        let commit = frontier.remove(&id).expect("frontier holds every heap id");
        // Taken before the commit is moved into the page below.
        let parents: Vec<ObjectId> = commit.parents().to_vec();
        commits.push(commit);
        on_batch(&commits);
        if commits.len() == limit {
            break;
        }
        // Enqueue not-yet-seen parents, fetching their objects concurrently.
        let parents: Vec<ObjectId> = parents.into_iter().filter(|p| visited.insert(*p)).collect();
        let objects = futures::future::join_all(parents.iter().map(|p| source.object(*p))).await;
        for (pid, object) in parents.iter().zip(objects) {
            if let Some(parent) = object.ok().and_then(|o| o.commit().ok()) {
                heap.push((parent.commit_date().timestamp().as_second(), *pid));
                frontier.insert(*pid, parent);
            }
        }
    }

    commits
}
