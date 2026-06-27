use crate::cache::CachingRepo;
use git_async::object::{Commit, ObjectId, TreeEntryType};
use git_async::reference::{RefEntry, RefName, RefTarget};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::rc::Rc;
use yew::{Html, html};

pub(crate) mod about;
pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod listing;
pub(crate) mod log;
pub(crate) mod refs_all;
pub(crate) mod refs_heads;
pub(crate) mod refs_tags;
pub(crate) mod summary;
pub(crate) mod tag;
pub(crate) mod tree;

#[derive(PartialEq, Clone)]
pub(crate) struct RefRow {
    name: String,
    short_hash: String,
    message: String,
    author: String,
    age: Age,
}

/// The "Branches" section (the old `refs_heads.html`): a heading plus a table
/// of branch rows, with an optional "more" link to the full branch list. Lives
/// here, next to [`RefRow`], so the refs and summary views can share it.
pub(crate) fn branches_section(branches: &[RefRow], more: bool) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Branches" }</h3>
            { refs_table("Branch", html! {
                <>
                    { for branches.iter().map(|b| refs_table_row(format!("#!/tree?h={}", b.name), b)) }
                    if more {
                        <tr><td>{ "[" }<a href="#!/refs/heads">{ "..." }</a>{ "]" }</td></tr>
                    }
                </>
            }) }
        </>
    }
}

/// The "Tags" section (the old `refs_tags.html`): a heading plus either a table
/// of tag rows (with an optional "more" link) or a "No tags." note.
pub(crate) fn tags_section(tags: &[RefRow], more: bool) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Tags" }</h3>
            if tags.is_empty() {
                <p class="msg">{ "No tags." }</p>
            } else {
                { refs_table("Tag", html! {
                    <>
                        { for tags.iter().map(|t| refs_table_row(format!("#!/refs/tags/{}", t.name), t)) }
                        if more {
                            <tr><td>{ "[" }<a href="#!/refs/tags">{ "..." }</a>{ "]" }</td></tr>
                        }
                    </>
                }) }
            }
        </>
    }
}

/// The shared ref-table shell; `first_col` is the leading column header
/// ("Branch" or "Tag") and `rows` is the already-rendered `<tbody>` contents.
fn refs_table(first_col: &'static str, rows: Html) -> Html {
    html! {
        <table class="summary-table">
            <thead>
                <tr>
                    <th>{ first_col }</th>
                    <th>{ "Commit message" }</th>
                    <th>{ "Author" }</th>
                    <th>{ "Age" }</th>
                </tr>
            </thead>
            <tbody>{ rows }</tbody>
        </table>
    }
}

fn refs_table_row(href: String, r: &RefRow) -> Html {
    html! {
        <tr>
            <td class="name"><a href={href}>{ r.name.clone() }</a></td>
            <td class="msg">{ r.message.clone() }</td>
            <td class="author">{ r.author.clone() }</td>
            <td class="age">{ r.age.display() }</td>
        </tr>
    }
}

#[derive(PartialEq, Clone)]
pub(crate) struct CommitRow {
    hash: String,
    short_hash: String,
    message: String,
    author: String,
    age: Age,
    refs: Vec<RefLabel>,
}

/// A branch or tag decoration shown next to a commit, cgit-style.
#[derive(PartialEq, Clone)]
pub(crate) struct RefLabel {
    name: String,
    kind: RefLabelKind,
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum RefLabelKind {
    Branch,
    Tag,
}

/// The commit list shared by the log and summary views (the old
/// `commits.html`). Lives here, next to [`CommitRow`], so both callers can
/// reuse it and reach the row's private fields.
pub(crate) fn commits_table(commits: &[CommitRow]) -> Html {
    html! {
        <table class="summary-table">
            <thead>
                <tr>
                    <th>{ "Age" }</th>
                    <th>{ "Commit" }</th>
                    <th>{ "Message" }</th>
                    <th>{ "Author" }</th>
                </tr>
            </thead>
            <tbody>
                { for commits.iter().map(commit_table_row) }
            </tbody>
        </table>
    }
}

fn commit_table_row(c: &CommitRow) -> Html {
    let href = format!("#!/commit/{}", c.hash);
    html! {
        <tr>
            <td class="age">{ c.age.display() }</td>
            <td class="name"><a href={href}>{ c.short_hash.clone() }</a></td>
            <td class="msg">{ c.message.clone() }{ for c.refs.iter().map(ref_label) }</td>
            <td class="author">{ c.author.clone() }</td>
        </tr>
    }
}

/// A single decoration after the commit message. Each is preceded by a literal
/// space so consecutive labels (and the message) stay separated.
fn ref_label(r: &RefLabel) -> Html {
    match r.kind {
        RefLabelKind::Tag => {
            let href = format!("#!/refs/tags/{}", r.name);
            html! { <>{ " " }<a class="ref-label tag" href={href}>{ r.name.clone() }</a></> }
        }
        RefLabelKind::Branch => {
            let href = format!("#!/log?h={}", r.name);
            html! { <>{ " " }<a class="ref-label branch" href={href}>{ r.name.clone() }</a></> }
        }
    }
}

fn age(dt: &chrono::DateTime<chrono::FixedOffset>) -> u64 {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp_millis() as f64;
    ((now_ms - then_ms) / 1000.0).max(0.0) as u64
}

/// A commit/ref timestamp that keeps both representations: the elapsed seconds
/// (for sorting by recency and choosing a format) and the absolute timestamp.
/// It sorts by recency and serializes — at render time — to a coarse relative
/// age within the last two weeks, or an absolute `YYYY-MM-DD` date (in the
/// commit's own timezone) beyond that.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Age {
    secs: u64,
    when: chrono::DateTime<chrono::FixedOffset>,
}

