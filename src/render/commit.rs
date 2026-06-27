use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::yield_to_browser;
use futures::stream::{FuturesOrdered, StreamExt};
use git_async::diff::{DiffEntry, TreeDiff};
use git_async::error::Error as GitError;
use git_async::object::{Object, ObjectId};
use similar::TextDiffConfig;
use yew::prelude::*;

#[derive(PartialEq, Clone)]
struct ParentRef {
    hash: String,
    short: String,
}

#[derive(PartialEq, Clone)]
struct DiffLine {
    kind: String,
    content: String,
}

#[derive(PartialEq, Clone)]
struct FileDiff {
    path: String,
    additions: usize,
    deletions: usize,
    bar_add: usize,
    bar_del: usize,
    /// True between the tree diff (which gives us the path) and this file's
    /// blobs loading (which give us the counts): the row shows the name with the
    /// stats column blank until they arrive.
    pending: bool,
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
    hash: String,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    parents: Vec<ParentRef>,
    tree_hash: String,
    message: Vec<MessageSegment>,
    total_additions: usize,
    total_deletions: usize,
    files: Vec<FileDiff>,
    diff_lines: Vec<DiffLine>,
}

/// Build a commit view, calling `on_partial` first with the metadata alone
/// (empty diff) and then once per file as the diff streams in, so the header
/// renders immediately and a large diff fills in top-to-bottom. The returned
/// value is the complete view.
pub(crate) async fn build_commit(
    repo: &CachingRepo,
    sha: &str,
    on_partial: impl Fn(CommitProps),
) -> anyhow::Result<CommitProps> {
    let oid =
        ObjectId::from_hex(sha.as_bytes()).ok_or_else(|| anyhow::anyhow!("invalid SHA: {sha}"))?;
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
    let base = CommitProps {
        hash: format!("{}", commit.id()),
        author_name: String::from_utf8_lossy(commit.author_name()).into_owned(),
        author_email: String::from_utf8_lossy(commit.author_email()).into_owned(),
        author_date: commit.author_date().to_string(),
        committer_name: String::from_utf8_lossy(commit.committer_name()).into_owned(),
        committer_email: String::from_utf8_lossy(commit.committer_email()).into_owned(),
        committer_date: commit.commit_date().to_string(),
        parents,
        tree_hash: format!("{}", commit.tree()),
        message: linkify_message(&String::from_utf8_lossy(commit.message())),
        total_additions: 0,
        total_deletions: 0,
        files: Vec::new(),
        diff_lines: Vec::new(),
    };
    on_partial(base.clone());

    let Some(parent) = parent_commits.first() else {
        // Root commit: nothing to diff against.
        return Ok(base);
    };

    let parent_tree = repo
        .lookup_object(parent.tree())
        .await
        .context("lookup parent tree")?
        .tree()
        .map_err(GitError::from)
        .context("unexpected object type")?;
    let td = repo
        .tree_diff(&parent_tree, &commit_tree)
        .await
        .context("tree diff")?;

    let (files, diff_lines) = stream_diff(repo, &td, |files, diff_lines| {
        on_partial(CommitProps {
            total_additions: files.iter().map(|f| f.additions).sum(),
            total_deletions: files.iter().map(|f| f.deletions).sum(),
            files: files.to_vec(),
            diff_lines: diff_lines.to_vec(),
            ..base.clone()
        });
    })
    .await;

    Ok(CommitProps {
        total_additions: files.iter().map(|f| f.additions).sum(),
        total_deletions: files.iter().map(|f| f.deletions).sum(),
        files,
        diff_lines,
        ..base
    })
}

/// A token is treated as a commit reference if it is a run of 7-40 lowercase
/// hex digits, i.e. a full SHA-1 or one of git's abbreviated forms.
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

/// Heuristic matching git's: a blob is treated as binary if a NUL byte appears
/// in its leading bytes. git scans the first 8000 bytes, so do the same.
fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

async fn load_blob(repo: &CachingRepo, id: ObjectId) -> Vec<u8> {
    if id.bytes() == &[0u8; 20] {
        return Vec::new();
    }
    match repo.lookup_object(id).await {
        Ok(Object::Blob(b)) => b.data_owned(),
        Ok(_) => format!("{id}").into_bytes(),
        Err(_) => Vec::new(),
    }
}

