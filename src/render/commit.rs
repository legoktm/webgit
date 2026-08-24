//! The commit view: the header block, the diffstat, and the diff itself.
//!
//! The parts live alongside: [`stream`] diffs the files as their blobs land,
//! [`side_by_side`] lays the diff out in two columns, [`controls`] is the diff
//! options panel, [`message`] linkifies the commit message, and [`patch`] is
//! the downloadable `.patch`.

mod controls;
mod message;
mod patch;
mod side_by_side;
mod stream;

#[cfg(test)]
mod tests;

use controls::diff_controls;
use message::{MessageSegment, linkify_message, message_segment};
use patch::patch_link;
use side_by_side::diff_body;
use stream::stream_diff;

use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::{format_datetime, mapped_ident};
use crate::route::DiffView;
use gib::error::Error as GitError;
use gib::object::{ObjectId, ObjectIdPrefix, PrefixResolution, Tree};
use gib_mailmap::Mailmap;
use gib_patch::{FileDiff, LineKind, PatchLine, PatchMeta};
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
    /// The same file diffed at git's defaults, kept only when the view is *not*
    /// at them. A `.patch` is applied rather than read, so it cannot be the one
    /// on screen once the reader has widened the context or hidden whitespace —
    /// cgit draws the same line, its patch endpoint ignoring the diff controls
    /// entirely.
    patch_diff: Option<FileDiff>,
    /// The two halves of the diffstat bar, in columns. Recomputed as files
    /// arrive, so they are the view's own and not the patch's.
    bar_add: usize,
    bar_del: usize,
}

impl FileRow {
    /// The diff to put in a `.patch`: git's defaults, whatever the view shows.
    fn for_patch(&self) -> Option<&FileDiff> {
        self.patch_diff.as_ref().or(self.diff.as_ref())
    }

    fn additions(&self) -> usize {
        self.diff.as_ref().map_or(0, |d| d.additions)
    }

    fn deletions(&self) -> usize {
        self.diff.as_ref().map_or(0, |d| d.deletions)
    }
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
    /// The commit as the URL names it — empty when the URL is the id-less
    /// `#!/commit`, which follows HEAD. The diff controls rebuild the current
    /// URL from this, so a HEAD-following view keeps following HEAD.
    url_sha: String,
    /// The diff settings this view was built with, and the state the controls
    /// show as current.
    view: DiffView,
}

/// Build a commit view, calling `on_partial` first with the metadata alone
/// (empty diff) and then once per file as the diff streams in, so the header
/// renders immediately and a large diff fills in top-to-bottom. The returned
/// value is the complete view.
pub(crate) async fn build_commit(
    repo: &CachingRepo,
    mailmap: &Mailmap,
    sha: &str,
    url_sha: &str,
    view: DiffView,
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
        url_sha: url_sha.to_string(),
        view,
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
            stream_diff(repo, &td, view.diff_options(), |files| {
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
        url_sha,
        view,
    } = props;

    html! {
        <>
            // Before the header table and floated right, where cgit puts it
            // (`ui-commit.c`), so the controls sit level with the top of the
            // commit rather than chasing the diff down the page.
            { diff_controls(url_sha, *view) }

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
                    if view.shows_diff() {
                        { diff_body(files, view.side_by_side) }
                    }
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
