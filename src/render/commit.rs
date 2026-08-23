use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::{download_bytes, format_datetime, mapped_ident, yield_to_browser};
use futures::stream::{FuturesOrdered, StreamExt};
use gib::diff::{DiffEntry, TreeDiff};
use gib::error::Error as GitError;
use gib::object::{Object, ObjectId, ObjectIdPrefix, PrefixResolution, Tree};
use gib_mailmap::Mailmap;
use gib_patch::{FileDiff, LineKind, PatchLine, PatchMeta, Side};
use std::cell::RefCell;
use yew::prelude::*;

#[derive(PartialEq, Clone)]
struct ParentRef {
    hash: String,
    short: String,
}

/// One row of the diffstat, and the file's diff once it has one.
#[derive(PartialEq, Clone)]
struct FileRow {
    path: String,
    /// The file's diff, absent between the tree diff (which gives us the path)
    /// and this file's blobs loading (which give us everything else): the row
    /// shows the name with the stats column blank until they arrive.
    diff: Option<FileDiff>,
    /// The two halves of the diffstat bar, in columns. Recomputed as files
    /// arrive, so they are the view's own and not the patch's.
    bar_add: usize,
    bar_del: usize,
}

impl FileRow {
    fn additions(&self) -> usize {
        self.diff.as_ref().map_or(0, |d| d.additions)
    }

    fn deletions(&self) -> usize {
        self.diff.as_ref().map_or(0, |d| d.deletions)
    }
}

/// One piece of a commit message: either literal text (escaped by Yew when
/// rendered) or a SHA-1 reference that becomes a link to that commit. Splitting
/// the message into segments lets Yew handle escaping natively instead of us
/// hand-building trusted HTML.
#[derive(PartialEq, Clone, Debug)]
enum MessageSegment {
    Text(String),
    Sha(String),
}

/// The view inputs for a commit. These double as the component's props and the
/// unit-test fixture, so the data-building (`build_commit`) and the markup
/// (`CommitView`) can be exercised independently.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct CommitProps {
    meta: PatchMeta,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    parents: Vec<ParentRef>,
    tree_hash: String,
    message: Vec<MessageSegment>,
    notes: Option<Vec<MessageSegment>>,
    total_additions: usize,
    total_deletions: usize,
    files: Vec<FileRow>,
    complete: bool,
}

/// Build a commit view, calling `on_partial` first with the metadata alone
/// (empty diff) and then once per file as the diff streams in, so the header
/// renders immediately and a large diff fills in top-to-bottom. The returned
/// value is the complete view.
pub(crate) async fn build_commit(
    repo: &CachingRepo,
    mailmap: &Mailmap,
    sha: &str,
    on_partial: impl Fn(CommitProps),
) -> anyhow::Result<CommitProps> {
    let oid = resolve_sha(repo, sha).await?;
    let commit = repo
        .lookup_object(oid)
        .await
        .context(format!("lookup {sha}"))?
        .commit()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let (parent_commits, commit_tree_obj) = futures::join!(
        async { repo.lookup_parents(&commit).await.unwrap_or_default() },
        repo.lookup_object(commit.tree()),
    );
    let commit_tree = commit_tree_obj
        .context("lookup commit tree")?
        .tree()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let parents: Vec<ParentRef> = parent_commits
        .iter()
        .map(|p| {
            let h = format!("{}", p.id());
            ParentRef {
                short: h[..8].to_string(),
                hash: h,
            }
        })
        .collect();

    // The metadata is ready well before the diff; emit it now so the header
    // table and message paint while the diff blobs are still loading.
    let (author_name, author_email) =
        mapped_ident(commit.author_name(), commit.author_email(), mailmap);
    let (committer_name, committer_email) =
        mapped_ident(commit.committer_name(), commit.committer_email(), mailmap);

    let base = CommitProps {
        meta: PatchMeta::from_commit(&commit),
        author_name,
        author_email,
        author_date: format_datetime(commit.author_date()),
        committer_name,
        committer_email,
        committer_date: format_datetime(commit.commit_date()),
        parents,
        tree_hash: format!("{}", commit.tree()),
        message: linkify_message(&String::from_utf8_lossy(commit.message())),
        notes: None,
        total_additions: 0,
        total_deletions: 0,
        files: Vec::new(),
        complete: false,
    };
    on_partial(base.clone());

    let notes = RefCell::new(None);
    let notes = &notes;
    let load_notes = async {
        *notes.borrow_mut() = commit_note(repo, oid).await;
    };

    let build_diff = async {
        let parent_tree = match parent_commits.first() {
            Some(parent) => repo
                .lookup_object(parent.tree())
                .await
                .context("lookup parent tree")?
                .tree()
                .map_err(GitError::from)
                .context("unexpected object type")?,
            // A root commit is diffed against the empty tree
            None => Tree::empty(),
        };
        let td = repo
            .tree_diff(&parent_tree, &commit_tree)
            .await
            .context("tree diff")?;

        anyhow::Ok(
            stream_diff(repo, &td, |files| {
                on_partial(CommitProps {
                    total_additions: files.iter().map(FileRow::additions).sum(),
                    total_deletions: files.iter().map(FileRow::deletions).sum(),
                    files: files.to_vec(),
                    notes: notes.borrow().clone(),
                    ..base.clone()
                });
            })
            .await,
        )
    };

    let (_, files) = futures::join!(load_notes, build_diff);
    let files = files?;

    Ok(CommitProps {
        total_additions: files.iter().map(FileRow::additions).sum(),
        total_deletions: files.iter().map(FileRow::deletions).sum(),
        files,
        notes: notes.borrow().clone(),
        complete: true,
        ..base
    })
}

