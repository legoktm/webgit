use crate::cache::CachingRepo;
use crate::route::encode_component;
use gib::object::{Commit, ObjectId, TreeEntryType};
use gib::reference::{RefEntry, RefName, RefTarget};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::rc::Rc;
use yew::{Html, html};

pub(crate) mod about;
pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod listing;
pub(crate) mod log;
pub(crate) mod readme;
pub(crate) mod refs_all;
pub(crate) mod refs_heads;
pub(crate) mod refs_tags;
pub(crate) mod snapshot;
pub(crate) mod summary;
pub(crate) mod tag;
pub(crate) mod tree;

#[derive(PartialEq, Clone)]
pub(crate) struct RefRow {
    name: String,
    /// The ref's commit metadata; `None` while the commit is still being
    /// fetched. The summary lists the (name-sorted) ref names immediately and
    /// backfills these columns as each section's commits resolve.
    meta: Option<RefMeta>,
}

#[derive(PartialEq, Clone)]
struct RefMeta {
    message: String,
    author: String,
    age: Age,
}

impl RefRow {
    /// A name-only row whose commit metadata hasn't loaded yet.
    pub(crate) fn pending(name: String) -> Self {
        RefRow { name, meta: None }
    }

    /// Recency sort key (used by the age-sorted refs pages, which never hold
    /// pending rows); a pending row sorts as most-recent.
    pub(crate) fn age_secs(&self) -> u64 {
        self.meta.as_ref().map_or(0, |m| m.age.secs())
    }
}