impl Age {
    fn new(when: &chrono::DateTime<chrono::FixedOffset>) -> Self {
        Self {
            secs: age(when),
            when: *when,
        }
    }

    /// Elapsed seconds, the sort key (smaller is more recent).
    pub(crate) fn secs(&self) -> u64 {
        self.secs
    }

    /// The rendered age: a coarse relative bucket within two weeks, else an
    /// absolute date. Used by every view that renders a row's age.
    pub(crate) fn display(&self) -> String {
        format_age(self.secs, &self.when)
    }
}

/// The display rule, split out as a pure function so the bucket boundaries can
/// be tested without depending on the wall clock.
fn format_age(secs: u64, dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    match secs {
        s if s < 90 => plural(s, "second"),
        s if s < 90 * 60 => plural(s / 60, "minute"),
        s if s < 36 * 3600 => plural(s / 3600, "hour"),
        s if s < 14 * 86400 => plural(s / 86400, "day"),
        _ => dt.format("%Y-%m-%d").to_string(),
    }
}

/// `<n> <unit>`, with the unit pluralised unless `n` is exactly 1.
fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

fn commit_first_line(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn head_branch_name(repo: &CachingRepo) -> Option<String> {
    let head = repo.head().await.ok()?;
    if let RefTarget::Symbolic(RefName::Ref(name)) = head.target() {
        let branch = name.strip_prefix(b"heads/")?;
        Some(String::from_utf8_lossy(branch).into_owned())
    } else {
        None
    }
}

/// Split the session's ref snapshot into short-named branches and tags.
pub(crate) async fn collect_refs(
    repo: &CachingRepo,
) -> (Vec<(String, RefEntry)>, Vec<(String, RefEntry)>) {
    let Ok(all_refs) = repo.all_refs().await else {
        return (Vec::new(), Vec::new());
    };
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for (ref_name, entry) in all_refs.iter() {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            branches.push((short.to_string(), *entry));
        } else if let Some(short) = label.strip_prefix("tags/") {
            tags.push((short.to_string(), *entry));
        }
    }
    (branches, tags)
}

/// Resolve a ref entry to the commit it points at, fetching the tag object
/// only when no peeled OID was recorded.
pub(crate) async fn commit_for_entry(entry: &RefEntry, repo: &CachingRepo) -> Option<Commit> {
    let obj = repo.lookup_object(entry.commit_target()).await.ok()?;
    repo.peel_to_commit(&obj).await.ok().flatten()
}