/// Compute the rendered diff lines and (additions, deletions) for a single
/// changed file. The returned lines always begin with the `diff --git` header;
/// a file detected as binary yields just that header plus a "Binary files
/// differ" line and zero counts. The `+++`/`---` unified-diff headers are
/// emitted but excluded from the counts, matching `git`'s diffstat.
fn diff_file(path: &str, old_data: Vec<u8>, new_data: Vec<u8>) -> (Vec<DiffLine>, usize, usize) {
    let mut lines = vec![DiffLine {
        kind: "hunk".to_string(),
        content: format!("diff --git a/{path} b/{path}"),
    }];

    // Git treats a blob as binary if it contains a NUL byte; in that case it
    // shows "Binary files differ" rather than a line-by-line diff, which would
    // be meaningless (and potentially huge). Mirror that here.
    if is_binary(&old_data) || is_binary(&new_data) {
        lines.push(DiffLine {
            kind: "ctx".to_string(),
            content: format!("Binary files a/{path} and b/{path} differ"),
        });
        return (lines, 0, 0);
    }

    let text_diff = TextDiffConfig::default().diff_lines(old_data, new_data);
    let udiff = text_diff
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();

    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in udiff.lines() {
        let lkind = match line.chars().next() {
            Some('+') => {
                if !line.starts_with("+++") {
                    additions += 1;
                }
                "add"
            }
            Some('-') => {
                if !line.starts_with("---") {
                    deletions += 1;
                }
                "del"
            }
            Some('@') => "hunk",
            _ => "ctx",
        };
        lines.push(DiffLine {
            kind: lkind.to_string(),
            content: line.to_string(),
        });
    }

    (lines, additions, deletions)
}

/// Rescale the diffstat bars to the current widest file (0–40 columns). Called
/// before each progress emit, so bars re-normalise as larger files arrive; the
/// final return leaves them at their finished widths.
fn recompute_bars(files: &mut [FileDiff]) {
    let max_changes = files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(1)
        .max(1);

    for f in files {
        let total = f.additions + f.deletions;
        let bar_total = total * 40 / max_changes;
        f.bar_add = f
            .additions
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
    on_progress: impl Fn(&[FileDiff], &[DiffLine]),
) -> (Vec<FileDiff>, Vec<DiffLine>) {
    // The changed-file list is known from the tree diff before any blob loads,
    // so show it right away with the stats column blank.
    let mut files: Vec<FileDiff> = td
        .entries()
        .iter()
        .map(|entry| FileDiff {
            path: String::from_utf8_lossy(entry.path().as_slice()).into_owned(),
            additions: 0,
            deletions: 0,
            bar_add: 0,
            bar_del: 0,
            pending: true,
        })
        .collect();
    let mut diff_lines: Vec<DiffLine> = Vec::new();
    on_progress(&files, &diff_lines);
    yield_to_browser().await;

    let mut loads: FuturesOrdered<_> = td
        .entries()
        .iter()
        .map(|entry| async move {
            match entry {
                DiffEntry::LeftOnly {
                    content: (old_id, _),
                    ..
                } => (load_blob(repo, *old_id).await, Vec::new()),
                DiffEntry::RightOnly {
                    content: (_, new_id),
                    ..
                } => (Vec::new(), load_blob(repo, *new_id).await),
                DiffEntry::Both {
                    content: (old_id, new_id),
                    ..
                } => {
                    futures::join!(load_blob(repo, *old_id), load_blob(repo, *new_id))
                }
            }
        })
        .collect();

    // `FuturesOrdered` yields in tree order, matching `files`.
    let mut idx = 0;
    let mut last_emit = js_sys::Date::now();
    while let Some((old_data, new_data)) = loads.next().await {
        let path = files[idx].path.clone();
        let (lines, additions, deletions) = diff_file(&path, old_data, new_data);
        diff_lines.extend(lines);
        files[idx].additions = additions;
        files[idx].deletions = deletions;
        files[idx].pending = false;
        idx += 1;
        // Paint progressively, but only every ~50ms so a large diff isn't
        // re-rendered (and the growing line list re-cloned) once per file. Yield
        // to the event loop after emitting so the browser actually repaints —
        // cached blobs drain as microtasks that wouldn't otherwise let it.
        if js_sys::Date::now() - last_emit >= DIFF_EMIT_INTERVAL_MS {
            recompute_bars(&mut files);
            on_progress(&files, &diff_lines);
            yield_to_browser().await;
            last_emit = js_sys::Date::now();
        }
    }

    recompute_bars(&mut files);
    (files, diff_lines)
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
        hash,
        author_name,
        author_email,
        author_date,
        committer_name,
        committer_email,
        committer_date,
        parents,
        tree_hash,
        message,
        total_additions,
        total_deletions,
        files,
        diff_lines,
    } = props.clone();

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
                        <td class="mono">{ hash }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "tree" }</td>
                        <td class="mono">{ tree_hash }</td>
                    </tr>
                </tbody>
            </table>

            <pre class="tag-message">{ for message.iter().map(message_segment) }</pre>

            if !files.is_empty() {
                <>
                    <div class="diffstat">
                        <p class="diffstat-summary">
                            { diffstat_summary(files.len(), total_additions, total_deletions) }
                        </p>
                        <table class="diffstat-table">
                            { for files.iter().map(diffstat_row) }
                        </table>
                    </div>
                    <pre class="diff-pre">{ for diff_lines.iter().map(diff_line) }</pre>
                </>
            }
        </>
    }
}