/// The commit's note, split for rendering, or `None` when it has none.
async fn commit_note(repo: &CachingRepo, oid: ObjectId) -> Option<Vec<MessageSegment>> {
    let note = repo.note(oid).await.ok()??;
    Some(linkify_message(&String::from_utf8_lossy(&note)))
}

/// Resolve a SHA written into a URL — the one in `#!/commit/…`, or a `?h=`
/// value that named no ref — to the object it names.
///
/// A full 40-character hash decodes directly, with no I/O — the case every
/// link this app generates itself takes. Anything shorter is one of git's
/// abbreviated forms, as quoted in commit messages and linkified by
/// [`linkify_message`], and has to be expanded against the repository. Like
/// git, we refuse to pick between objects that share an abbreviation rather
/// than sending the reader to an arbitrary one.
pub(crate) async fn resolve_sha(repo: &CachingRepo, sha: &str) -> anyhow::Result<ObjectId> {
    if let Some(oid) = ObjectId::from_hex(sha.as_bytes()) {
        return Ok(oid);
    }
    let prefix = ObjectIdPrefix::from_hex(sha.as_bytes())
        .ok_or_else(|| anyhow::anyhow!("invalid SHA: {sha}"))?;
    match repo
        .resolve_prefix(&prefix)
        .await
        .context(format!("resolve {sha}"))?
    {
        PrefixResolution::Found(oid) => Ok(oid),
        PrefixResolution::Ambiguous => Err(anyhow::anyhow!("ambiguous SHA: {sha}")),
        // Abbreviations of objects that exist only as loose files can't be
        // expanded (see `Repo::resolve_prefix`), so they land here too.
        PrefixResolution::NotFound => Err(anyhow::anyhow!("unknown SHA: {sha}")),
    }
}