/// The "Branches" section (the old `refs_heads.html`): a heading plus a table
/// of branch rows, with an optional "more" link to the full branch list. Lives
/// here, next to [`RefRow`], so the refs and summary views can share it.
pub(crate) fn branches_section(branches: &[RefRow], more: bool) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Branches" }</h3>
            { refs_table("Branch", None, html! {
                <>
                    { for branches.iter().map(|b| refs_table_row(format!("#!/tree?h={}", encode_component(&b.name)), b, None)) }
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
pub(crate) fn tags_section(tags: &[RefRow], more: bool, repo_name: &str) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Tags" }</h3>
            if tags.is_empty() {
                <p class="msg">{ "No tags." }</p>
            } else {
                { refs_table("Tag", Some("Download"), html! {
                    <>
                        { for tags.iter().map(|t| refs_table_row(
                            format!("#!/refs/tags/{}", encode_component(&t.name)),
                            t,
                            Some(snapshot_cell(repo_name, &t.name)),
                        )) }
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
/// ("Branch" or "Tag"), `extra_col` an optional header sitting just right of the
/// commit message (the tag tables' snapshot links), and `rows` the
/// already-rendered `<tbody>`.
fn refs_table(first_col: &'static str, extra_col: Option<&'static str>, rows: Html) -> Html {
    html! {
        <table class="summary-table">
            <thead>
                <tr>
                    <th>{ first_col }</th>
                    <th>{ "Commit message" }</th>
                    if let Some(extra) = extra_col {
                        <th>{ extra }</th>
                    }
                    <th>{ "Author" }</th>
                    <th>{ "Age" }</th>
                </tr>
            </thead>
            <tbody>{ rows }</tbody>
        </table>
    }
}

/// A minimalist, CSS-animated loading ellipsis shown in place of a value that
/// is still being fetched. The dots cycle via the `.loading-dots` stylesheet
/// rule, so no inline style or script is needed (CSP-safe).
pub(crate) fn loading_dots() -> Html {
    html! { <span class="loading-dots" aria-label="Loading"></span> }
}

/// One ref row. The snapshot cell sits between the message and the author, and
/// is rendered whether or not the commit metadata has arrived — the archive
/// link depends only on the ref name, so there is nothing to wait for.
fn refs_table_row(href: String, r: &RefRow, extra: Option<Html>) -> Html {
    html! {
        <tr key={r.name.clone()}>
            <td class="name"><a href={href}>{ r.name.clone() }</a></td>
            <td class="msg">
                { match &r.meta {
                    Some(m) => html! { m.message.clone() },
                    None => loading_dots(),
                } }
            </td>
            if let Some(extra) = extra {
                <td class="snapshot">{ extra }</td>
            }
            <td class="author">
                { r.meta.as_ref().map(|m| m.author.clone()).unwrap_or_default() }
            </td>
            <td class="age">
                { r.meta.as_ref().map(|m| m.age.display()).unwrap_or_default() }
            </td>
        </tr>
    }
}

/// The archive link for a tag, labelled with the file it downloads.
fn snapshot_cell(repo_name: &str, tag: &str) -> Html {
    html! {
        <a class="snapshot-link" href={crate::route::snapshot_url(tag)}>
            { crate::render::snapshot::snapshot_file_name(repo_name, tag) }
        </a>
    }
}

#[derive(PartialEq, Clone)]
pub(crate) struct CommitRow {
    /// The full commit id, kept in its 20-byte form rather than as hex.
    id: ObjectId,
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
    let href = format!("#!/commit/{}", c.id);
    html! {
        <tr key={c.id.to_string()}>
            <td class="age">{ c.age.display() }</td>
            <td class="name"><a href={href}>{ c.short_hash.clone() }</a></td>
            <td class="msg">{ c.message.clone() }{ for c.refs.iter().map(ref_label) }</td>
            <td class="author">{ c.author.clone() }</td>
        </tr>
    }
}

/// The 8-character abbreviation of `id` displayed in commit tables. Rendered
/// once when the row is built, so the full hex form is never retained.
fn short_hash(id: ObjectId) -> String {
    format!("{id}")[..8].to_string()
}

/// A single decoration after the commit message. Each is preceded by a literal
/// space so consecutive labels (and the message) stay separated.
fn ref_label(r: &RefLabel) -> Html {
    match r.kind {
        RefLabelKind::Tag => {
            let href = format!("#!/refs/tags/{}", encode_component(&r.name));
            html! { <>{ " " }<a class="ref-label tag" href={href}>{ r.name.clone() }</a></> }
        }
        RefLabelKind::Branch => {
            let href = format!("#!/log?h={}", encode_component(&r.name));
            html! { <>{ " " }<a class="ref-label branch" href={href}>{ r.name.clone() }</a></> }
        }
    }
}

/// Hand control back to the browser event loop so it can paint pending DOM
/// updates before the next chunk of work. A resolved `Promise` (microtask)
/// would not give the renderer a turn — a 0 ms `setTimeout` is a real macrotask
/// boundary, which is where the browser gets to repaint. Used between streamed
/// render batches whose data resolves too fast (cached) to yield on its own.
pub(crate) async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

fn age(dt: &jiff::Zoned) -> u64 {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp().as_millisecond() as f64;
    ((now_ms - then_ms) / 1000.0).max(0.0) as u64
}

/// A commit/ref timestamp that keeps both representations: the elapsed seconds
/// (for sorting by recency and choosing a format) and the calendar date in the
/// commit's own timezone. It sorts by recency and serializes — at render time —
/// to a coarse relative age within the last two weeks, or that absolute
/// `YYYY-MM-DD` date beyond that.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Age {
    secs: u64,
    when: jiff::civil::Date,
}

impl Age {
    fn new(when: &jiff::Zoned) -> Self {
        Self {
            secs: age(when),
            when: when.date(),
        }
    }

    /// Elapsed seconds, the sort key (smaller is more recent).
    pub(crate) fn secs(&self) -> u64 {
        self.secs
    }

    /// The rendered age: a coarse relative bucket within two weeks, else an
    /// absolute date. Used by every view that renders a row's age.
    pub(crate) fn display(&self) -> String {
        format_age(self.secs, self.when)
    }
}

/// The display rule, split out as a pure function so the bucket boundaries can
/// be tested without depending on the wall clock. A [`jiff::civil::Date`]
/// displays as ISO 8601 `YYYY-MM-DD`, which is the format we want verbatim.
fn format_age(secs: u64, date: jiff::civil::Date) -> String {
    match secs {
        s if s < 90 => plural(s, "second"),
        s if s < 90 * 60 => plural(s / 60, "minute"),
        s if s < 36 * 3600 => plural(s / 3600, "hour"),
        s if s < 14 * 86400 => plural(s / 86400, "day"),
        _ => date.to_string(),
    }
}

/// A timestamp rendered as `YYYY-MM-DD HH:MM:SS ±HH:MM` in its own timezone,
/// for the commit and tag metadata tables. Assembled by hand rather than via
/// `strftime` — the pieces each `Display` in exactly the shape we need, except
/// the offset, which jiff prints as `+01` where we want `+01:00`.
pub(crate) fn format_datetime(dt: &jiff::Zoned) -> String {
    let total = dt.offset().seconds();
    let sign = if total < 0 { '-' } else { '+' };
    let (hours, minutes) = (total.abs() / 3600, (total.abs() % 3600) / 60);
    format!("{} {} {sign}{hours:02}:{minutes:02}", dt.date(), dt.time())
}

/// `<n> <unit>`, with the unit pluralised unless `n` is exactly 1.
fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Heuristic matching git's: a blob is treated as binary if a NUL byte appears
/// in its leading bytes. git scans the first 8000 bytes, so do the same.
///
/// Shared by the two views that must decide whether bytes can be shown as text:
/// the diff (which prints "Binary files differ" instead of a hunk) and the blob
/// view (which refuses to render the file at all).
pub(crate) fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
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

/// Releases indexed values in index order no matter what order they resolve in.
///
/// Concurrent fetches complete scrambled relative to their list — an
/// already-cached object resolves in a microtask while its neighbour waits on
/// the network, an annotated tag costs an extra round trip to peel, and over
/// HTTP/1.1 the browser's per-origin connection cap completes requests in waves
/// — and a table filling in scattered order reads as glitchy. Parking each
/// value as it lands and emitting only the contiguous resolved prefix makes the
/// table fill top-down while leaving the fetches fully concurrent: nothing waits
/// on anything, the reveal is merely sequenced.
struct InOrder<T> {
    /// `None` = not resolved yet; `Some(None)` = resolved with nothing to emit.
    slots: RefCell<Vec<Option<Option<T>>>>,
    /// Index of the next value to release; everything below it has been emitted.
    next: Cell<usize>,
}

impl<T> InOrder<T> {
    fn new(len: usize) -> Self {
        InOrder {
            slots: RefCell::new((0..len).map(|_| None).collect()),
            next: Cell::new(0),
        }
    }

    /// Record slot `i` as resolved, then emit however much of the prefix that
    /// unblocked — nothing at all unless `i` was itself the next one due. A
    /// `None` value consumes its slot without being emitted, so a value that
    /// fails to resolve can't stall the ones after it.
    fn resolve(&self, i: usize, value: Option<T>, emit: impl Fn(usize, T)) {
        self.slots.borrow_mut()[i] = Some(value);
        loop {
            let idx = self.next.get();
            // Scoped so the borrow is released before `emit` runs.
            let value = {
                let mut slots = self.slots.borrow_mut();
                match slots.get_mut(idx) {
                    Some(slot) if slot.is_some() => slot.take().flatten(),
                    _ => break,
                }
            };
            self.next.set(idx + 1);
            if let Some(value) = value {
                emit(idx, value);
            }
        }
    }
}

/// Resolve `refs` concurrently, calling `on_row(index, RefRow)` — `index` being
/// the ref's position in `refs` — to backfill a name-only skeleton row by row
/// instead of waiting for the whole list. Fetching is fully concurrent, but the
/// callbacks are delivered strictly in index order, so the table fills top-down
/// (see [`InOrder`]).
pub(crate) async fn fetch_ref_rows_each(
    refs: &[(String, RefEntry)],
    repo: &CachingRepo,
    on_row: impl Fn(usize, RefRow),
) {
    let reveal = InOrder::new(refs.len());
    let (reveal, on_row) = (&reveal, &on_row);

    futures::future::join_all(refs.iter().enumerate().map(|(i, (short, entry))| {
        let short = short.clone();
        async move {
            let row = commit_for_entry(entry, repo)
                .await
                .map(|commit| ref_row(short, &commit));
            reveal.resolve(i, row, on_row);
        }
    }))
    .await;
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
fn entry_id(tree: &gib::object::Tree, name: &str) -> Option<ObjectId> {
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

/// Counters for one [`walk_commits_streamed`] call, logged to the console so it's
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

/// Walk history a page at a time, calling `on_batch` with the rows
/// gathered so far after each chunk of commit objects is fetched, so the log
/// can render progressively instead of waiting for the whole page. The return
/// value is still the complete page plus whether a further page exists.
///
/// Rows are emitted with no ref decorations (`refs` empty), as
/// [`recent_commits`] does: the caller computes the decoration map concurrently
/// and folds it in with [`apply_decorations`], so peeling every tag never holds
/// up the walk.
pub(crate) async fn walk_commits_streamed(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: Option<&str>,
    skip: usize,
    limit: usize,
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
            commits.push(CommitRow {
                id: *id,
                short_hash: short_hash(*id),
                message: commit_first_line(commit.message()),
                author: String::from_utf8_lossy(commit.author_name()).into_owned(),
                age: Age::new(commit.author_date()),
                refs: Vec::new(),
            });
        }
        on_batch(&commits);
    }

    (commits, has_more)
}

/// Walk the most recent `limit` commits reachable from `head_commit` by fetching
/// commit objects directly, deliberately bypassing the commit-graph. For a small
/// bounded preview (the summary) this avoids the whole-file bulk load that
/// [`walk_commits_streamed`] triggers on its first `graph_record` call — a handful of
/// cheap object reads (the same path the ref rows use) instead of downloading
/// and persisting every commit's metadata just to show a teaser. History is
/// unfiltered, so there is no path/Bloom work; `on_batch` is called with the
/// rows so far as each commit object resolves, so they stream in newest-first.
///
/// Rows are emitted with no ref decorations (`refs` empty): the caller computes
/// the decoration map concurrently and folds it in with [`apply_decorations`],
/// so the (sometimes fetch-bound) ref scan never holds up the commit rows.
pub(crate) async fn recent_commits(
    head_commit: &Commit,
    repo: &CachingRepo,
    limit: usize,
    on_batch: impl Fn(&[CommitRow]),
) -> Vec<CommitRow> {
    // Same frontier discipline as `walk_commits_streamed`'s unfiltered arm — a heap
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

    let mut commits: Vec<CommitRow> = Vec::with_capacity(limit);
    while let Some((_, id)) = heap.pop() {
        let commit = frontier.remove(&id).expect("frontier holds every heap id");
        commits.push(CommitRow {
            id,
            short_hash: short_hash(id),
            message: commit_first_line(commit.message()),
            author: String::from_utf8_lossy(commit.author_name()).into_owned(),
            age: Age::new(commit.author_date()),
            refs: Vec::new(),
        });
        on_batch(&commits);
        if commits.len() == limit {
            break;
        }
        // Enqueue not-yet-seen parents, fetching their objects concurrently.
        let parents: Vec<ObjectId> = commit
            .parents()
            .iter()
            .copied()
            .filter(|p| visited.insert(*p))
            .collect();
        let objects =
            futures::future::join_all(parents.iter().map(|p| repo.lookup_object(*p))).await;
        for (pid, object) in parents.iter().zip(objects) {
            if let Some(parent) = object.ok().and_then(|o| o.commit().ok()) {
                heap.push((parent.commit_date().timestamp().as_second(), *pid));
                frontier.insert(*pid, parent);
            }
        }
    }

    commits
}

/// Fold a decoration map into already-built commit rows, matching on commit id,
/// so the summary can stream label-less rows from [`recent_commits`] and add the
/// branch/tag chips once its (separately, concurrently fetched) decoration map
/// resolves. A no-op when there is nothing to decorate.
///
/// The log calls this once per streamed partial as well as once for the finished
/// page, so it stays allocation-free: rows carry their [`ObjectId`], which is
/// looked up in `decorations` directly rather than via a hex-keyed side map.
pub(crate) fn apply_decorations(
    rows: &mut [CommitRow],
    decorations: &BTreeMap<ObjectId, Vec<RefLabel>>,
) {
    if decorations.is_empty() {
        return;
    }
    for row in rows.iter_mut() {
        if let Some(labels) = decorations.get(&row.id) {
            row.refs = labels.clone();
        }
    }
}

/// An object URL over `data`, for whatever needs to hand bytes to the browser:
/// a blob's `<img>` and download link, a snapshot's download link.
///
/// An object URL rather than a `data:` one because the bytes are already in
/// memory: base64 would add a third again in size and park the whole encoded
/// file in a DOM attribute, where a `blob:` URL is a short string the browser
/// resolves back to a buffer. One URL per set of bytes, since constructing the
/// `Blob` copies them and a second one would hold the file twice.
///
/// The URL is created in an effect, not during render, for two reasons: it is a
/// side effect with a matching teardown (an object URL pins its buffer until
/// revoked, so navigating between blobs would otherwise leak one per visit),
/// and it keeps `web_sys` off the render path, where the SSR-based tests run
/// without a DOM. Under SSR the effect never fires and the empty string is what
/// the caller sees.
#[yew::hook]
pub(crate) fn use_object_url(mime: &'static str, data: &Rc<Vec<u8>>) -> String {
    let url = yew::use_state(String::new);
    {
        let url = url.clone();
        yew::use_effect_with(
            (mime, data.clone()),
            move |(mime, data): &(&'static str, Rc<Vec<u8>>)| {
                let created = object_url(mime, data).unwrap_or_default();
                url.set(created.clone());
                move || {
                    if !created.is_empty() {
                        let _ = web_sys::Url::revoke_object_url(&created);
                    }
                }
            },
        );
    }
    (*url).clone()
}

/// As [`use_object_url`], for a `Blob` the caller already has.
///
/// The snapshot view's archive arrives this way — the browser assembled it from
/// the gzip stream, so its bytes were never in our memory and there is nothing
/// to wrap. Blob equality is JS identity, so the effect re-runs when the archive
/// is genuinely a different one and not merely re-rendered.
#[yew::hook]
pub(crate) fn use_blob_url(blob: &web_sys::Blob) -> String {
    let url = yew::use_state(String::new);
    {
        let url = url.clone();
        yew::use_effect_with(blob.clone(), move |blob: &web_sys::Blob| {
            let created = web_sys::Url::create_object_url_with_blob(blob).unwrap_or_default();
            url.set(created.clone());
            move || {
                if !created.is_empty() {
                    let _ = web_sys::Url::revoke_object_url(&created);
                }
            }
        });
    }
    (*url).clone()
}

/// Wrap `data` in a `Blob` of type `mime` and mint an object URL for it. `None`
/// if the browser refuses either step, which leaves the view showing neither an
/// image nor a download link rather than broken ones.
fn object_url(mime: &str, data: &[u8]) -> Option<String> {
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(data));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    RefRow {
        name,
        meta: Some(RefMeta {
            message: commit_first_line(c.message()),
            author: String::from_utf8_lossy(c.author_name()).into_owned(),
            age: Age::new(c.author_date()),
        }),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{Age, CommitRow, ObjectId, RefLabel, RefLabelKind, RefMeta, RefRow};

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

    fn ymd(date: &str) -> jiff::civil::Date {
        date.parse().unwrap()
    }

    pub(crate) fn ref_row(name: &str, message: &str, author: &str, age: Age) -> RefRow {
        RefRow {
            name: name.to_string(),
            meta: Some(RefMeta {
                message: message.to_string(),
                author: author.to_string(),
                age,
            }),
        }
    }

    pub(crate) fn commit_row(short_hash: &str, message: &str, author: &str, age: Age) -> CommitRow {
        // Zero-pad the abbreviation out to a full id, so the row's link renders
        // the same 40 hex characters a real walk would produce.
        let hex = format!("{short_hash}{}", "0".repeat(40 - short_hash.len()));
        CommitRow {
            id: ObjectId::from_hex(hex.as_bytes()).expect("fixture id must be 40 hex characters"),
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

    fn fixed_date() -> jiff::civil::Date {
        jiff::civil::date(2001, 2, 3)
    }

    /// Resolve `order` (a permutation of slot indices) and collect what was
    /// emitted, as `(index, value)` pairs in emission order.
    fn reveal_sequence(len: usize, order: &[usize]) -> Vec<(usize, usize)> {
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(len);
        for &i in order {
            reveal.resolve(i, Some(i * 10), |idx, v| {
                emitted.borrow_mut().push((idx, v))
            });
        }
        emitted.into_inner()
    }

    #[test]
    fn in_order_emits_in_index_order_however_it_resolves() {
        // Reverse, then a scrambled order: emission is by index either way.
        let expected: Vec<(usize, usize)> = (0..4).map(|i| (i, i * 10)).collect();
        assert_eq!(reveal_sequence(4, &[3, 2, 1, 0]), expected);
        assert_eq!(reveal_sequence(4, &[1, 3, 0, 2]), expected);
        assert_eq!(reveal_sequence(4, &[0, 1, 2, 3]), expected);
    }

    #[test]
    fn in_order_holds_values_behind_an_unresolved_slot() {
        // Slot 0 outstanding: nothing may be emitted, however many land after it.
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(3);
        let push = |idx, v| emitted.borrow_mut().push((idx, v));

        reveal.resolve(2, Some(20), push);
        reveal.resolve(1, Some(10), push);
        assert!(emitted.borrow().is_empty(), "nothing precedes slot 0");

        // Slot 0 landing releases the whole run at once.
        reveal.resolve(0, Some(0), push);
        assert_eq!(emitted.into_inner(), vec![(0, 0), (1, 10), (2, 20)]);
    }

    #[test]
    fn in_order_skips_unresolvable_values_without_stalling() {
        // A ref whose commit doesn't resolve consumes its slot silently; the
        // rows after it must still come through.
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(3);
        let push = |idx, v| emitted.borrow_mut().push((idx, v));

        reveal.resolve(1, Some(10), push);
        reveal.resolve(0, None, push);
        assert_eq!(*emitted.borrow(), vec![(1, 10)]);

        reveal.resolve(2, Some(20), push);
        assert_eq!(emitted.into_inner(), vec![(1, 10), (2, 20)]);
    }

    #[test]
    fn test_format_age_relative_buckets() {
        let date = fixed_date();
        assert_eq!(format_age(0, date), "0 seconds");
        assert_eq!(format_age(1, date), "1 second");
        assert_eq!(format_age(89, date), "89 seconds");
        assert_eq!(format_age(90, date), "1 minute");
        assert_eq!(format_age(89 * 60, date), "89 minutes");
        assert_eq!(format_age(90 * 60, date), "1 hour");
        assert_eq!(format_age(35 * 3600, date), "35 hours");
        assert_eq!(format_age(36 * 3600, date), "1 day");
        assert_eq!(format_age(13 * 86400, date), "13 days");
    }

    #[test]
    fn test_format_age_two_weeks_and_older_is_date() {
        let date = fixed_date();
        // From exactly two weeks on, show the commit's own date instead.
        assert_eq!(format_age(14 * 86400, date), "2001-02-03");
        assert_eq!(format_age(86400 * 400, date), "2001-02-03");
    }

    #[test]
    fn test_format_datetime() {
        fn at(secs: i64, offset_secs: i32) -> jiff::Zoned {
            jiff::Timestamp::from_second(secs)
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::fixed(
                    jiff::tz::Offset::from_seconds(offset_secs).unwrap(),
                ))
        }
        // A whole-hour offset still gets its `:00` minutes, and the wall clock
        // is the one in that offset, not UTC.
        assert_eq!(
            format_datetime(&at(1774735018, 0)),
            "2026-03-28 21:56:58 +00:00"
        );
        assert_eq!(
            format_datetime(&at(1774735018, 3600)),
            "2026-03-28 22:56:58 +01:00"
        );
        assert_eq!(
            format_datetime(&at(1774735018, 19800)),
            "2026-03-29 03:26:58 +05:30"
        );
        assert_eq!(
            format_datetime(&at(1774735018, -28800)),
            "2026-03-28 13:56:58 -08:00"
        );
    }

    #[test]
    fn age_sorts_by_recency_regardless_of_display() {
        let date = fixed_date();
        // A mix of relative-rendered and date-rendered ages; sorting must order
        // them by elapsed seconds (most recent first), not by the display text.
        let mut ages = [
            Age {
                secs: 86400 * 400,
                when: date,
            },
            Age {
                secs: 60,
                when: date,
            },
            Age {
                secs: 3600,
                when: date,
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
    fn test_is_binary() {
        assert!(!is_binary(b""));
        assert!(!is_binary(b"hello\nworld\n"));
        // UTF-8 multibyte content has no NUL bytes and must stay textual.
        assert!(!is_binary("café — résumé".as_bytes()));
        assert!(is_binary(b"PK\x03\x04\x00\x00"));
        assert!(is_binary(b"text then \0 nul"));
        // A NUL past the 8000-byte scan window is not flagged, matching git.
        let mut late_nul = vec![b'a'; 8000];
        late_nul.push(0);
        assert!(!is_binary(&late_nul));
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