fn parent_row(first: bool, p: &ParentRef) -> Html {
    let href = format!("#!/commit/{}", p.hash);
    html! {
        <tr>
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

fn diffstat_row(f: &FileDiff) -> Html {
    // Before its blobs load, a file's name is known but its stats aren't: show
    // the name with the remaining columns blank until they stream in.
    if f.pending {
        return html! {
            <tr>
                <td class="diffstat-name">{ f.path.clone() }</td>
                <td class="diffstat-count"></td>
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
        <tr>
            <td class="diffstat-name">{ f.path.clone() }</td>
            <td class="diffstat-count">{ f.additions + f.deletions }</td>
            <td class="diffstat-bar-cell">
                <span class="diffstat-bar">
                    <span class={bar_add}></span><span class={bar_del}></span>
                </span>
            </td>
            <td class="diffstat-pm">
                if f.additions > 0 {
                    <span class="add-count">{ format!("+{}", f.additions) }</span>
                }
                if f.deletions > 0 {
                    <span class="del-count">{ format!("-{}", f.deletions) }</span>
                }
            </td>
        </tr>
    }
}

fn diff_line(line: &DiffLine) -> Html {
    let class = format!("diff-{}", line.kind);
    // The trailing newline is part of the line's content inside the <pre>, so
    // each line's span ends with it (matching git's own line-oriented output).
    let content = format!("{}\n", line.content);
    html! { <span class={class}>{ content }</span> }
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
            hash: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            author_name: "Kunal Mehta".to_string(),
            author_email: "author@example.org".to_string(),
            author_date: "2026-01-15 12:34:56 +00:00".to_string(),
            committer_name: "Committer Person".to_string(),
            committer_email: "committer@example.org".to_string(),
            committer_date: "2026-01-15 13:00:00 +00:00".to_string(),
            parents: vec![],
            tree_hash: "fedcba98fedcba98fedcba98fedcba98fedcba98".to_string(),
            message: linkify_message(
                "Fix the thing\n\nLonger explanation with <html> & \"chars\".\nSee 0123abcd for context.",
            ),
            total_additions: 0,
            total_deletions: 0,
            files: vec![],
            diff_lines: vec![],
        }
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
    fn test_diff_file_modification() {
        let (lines, additions, deletions) = diff_file(
            "foo.txt",
            b"alpha\nbeta\n".to_vec(),
            b"alpha\nbeta changed\n".to_vec(),
        );
        assert_eq!(additions, 1);
        assert_eq!(deletions, 1);
        // The first line is always the git-style header.
        assert_eq!(lines[0].kind, "hunk");
        assert_eq!(lines[0].content, "diff --git a/foo.txt b/foo.txt");
        // A hunk marker and the changed lines are present and classified.
        assert!(
            lines
                .iter()
                .any(|l| l.kind == "hunk" && l.content.starts_with("@@"))
        );
        assert!(lines.iter().any(|l| l.kind == "add"
            && l.content.starts_with('+')
            && !l.content.starts_with("+++")));
        assert!(lines.iter().any(|l| l.kind == "del"
            && l.content.starts_with('-')
            && !l.content.starts_with("---")));
    }

    #[test]
    fn test_diff_file_pure_addition_and_deletion() {
        // A brand-new file: every line counts as an addition, none as deletion.
        let (_lines, additions, deletions) =
            diff_file("new.txt", Vec::new(), b"one\ntwo\nthree\n".to_vec());
        assert_eq!((additions, deletions), (3, 0));

        // A removed file: the reverse.
        let (_lines, additions, deletions) =
            diff_file("gone.txt", b"one\ntwo\n".to_vec(), Vec::new());
        assert_eq!((additions, deletions), (0, 2));
    }

    #[test]
    fn test_diff_file_binary() {
        // A NUL byte makes the file binary: no line-by-line diff, zero counts.
        let (lines, additions, deletions) =
            diff_file("blob.bin", b"\0\x01\x02".to_vec(), b"\0\x03".to_vec());
        assert_eq!((additions, deletions), (0, 0));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "diff --git a/blob.bin b/blob.bin");
        assert_eq!(lines[1].kind, "ctx");
        assert_eq!(
            lines[1].content,
            "Binary files a/blob.bin and b/blob.bin differ"
        );
    }

    fn file(path: &str, additions: usize, deletions: usize) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            additions,
            deletions,
            bar_add: 0,
            bar_del: 0,
            pending: false,
        }
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
    fn test_linkify_message_no_spurious_links() {
        // Markup and quotes are never mistaken for SHAs: the whole string stays
        // one text run, and Yew escapes it at render time (see the snapshots).
        let attack = "<script>alert('xss')</script> & \"quotes\"";
        assert_eq!(
            linkify_message(attack),
            vec![MessageSegment::Text(attack.to_string())]
        );
    }

    #[test]
    fn test_commit_html_root_commit() {
        // No parents and no diff: the diffstat section should be absent.
        insta::assert_snapshot!(render(base_fixture()));
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
            FileDiff {
                path: "src/main.rs".to_string(),
                additions: 3,
                deletions: 1,
                bar_add: 30,
                bar_del: 10,
                pending: false,
            },
            FileDiff {
                path: "README".to_string(),
                additions: 1,
                deletions: 0,
                bar_add: 10,
                bar_del: 0,
                pending: false,
            },
        ];
        props.total_additions = 4;
        props.total_deletions = 1;
        props.diff_lines = vec![
            DiffLine {
                kind: "hunk".to_string(),
                content: "diff --git a/src/main.rs b/src/main.rs".to_string(),
            },
            DiffLine {
                kind: "hunk".to_string(),
                content: "@@ -1,3 +1,5 @@".to_string(),
            },
            DiffLine {
                kind: "ctx".to_string(),
                content: " fn main() {".to_string(),
            },
            DiffLine {
                kind: "del".to_string(),
                content: "-    println!(\"old\");".to_string(),
            },
            DiffLine {
                kind: "add".to_string(),
                content: "+    println!(\"<new> & escaped\");".to_string(),
            },
        ];
        insta::assert_snapshot!(render(props));
    }

    #[test]
    fn test_commit_html_diff_pending() {
        // The streamed first frame: the changed-file list is known (from the
        // tree diff) but no blobs have loaded, so every row is `pending` — name
        // shown, stats columns blank — and the diff body is still empty.
        let mut props = base_fixture();
        props.parents = vec![ParentRef {
            hash: "1111111111111111111111111111111111111111".to_string(),
            short: "11111111".to_string(),
        }];
        props.files = vec![
            FileDiff {
                path: "src/main.rs".to_string(),
                additions: 0,
                deletions: 0,
                bar_add: 0,
                bar_del: 0,
                pending: true,
            },
            FileDiff {
                path: "README".to_string(),
                additions: 0,
                deletions: 0,
                bar_add: 0,
                bar_del: 0,
                pending: true,
            },
        ];
        insta::assert_snapshot!(render(props));
    }
}