/// A token is treated as a commit reference if it is a run of 7-40 lowercase
/// hex digits, i.e. a full SHA-1 or one of git's abbreviated forms.
///
/// All-digit runs are included, so a date ("20250101") or a bug number
/// ("1234567") is hex-shaped enough to be linkified. Whether such a link goes
/// anywhere is left to [`resolve_sha`]: one that names no object reports
/// "unknown SHA" rather than rendering as a commit. The alternative — requiring
/// an `a`-`f` — would silence those, but at the cost of the all-digit
/// abbreviations, which are real references a reader would want to follow.
fn is_sha1(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Flush the current alphanumeric run: emit it as a `Sha` segment if it looks
/// like a commit reference, otherwise fold it into the running text buffer.
fn flush_token(token: &mut String, text: &mut String, segments: &mut Vec<MessageSegment>) {
    if is_sha1(token) {
        if !text.is_empty() {
            segments.push(MessageSegment::Text(std::mem::take(text)));
        }
        segments.push(MessageSegment::Sha(std::mem::take(token)));
    } else {
        text.push_str(token);
        token.clear();
    }
}

/// Split a commit message into text/SHA segments. SHA-1 references become links
/// to the referenced commit; everything else is plain text. Escaping is left to
/// Yew, which encodes text nodes when it renders them.
fn linkify_message(message: &str) -> Vec<MessageSegment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut token = String::new();

    for c in message.chars() {
        // Word boundaries are ASCII alphanumerics; anything else ends the
        // current token so e.g. a hash inside "word_abc1234" is not matched.
        if c.is_ascii_alphanumeric() {
            token.push(c);
        } else {
            flush_token(&mut token, &mut text, &mut segments);
            text.push(c);
        }
    }
    flush_token(&mut token, &mut text, &mut segments);
    if !text.is_empty() {
        segments.push(MessageSegment::Text(text));
    }
    segments
}

/// The bytes of one side of a change, or nothing when the file did not exist
/// on that side.
async fn load_side(repo: &CachingRepo, side: Option<Side>) -> Vec<u8> {
    let Some(side) = side else {
        return Vec::new();
    };
    match repo.lookup_object(side.id).await {
        Ok(Object::Blob(b)) => b.data_owned(),
        Ok(_) => format!("{}", side.id).into_bytes(),
        Err(_) => Vec::new(),
    }
}

/// The two sides of a diff entry, absent where the file did not exist.
fn sides(entry: &DiffEntry<(ObjectId, ObjectId)>) -> (Option<Side>, Option<Side>) {
    match entry {
        DiffEntry::LeftOnly {
            entry_type,
            content: (old, _),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *entry_type,
            }),
            None,
        ),
        DiffEntry::RightOnly {
            entry_type,
            content: (_, new),
            ..
        } => (
            None,
            Some(Side {
                id: *new,
                entry_type: *entry_type,
            }),
        ),
        DiffEntry::Both {
            left_type,
            right_type,
            content: (old, new),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *left_type,
            }),
            Some(Side {
                id: *new,
                entry_type: *right_type,
            }),
        ),
    }
}

/// Rescale the diffstat bars to the current widest file (0–40 columns). Called
/// before each progress emit, so bars re-normalise as larger files arrive; the
/// final return leaves them at their finished widths.
fn recompute_bars(files: &mut [FileRow]) {
    let max_changes = files
        .iter()
        .map(|f| f.additions() + f.deletions())
        .max()
        .unwrap_or(1)
        .max(1);

    for f in files {
        let total = f.additions() + f.deletions();
        let bar_total = total * 40 / max_changes;
        f.bar_add = f
            .additions()
            .checked_mul(bar_total)
            .and_then(|n| n.checked_div(total))
            .unwrap_or(0);
        f.bar_del = bar_total - f.bar_add;
    }
}

/// How often, in milliseconds of wall time, to emit a partial diff while
/// streaming. Cached blobs resolve in a back-to-back microtask burst that would
/// otherwise never yield the renderer a turn; emitting (and yielding) on a time
/// budget paints progressively without re-rendering the whole diff once per
/// file. A small/fast diff never trips it and just renders once at the end.
const DIFF_EMIT_INTERVAL_MS: f64 = 50.0;