pub(crate) async fn fetch_ref_rows(refs: &[(String, RefEntry)], repo: &CachingRepo) -> Vec<RefRow> {
    futures::future::join_all(refs.iter().map(|(short, entry)| {
        let short = short.clone();
        async move {
            let commit = commit_for_entry(entry, repo).await?;
            Some(ref_row(short, &commit))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Map each decorated commit to its branch/tag labels, for cgit-style
/// decorations in commit lists.
pub(crate) async fn decoration_map(repo: &CachingRepo) -> BTreeMap<ObjectId, Vec<RefLabel>> {
    let (branches, tags) = collect_refs(repo).await;
    let mut map: BTreeMap<ObjectId, Vec<RefLabel>> = BTreeMap::new();
    for (name, entry) in branches {
        map.entry(entry.commit_target())
            .or_default()
            .push(RefLabel {
                name,
                kind: RefLabelKind::Branch,
            });
    }
    let tag_oids = futures::future::join_all(tags.into_iter().map(|(name, entry)| async move {
        // Without a recorded peeled OID this costs one (cached) object
        // lookup per tag.
        let oid = match entry.peeled() {
            Some(oid) => oid,
            None => commit_for_entry(&entry, repo).await?.id(),
        };
        Some((name, oid))
    }))
    .await;
    for (name, oid) in tag_oids.into_iter().flatten() {
        map.entry(oid).or_default().push(RefLabel {
            name,
            kind: RefLabelKind::Tag,
        });
    }
    map
}

/// The id of the entry named `name` in `tree`, or `None` if absent.
fn entry_id(tree: &git_async::object::Tree, name: &str) -> Option<ObjectId> {
    tree.entries()
        .find(|e| e.name() == name.as_bytes())
        .map(|e| e.id())
}

/// Resolve a slash-separated path to the [`ObjectId`] it points at within the
/// tree `tree_id` — a blob id for a file, a tree id for a directory. Returns
/// `None` if the path does not exist in that tree. Only used for root commits,
/// which have no parent to diff against.
async fn path_object_id(
    tree_id: ObjectId,
    components: &[&str],
    repo: &CachingRepo,
) -> Option<ObjectId> {
    let (last, dirs) = components.split_last()?;
    let mut current = repo.lookup_object(tree_id).await.ok()?.tree().ok()?;
    for component in dirs {
        let entry = current
            .entries()
            .find(|e| e.name() == component.as_bytes())?;
        if entry.entry_type() != TreeEntryType::Tree {
            return None;
        }
        current = repo.lookup_object(entry.id()).await.ok()?.tree().ok()?;
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
async fn path_differs(a: ObjectId, b: ObjectId, components: &[&str], repo: &CachingRepo) -> bool {
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
        let (ta, tb) = match futures::join!(repo.lookup_object(a), repo.lookup_object(b)) {
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
    let (ta, tb) = match futures::join!(repo.lookup_object(a), repo.lookup_object(b)) {
        (Ok(oa), Ok(ob)) => match (oa.tree(), ob.tree()) {
            (Ok(ta), Ok(tb)) => (ta, tb),
            _ => return true,
        },
        _ => return true,
    };
    entry_id(&ta, last) != entry_id(&tb, last)
}

/// Counters for one [`walk_commits`] call, logged to the console so it's
/// visible whether the commit-graph (and its Bloom filters) is actually doing
/// the work: graph hits should dominate fallbacks, and on a filtered log most
/// commits should be Bloom-skipped rather than tree-diffed.
#[derive(Default)]
struct WalkStats {
    /// Commits whose metadata came from the commit-graph (no object fetch).
    graph_meta_hits: usize,
    /// Commits whose metadata required fetching the commit object instead.
    object_meta_fallbacks: usize,
    /// Filtered commits skipped via the Bloom filter with no tree fetch.
    bloom_skips: usize,
    /// Filtered commits that needed a real tree diff.
    tree_diffs: usize,
}

/// The traversal data a single commit contributes to a log walk: enough to
/// order the frontier (`time`), continue it (`parents`), filter by path
/// (`tree`, plus `pos` for the Bloom filter), all without re-parsing objects.
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
/// The graph is bulk-loaded once (and persisted) via [`CachingRepo::graph_record`],
/// so traversal is in-memory and survives reloads; a commit missing from the
/// bulk set (e.g. pushed since the seed) is resolved with a single targeted read.
async fn ensure_node(
    repo: &CachingRepo,
    cache: &mut BTreeMap<ObjectId, Rc<WalkNode>>,
    id: ObjectId,
    known: Option<&Commit>,
    stats: &mut WalkStats,
) -> Option<Rc<WalkNode>> {
    if let Some(node) = cache.get(&id) {
        return Some(Rc::clone(node));
    }
    let node = if let Some(rec) = repo.graph_record(id).await {
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
            None => repo.lookup_object(id).await.ok()?.commit().ok()?,
        };
        WalkNode {
            tree: commit.tree(),
            parents: commit.parents().to_vec(),
            time: commit.commit_date().timestamp(),
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
async fn confirm_task(repo: &CachingRepo, task: &ConfirmTask, components: &[&str]) -> bool {
    if task.root {
        return path_object_id(task.tree, components, repo).await.is_some();
    }
    for parent_tree in &task.parent_trees {
        if !path_differs(task.tree, *parent_tree, components, repo).await {
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

/// Non-streaming convenience wrapper: walk and return the whole page at once.
/// Used by the summary's bounded recent-commits list, which has nothing to
/// stream.
pub(crate) async fn walk_commits(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: Option<&str>,
    skip: usize,
    limit: usize,
    decorations: &BTreeMap<ObjectId, Vec<RefLabel>>,
) -> (Vec<CommitRow>, bool) {
    walk_commits_streamed(head_commit, repo, path, skip, limit, decorations, |_| {}).await
}

/// Walk history like [`walk_commits`], but call `on_batch` with the rows
/// gathered so far after each chunk of commit objects is fetched, so the log
/// can render progressively instead of waiting for the whole page. The return
/// value is still the complete page plus whether a further page exists.
pub(crate) async fn walk_commits_streamed(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: Option<&str>,
    skip: usize,
    limit: usize,
    decorations: &BTreeMap<ObjectId, Vec<RefLabel>>,
    on_batch: impl Fn(&[CommitRow]),
) -> (Vec<CommitRow>, bool) {
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
        repo,
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
    // exists.
    let want = skip + limit;
    let mut matched: Vec<ObjectId> = Vec::new();
    let mut has_more = false;
    let mut traversed = 0usize;

    match &path_components {
        // Unfiltered: every traversed commit matches; pure in-memory traversal.
        None => {
            while let Some((_, id)) = heap.pop() {
                traversed += 1;
                let node = meta.get(&id).map(Rc::clone).unwrap();
                if matched.len() == want {
                    has_more = true;
                    break;
                }
                matched.push(id);
                for parent in node.parents.iter().copied() {
                    if visited.insert(parent)
                        && let Some(parent_node) =
                            ensure_node(repo, &mut meta, parent, None, &mut stats).await
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
            let mut pending: Vec<ConfirmTask> = Vec::new();
            'outer: loop {
                // Refill the candidate buffer by traversing (in-memory) commits,
                // enqueueing parents and Bloom-skipping non-matches as we go.
                while pending.len() < CONFIRM_BATCH {
                    let Some((_, id)) = heap.pop() else { break };
                    traversed += 1;
                    let node = meta.get(&id).map(Rc::clone).unwrap();
                    let mut parent_trees = Vec::with_capacity(node.parents.len());
                    for parent in node.parents.iter().copied() {
                        if let Some(parent_node) =
                            ensure_node(repo, &mut meta, parent, None, &mut stats).await
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
                    } else if repo.graph_path_unchanged(node.bloom.as_deref(), path_str) {
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
                        .map(|task| confirm_task(repo, task, components)),
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

    crate::console_log(&format!(
        "webgit: log walk{}: traversed {traversed} commits \
         (graph-meta {}, object-meta {}), filter: {} Bloom-skips / {} tree-diffs, \
         showing {} rows{}",
        path.map(|p| format!(" [{p}]")).unwrap_or_default(),
        stats.graph_meta_hits,
        stats.object_meta_fallbacks,
        stats.bloom_skips,
        stats.tree_diffs,
        matched.len().saturating_sub(skip).min(limit),
        if has_more { " (more pages)" } else { "" },
    ));

    // Fetch objects only for the commits actually shown — at most `limit`. Work
    // in chunks (each chunk fetched concurrently) so the first rows can render
    // while the rest are still in flight, emitting the rows so far after each.
    let window: Vec<ObjectId> = matched.into_iter().skip(skip).take(limit).collect();
    let mut commits: Vec<CommitRow> = Vec::with_capacity(window.len());
    for chunk in window.chunks(STREAM_BATCH) {
        let objects =
            futures::future::join_all(chunk.iter().map(|id| repo.lookup_object(*id))).await;
        for (id, object) in chunk.iter().zip(objects) {
            let Some(commit) = object.ok().and_then(|o| o.commit().ok()) else {
                continue;
            };
            let hash = format!("{id}");
            commits.push(CommitRow {
                short_hash: hash[..8].to_string(),
                hash,
                message: commit_first_line(commit.message()),
                author: String::from_utf8_lossy(commit.author_name()).into_owned(),
                age: Age::new(&commit.author_date()),
                refs: decorations.get(id).cloned().unwrap_or_default(),
            });
        }
        on_batch(&commits);
    }

    (commits, has_more)
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    let hash = format!("{}", c.id());
    RefRow {
        name,
        short_hash: hash[..8].to_string(),
        message: commit_first_line(c.message()),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: Age::new(&c.author_date()),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{Age, CommitRow, RefLabel, RefLabelKind, RefRow};

    /// An [`Age`] that renders as a relative bucket; `secs` must be under the
    /// two-week cutoff for the (placeholder) date to stay hidden.
    pub(crate) fn relative_age(secs: u64) -> Age {
        Age {
            secs,
            when: ymd("2000-01-01"),
        }
    }

    /// An [`Age`] old enough to render as the given absolute `YYYY-MM-DD` date.
    pub(crate) fn date_age(date: &str) -> Age {
        Age {
            secs: 365 * 86400,
            when: ymd(date),
        }
    }

    fn ymd(date: &str) -> chrono::DateTime<chrono::FixedOffset> {
        use chrono::TimeZone;
        let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        chrono::FixedOffset::east_opt(0)
            .unwrap()
            .from_local_datetime(&naive)
            .unwrap()
    }

    pub(crate) fn ref_row(name: &str, message: &str, author: &str, age: Age) -> RefRow {
        RefRow {
            name: name.to_string(),
            short_hash: "0123abcd".to_string(),
            message: message.to_string(),
            author: author.to_string(),
            age,
        }
    }

    pub(crate) fn commit_row(short_hash: &str, message: &str, author: &str, age: Age) -> CommitRow {
        CommitRow {
            hash: format!("{short_hash}{}", "0".repeat(40 - short_hash.len())),
            short_hash: short_hash.to_string(),
            message: message.to_string(),
            author: author.to_string(),
            age,
            refs: Vec::new(),
        }
    }

    pub(crate) fn decorated_commit_row(
        short_hash: &str,
        message: &str,
        author: &str,
        age: Age,
        branches: &[&str],
        tags: &[&str],
    ) -> CommitRow {
        let mut row = commit_row(short_hash, message, author, age);
        for name in branches {
            row.refs.push(RefLabel {
                name: name.to_string(),
                kind: RefLabelKind::Branch,
            });
        }
        for name in tags {
            row.refs.push(RefLabel {
                name: name.to_string(),
                kind: RefLabelKind::Tag,
            });
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_dt() -> chrono::DateTime<chrono::FixedOffset> {
        use chrono::TimeZone;
        chrono::FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2001, 2, 3, 4, 5, 6)
            .unwrap()
    }

    #[test]
    fn test_format_age_relative_buckets() {
        let dt = fixed_dt();
        assert_eq!(format_age(0, &dt), "0 seconds");
        assert_eq!(format_age(1, &dt), "1 second");
        assert_eq!(format_age(89, &dt), "89 seconds");
        assert_eq!(format_age(90, &dt), "1 minute");
        assert_eq!(format_age(89 * 60, &dt), "89 minutes");
        assert_eq!(format_age(90 * 60, &dt), "1 hour");
        assert_eq!(format_age(35 * 3600, &dt), "35 hours");
        assert_eq!(format_age(36 * 3600, &dt), "1 day");
        assert_eq!(format_age(13 * 86400, &dt), "13 days");
    }

    #[test]
    fn test_format_age_two_weeks_and_older_is_date() {
        let dt = fixed_dt();
        // From exactly two weeks on, show the commit's own date instead.
        assert_eq!(format_age(14 * 86400, &dt), "2001-02-03");
        assert_eq!(format_age(86400 * 400, &dt), "2001-02-03");
    }

    #[test]
    fn age_sorts_by_recency_regardless_of_display() {
        let dt = fixed_dt();
        // A mix of relative-rendered and date-rendered ages; sorting must order
        // them by elapsed seconds (most recent first), not by the display text.
        let mut ages = [
            Age {
                secs: 86400 * 400,
                when: dt,
            },
            Age { secs: 60, when: dt },
            Age {
                secs: 3600,
                when: dt,
            },
        ];
        ages.sort_by_key(Age::secs);
        assert_eq!(
            ages.map(|a| a.secs()),
            [60, 3600, 86400 * 400],
            "expected ascending recency order"
        );
    }

    #[test]
    fn test_commit_first_line() {
        assert_eq!(
            commit_first_line(b"Fix bug\n\nLonger description\n"),
            "Fix bug"
        );
        assert_eq!(commit_first_line(b"Single line"), "Single line");
        assert_eq!(commit_first_line(b"trailing newline\n"), "trailing newline");
        assert_eq!(commit_first_line(b""), "");
        // Invalid UTF-8 is replaced, not dropped.
        assert_eq!(
            commit_first_line(b"caf\xc3\xa9 \xff fix"),
            "caf\u{e9} \u{fffd} fix"
        );
    }
}