/// Diff every changed file, calling `on_progress` as it goes. The diffstat's
/// file list is emitted immediately from the tree diff (paths known up front,
/// stats `pending`); then each file's blobs are loaded — kicked off all at once
/// so the round-trips overlap, but consumed in tree order via [`FuturesOrdered`]
/// so the diff body fills in top-to-bottom — and its counts/lines folded in,
/// re-emitting roughly every [`DIFF_EMIT_INTERVAL_MS`]. Diffing itself is
/// CPU-bound and stays sequential.
async fn stream_diff(
    repo: &CachingRepo,
    td: &TreeDiff,
    on_progress: impl Fn(&[FileRow]),
) -> Vec<FileRow> {
    // The changed-file list is known from the tree diff before any blob loads,
    // so show it right away with the stats column blank.
    let mut files: Vec<FileRow> = td
        .entries()
        .iter()
        .map(|entry| FileRow {
            path: String::from_utf8_lossy(entry.path().as_slice()).into_owned(),
            diff: None,
            bar_add: 0,
            bar_del: 0,
        })
        .collect();
    on_progress(&files);
    yield_to_browser().await;

    let mut loads: FuturesOrdered<_> = td
        .entries()
        .iter()
        .map(|entry| async move {
            let (old, new) = sides(entry);
            let (old_data, new_data) = futures::join!(load_side(repo, old), load_side(repo, new));
            (old, new, old_data, new_data)
        })
        .collect();

    // `FuturesOrdered` yields in tree order, matching `files`.
    let mut idx = 0;
    let mut last_emit = js_sys::Date::now();
    while let Some((old, new, old_data, new_data)) = loads.next().await {
        files[idx].diff = Some(gib_patch::diff_file(
            &files[idx].path,
            old,
            new,
            &old_data,
            &new_data,
        ));
        idx += 1;
        // Paint progressively, but only every ~50ms so a large diff isn't
        // re-rendered (and the growing line list re-cloned) once per file. Yield
        // to the event loop after emitting so the browser actually repaints —
        // cached blobs drain as microtasks that wouldn't otherwise let it.
        if js_sys::Date::now() - last_emit >= DIFF_EMIT_INTERVAL_MS {
            recompute_bars(&mut files);
            on_progress(&files);
            yield_to_browser().await;
            last_emit = js_sys::Date::now();
        }
    }

    recompute_bars(&mut files);
    files
}

/// The Yew component used to mount the commit view into the DOM. The markup
/// lives in the plain `commit_view` function below so it can be exercised
/// without a renderer.
#[function_component(CommitView)]
pub(crate) fn commit_view_component(props: &CommitProps) -> Html {
    commit_view(props)
}

pub(crate) fn commit_view(props: &CommitProps) -> Html {
    let CommitProps {
        meta,
        author_name,
        author_email,
        author_date,
        committer_name,
        committer_email,
        committer_date,
        parents,
        tree_hash,
        message,
        notes,
        total_additions,
        total_deletions,
        files,
        complete: _,
    } = props;

    html! {
        <>
            <table class="tag-table">
                <tbody>
                    <tr>
                        <td class="label">{ "author" }</td>
                        <td>{ format!("{author_name} <{author_email}> {author_date}") }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "committer" }</td>
                        <td>{ format!("{committer_name} <{committer_email}> {committer_date}") }</td>
                    </tr>
                    { for parents.iter().enumerate().map(|(i, p)| parent_row(i == 0, p)) }
                    <tr>
                        <td class="label">{ "commit" }</td>
                        <td class="mono">{ meta.hash.clone() }{ patch_link(props) }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "tree" }</td>
                        <td class="mono">
                            <a href={crate::route::tree_url("", Some(&meta.hash), false)}>
                                { tree_hash.clone() }
                            </a>
                        </td>
                    </tr>
                </tbody>
            </table>

            <pre class="tag-message">{ for message.iter().map(message_segment) }</pre>

            if let Some(notes) = notes {
                <div class="notes-header">{ "Notes" }</div>
                <pre class="notes">{ for notes.iter().map(message_segment) }</pre>
            }

            if !files.is_empty() {
                <>
                    <div class="diffstat">
                        <p class="diffstat-summary">
                            { diffstat_summary(files.len(), *total_additions, *total_deletions) }
                        </p>
                        <table class="diffstat-table">
                            { for files.iter().map(diffstat_row) }
                        </table>
                    </div>
                    <pre class="diff-pre">
                        { for files.iter().filter_map(|f| f.diff.as_ref())
                            .flat_map(|d| d.lines.iter()).map(diff_line) }
                    </pre>
                </>
            }
        </>
    }
}

fn parent_row(first: bool, p: &ParentRef) -> Html {
    let href = format!("#!/commit/{}", p.hash);
    html! {
        <tr key={p.hash.clone()}>
            <td class="label">{ if first { "parent" } else { "" } }</td>
            <td class="mono"><a href={href}>{ p.short.clone() }</a></td>
        </tr>
    }
}

fn message_segment(seg: &MessageSegment) -> Html {
    match seg {
        MessageSegment::Text(t) => html! { { t.clone() } },
        MessageSegment::Sha(s) => {
            let href = format!("#!/commit/{s}");
            html! { <a href={href}>{ s.clone() }</a> }
        }
    }
}

fn diffstat_row(f: &FileRow) -> Html {
    // Before its blobs load, a file's name is known but its stats aren't: show
    // the name with the remaining columns blank until they stream in.
    if f.diff.is_none() {
        return html! {
            <tr key={f.path.clone()}>
                <td class="diffstat-name">{ f.path.clone() }</td>
                <td class="diffstat-count">{ crate::render::loading_dots() }</td>
                <td class="diffstat-bar-cell"></td>
                <td class="diffstat-pm"></td>
            </tr>
        };
    }
    // The bar widths are data-driven (0-40%), but the CSP (`style-src 'self'`)
    // forbids inline `style` attributes, so the width is selected via a
    // `bar-w-N` class defined in styles.css rather than set inline.
    let bar_add = format!("bar-add bar-w-{}", f.bar_add);
    let bar_del = format!("bar-del bar-w-{}", f.bar_del);
    html! {
        <tr key={f.path.clone()}>
            <td class="diffstat-name">{ f.path.clone() }</td>
            <td class="diffstat-count">{ f.additions() + f.deletions() }</td>
            <td class="diffstat-bar-cell">
                <span class="diffstat-bar">
                    <span class={bar_add}></span><span class={bar_del}></span>
                </span>
            </td>
            <td class="diffstat-pm">
                if f.additions() > 0 {
                    <span class="add-count">{ format!("+{}", f.additions()) }</span>
                }
                if f.deletions() > 0 {
                    <span class="del-count">{ format!("-{}", f.deletions()) }</span>
                }
            </td>
        </tr>
    }
}

fn diff_line(line: &PatchLine) -> Html {
    let class = match line.kind {
        LineKind::Meta => "diff-hunk",
        LineKind::Insert => "diff-add",
        LineKind::Delete => "diff-del",
        LineKind::Context => "diff-ctx",
    };
    // The trailing newline is part of the line's content inside the <pre>, so
    // each line's span ends with it (matching git's own line-oriented output).
    let content = format!("{}\n", line.text);
    html! { <span class={class}>{ content }</span> }
}

/// The `(patch)` link beside the commit hash, where cgit's commit view puts it.
fn patch_link(props: &CommitProps) -> Html {
    if !props.complete {
        return Html::default();
    }
    let props = props.clone();
    let onclick = Callback::from(move |e: MouseEvent| {
        // There is no patch page to navigate to: the href only makes this a
        // link, and the download takes the place of following it.
        e.prevent_default();
        download_bytes(
            &format!("{}.patch", props.meta.hash),
            "text/plain",
            build_patch(&props).as_bytes(),
        );
    });
    html! {
        <>{ " (" }<a class="patch-link" href="#" {onclick}>{ "patch" }</a>{ ")" }</>
    }
}

/// The commit as a patch file, from the diff the view is already holding.
fn build_patch(props: &CommitProps) -> String {
    gib_patch::format_patch(
        &props.meta,
        props.files.iter().filter_map(|f| f.diff.as_ref()),
        &format!("webgit {}", crate::render::about::COMMIT),
    )
}

fn diffstat_summary(files: usize, additions: usize, deletions: usize) -> String {
    format!(
        "{files} {} changed, {additions} {}(+), {deletions} {}(-)",
        plural(files, "file", "files"),
        plural(additions, "insertion", "insertions"),
        plural(deletions, "deletion", "deletions"),
    )
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `CommitView` to a static HTML string via SSR. See `render::tag`
    /// for why we go through SSR rather than comparing `VNode`s.
    fn render(props: CommitProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<CommitView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        prettify(&html)
    }

    /// Break adjacent tags onto their own lines for readable snapshots, but emit
    /// the contents of `<pre>` elements verbatim. Unlike the other ported views,
    /// the commit diff renders sibling `<span>`s *inside* `<pre class="diff-pre">`,
    /// so the blanket `><` split the others use would inject newlines into
    /// whitespace the browser renders literally.
    fn prettify(html: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        while let Some(open) = rest.find("<pre") {
            out.push_str(&rest[..open].replace("><", ">\n<"));
            rest = &rest[open..];
            match rest.find("</pre>") {
                Some(close) => {
                    let end = close + "</pre>".len();
                    out.push_str(&rest[..end]);
                    rest = &rest[end..];
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        }
        out.push_str(&rest.replace("><", ">\n<"));
        out
    }

    fn base_fixture() -> CommitProps {
        CommitProps {
            meta: PatchMeta {
                hash: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
                author_name: "Kunal Mehta".to_string(),
                author_email: "author@example.org".to_string(),
                date: "Thu, 15 Jan 2026 12:34:56 +0000".to_string(),
                message: MESSAGE.to_string(),
            },
            author_name: "Kunal Mehta".to_string(),
            author_email: "author@example.org".to_string(),
            author_date: "2026-01-15 12:34:56 +00:00".to_string(),
            committer_name: "Committer Person".to_string(),
            committer_email: "committer@example.org".to_string(),
            committer_date: "2026-01-15 13:00:00 +00:00".to_string(),
            parents: vec![],
            tree_hash: "fedcba98fedcba98fedcba98fedcba98fedcba98".to_string(),
            message: linkify_message(MESSAGE),
            notes: None,
            total_additions: 0,
            total_deletions: 0,
            files: vec![],
            complete: true,
        }
    }

    const MESSAGE: &str =
        "Fix the thing\n\nLonger explanation with <html> & \"chars\".\nSee 0123abcd for context.";

    /// A file row with a diff already in it, as the view holds one once its
    /// blobs have loaded.
    fn row(path: &str, old: &[u8], new: &[u8], bar_add: usize, bar_del: usize) -> FileRow {
        // The two sides carry different (made-up) blob ids, as a real change
        // does, so the `index` header line comes out as it would in the app.
        let blob = |data: &[u8], hex: &[u8]| {
            (!data.is_empty()).then_some(Side {
                id: ObjectId::from_hex(hex).unwrap(),
                entry_type: gib::object::TreeEntryType::File,
            })
        };
        FileRow {
            path: path.to_string(),
            diff: Some(gib_patch::diff_file(
                path,
                blob(old, b"1111111111111111111111111111111111111111"),
                blob(new, b"2222222222222222222222222222222222222222"),
                old,
                new,
            )),
            bar_add,
            bar_del,
        }
    }

    /// A file row whose diff has `additions` added lines and `deletions`
    /// removed ones, for the bar arithmetic.
    fn file(path: &str, additions: usize, deletions: usize) -> FileRow {
        let old = "old\n".repeat(deletions);
        let new = "new\n".repeat(additions);
        row(path, old.as_bytes(), new.as_bytes(), 0, 0)
    }

    #[test]
    fn test_recompute_bars_normalises_to_widest() {
        // The widest file (4 changes) fills the 40-column bar split by its
        // add/del ratio; a smaller file scales proportionally.
        let mut files = vec![file("big.rs", 3, 1), file("small", 1, 0)];
        recompute_bars(&mut files);
        assert_eq!((files[0].bar_add, files[0].bar_del), (30, 10));
        assert_eq!((files[1].bar_add, files[1].bar_del), (10, 0));
    }

    #[test]
    fn test_recompute_bars_rescales_as_files_stream_in() {
        // Mid-stream the first (and only) file owns the full bar...
        let mut files = vec![file("first", 4, 0)];
        recompute_bars(&mut files);
        assert_eq!((files[0].bar_add, files[0].bar_del), (40, 0));
        // ...then a larger file arriving rescales the earlier one down.
        files.push(file("bigger", 0, 8));
        recompute_bars(&mut files);
        assert_eq!((files[0].bar_add, files[0].bar_del), (20, 0));
        assert_eq!((files[1].bar_add, files[1].bar_del), (0, 40));
    }

    #[test]
    fn test_linkify_message() {
        use MessageSegment::{Sha, Text};
        // A 7-40 char hex run becomes a SHA segment; surrounding text stays text.
        assert_eq!(
            linkify_message("see 0123abcd <ok>"),
            vec![
                Text("see ".to_string()),
                Sha("0123abcd".to_string()),
                Text(" <ok>".to_string()),
            ]
        );
        // Full 40-char SHA-1.
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(linkify_message(full), vec![Sha(full.to_string())]);
        // Too short (<7), too long (>40), uppercase, or embedded in a word: text.
        assert_eq!(linkify_message("abc123"), vec![Text("abc123".to_string())]);
        assert_eq!(
            linkify_message("0123ABCD"),
            vec![Text("0123ABCD".to_string())]
        );
        assert_eq!(
            linkify_message("x0123abcd"),
            vec![Text("x0123abcd".to_string())]
        );
        assert_eq!(linkify_message(&"a".repeat(41)), vec![Text("a".repeat(41))]);
    }

    #[test]
    fn test_linkify_message_links_all_digit_runs() {
        use MessageSegment::Sha;
        // An all-digit run is a valid abbreviation, so it is linkified like any
        // other. Resolution decides whether it names anything — a date or a bug
        // number simply reports "unknown SHA" when followed.
        for token in ["20250101", "1234567", "1234567890", "2025010a"] {
            assert_eq!(
                linkify_message(token),
                vec![Sha(token.to_string())],
                "{token} should be linkified"
            );
        }
    }

    #[test]
    fn test_linkify_message_no_spurious_links() {
        // Markup and quotes are never mistaken for SHAs: the whole string stays
        // one text run, and Yew escapes it at render time (see the snapshots).
        let attack = "<script>alert('xss')</script> & \"quotes\"";
        assert_eq!(
            linkify_message(attack),
            vec![MessageSegment::Text(attack.to_string())]
        );
    }

    /// A commit view with one modified file, as it looks once its diff has
    /// streamed in.
    fn patch_fixture() -> CommitProps {
        let mut props = base_fixture();
        props.parents = vec![ParentRef {
            hash: "1111111111111111111111111111111111111111".to_string(),
            short: "11111111".to_string(),
        }];
        props.files = vec![row(
            "src/main.rs",
            b"alpha\nbeta\n",
            b"alpha\nbeta changed\n",
            20,
            20,
        )];
        props.total_additions = 1;
        props.total_deletions = 1;
        props
    }

    #[test]
    fn test_build_patch() {
        // What the "(patch)" link downloads. The format itself is `gib-patch`'s
        // and is checked against `git format-patch` there; what this pins is
        // the wiring — the view's metadata and its streamed diff reaching the
        // patch writer intact, under this build's signature.
        let patch =
            build_patch(&patch_fixture()).replace(crate::render::about::COMMIT, "<version>");
        insta::assert_snapshot!(patch);
    }

    #[test]
    fn test_the_patch_keeps_the_contact_the_commit_records() {
        // The header shows the mailmapped author; the patch must not, because
        // `git format-patch` is not one of the commands `log.mailmap` applies
        // to — a patch carries the authorship it will be applied under.
        let mut props = patch_fixture();
        props.author_name = "Proper Name".to_string();
        props.author_email = "proper@example.org".to_string();

        let patch = build_patch(&props);
        assert!(
            patch.contains("From: Kunal Mehta <author@example.org>"),
            "{patch}"
        );
        assert!(!patch.contains("proper@example.org"), "{patch}");

        let rendered = render(props);
        assert!(
            rendered.contains("Proper Name &lt;proper@example.org&gt;"),
            "{rendered}"
        );
    }

    #[test]
    fn test_patch_link_waits_for_the_whole_diff() {
        // A patch built from an unfinished view would be missing files, so the
        // link is only offered once the diff is done...
        let props = patch_fixture();
        assert!(render(props.clone()).contains("patch-link"));

        // ...both while a file's blobs are still loading...
        let mut streaming = props.clone();
        streaming.complete = false;
        streaming.files[0].diff = None;
        assert!(!render(streaming).contains("patch-link"));

        // ...and on the metadata-only first frame, whose empty file list would
        // otherwise look exactly like a root commit's and hand over a patch
        // with no diff in it.
        let mut first_frame = props;
        first_frame.complete = false;
        first_frame.files = vec![];
        assert!(!render(first_frame).contains("patch-link"));
    }

    #[test]
    fn test_commit_html_root_commit() {
        // No parents and no diff: the diffstat section should be absent.
        insta::assert_snapshot!(render(base_fixture()));
    }

    #[test]
    fn test_commit_html_root_commit_with_diff() {
        // A root commit is diffed against the empty tree, so it has files like
        // any other commit: the diffstat and the diff itself both render, with
        // no `parent` row above them.
        let mut props = base_fixture();
        props.files = vec![row("src/main.rs", b"", b"alpha\nbeta\n", 40, 0)];
        props.total_additions = 2;
        insta::assert_snapshot!(render(props));
    }

    #[test]
    fn test_commit_html_merge_with_diff() {
        let mut props = base_fixture();
        props.parents = vec![
            ParentRef {
                hash: "1111111111111111111111111111111111111111".to_string(),
                short: "11111111".to_string(),
            },
            ParentRef {
                hash: "2222222222222222222222222222222222222222".to_string(),
                short: "22222222".to_string(),
            },
        ];
        props.files = vec![
            row(
                "src/main.rs",
                b"fn main() {\n    println!(\"old\");\n}\n",
                b"fn main() {\n    println!(\"<new> & escaped\");\n}\n",
                30,
                10,
            ),
            row("README", b"", b"hello\n", 10, 0),
        ];
        props.total_additions = 2;
        props.total_deletions = 1;
        insta::assert_snapshot!(render(props));
    }

    #[test]
    fn test_commit_html_diff_pending() {
        // The streamed first frame: the changed-file list is known (from the
        // tree diff) but no blobs have loaded, so every row is pending — name
        // shown, stats columns blank — the diff body is still empty, and the
        // patch link has nothing to offer yet.
        let mut props = base_fixture();
        props.parents = vec![ParentRef {
            hash: "1111111111111111111111111111111111111111".to_string(),
            short: "11111111".to_string(),
        }];
        props.complete = false;
        props.files = ["src/main.rs", "README"]
            .into_iter()
            .map(|path| FileRow {
                path: path.to_string(),
                diff: None,
                bar_add: 0,
                bar_del: 0,
            })
            .collect();
        insta::assert_snapshot!(render(props));
    }

    #[test]
    fn test_commit_html_with_notes() {
        // A note is rendered under its own heading, below the message and
        // above the diff, with the same SHA linkification the message gets.
        let mut props = base_fixture();
        props.notes = Some(linkify_message(
            "Cherry-picked from 0123abcd.\nReviewed with <angle> & \"quoted\" text.\n",
        ));
        insta::assert_snapshot!(render(props));
    }

    #[test]
    fn commit_html_without_notes_has_no_notes_markup() {
        // The usual case: no notes ref, or none for this commit. Nothing is
        // drawn for it — not an empty box, and no heading.
        let html = render(base_fixture());
        assert!(!html.contains("notes"));
    }
}
