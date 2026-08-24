//! Turning a parsed [`Route`] into the props for the view it names: the
//! async half of routing, and the only part that touches the repository.

use super::{RefsRoute, Route, parse_hash};
use crate::RepoBundle;
use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::about::{AboutProps, build_about};
use crate::render::blame::{BlameProps, build_blame};
use crate::render::blob::{BlobProps, build_blob_props};
use crate::render::commit::{CommitProps, build_commit, resolve_sha};
use crate::render::log::{LogProps, LogQuery, build_log};
use crate::render::readme::{ReadmeProps, build_readme};
use crate::render::refs_all::{RefsAllProps, build_refs_all};
use crate::render::refs_heads::{RefsHeadsProps, build_refs_heads};
use crate::render::refs_tags::{RefsTagsProps, build_refs_tags};
use crate::render::snapshot::{SnapshotProps, build_snapshot};
use crate::render::summary::{SummaryProps, build_summary};
use crate::render::tag::{TagProps, build_tag};
use crate::render::tree::{TreeProps, build_tree_props};
use crate::render::{commit_for_entry, head_branch_name};
use gib::object::{ObjectId, ObjectIdPrefix, Tree, TreeEntryType};
use gib::reference::RefName;
use gib_mailmap::Mailmap;

// ---------------------------------------------------------------------------
// Tree / blob walking
// ---------------------------------------------------------------------------

async fn walk_to_tree(root: &Tree, path: &str, repo: &CachingRepo) -> Option<Tree> {
    let mut current = root.clone();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        let entry = current
            .entries()
            .find(|e| e.name() == component.as_bytes())?;
        if entry.entry_type() != TreeEntryType::Tree {
            return None;
        }
        let obj = repo.lookup_object(entry.id()).await.ok()?;
        current = obj.tree().ok()?;
    }
    Some(current)
}

/// Resolve `path` to a blob and hand back its bytes.
///
/// There is deliberately no size cap here. A loose object is a single zlib
/// stream that has to be inflated in full before anything can be read out of
/// it, so the whole blob is already in memory by the time this returns and an
/// early bail would save nothing. The cap lives where the expense actually is:
/// `build_blob_props` decides what to render before copying or splitting.
async fn walk_to_blob(root: &Tree, path: &str, repo: &CachingRepo) -> Option<(ObjectId, Vec<u8>)> {
    let (dir_path, filename) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let tree = walk_to_tree(root, dir_path, repo).await?;
    let entry = tree.entries().find(|e| e.name() == filename.as_bytes())?;
    let obj = repo.lookup_object(entry.id()).await.ok()?;
    let blob = obj.blob().ok()?;
    let id = blob.id();
    Some((id, blob.data_owned()))
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RefKind {
    Tag,
    Branch,
    /// A commit named directly by its hash in `?h=`, belonging to no branch or
    /// tag we resolved it through.
    Commit,
}

/// The abbreviated form of a hash, as shown wherever a full one would crowd out
/// what surrounds it. Eight characters, matching the commit view's parent links
/// and the snapshot of a detached HEAD.
///
/// Truncation is by character, not by byte slice: every hash reaching this is 40
/// hex digits, but a helper that panics on a short input is a trap for the next
/// caller.
fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// The `?h=` value to *load* from, with a literal `HEAD` folded away to `None`.
async fn effective_head<'a>(repo: &CachingRepo, head: Option<&'a str>) -> Option<&'a str> {
    let name = head?;
    if name != "HEAD" || has_ref_named(repo, "HEAD").await {
        return Some(name);
    }
    None
}

/// Whether the repository has a branch or a tag whose short name is `name`.
async fn has_ref_named(repo: &CachingRepo, name: &str) -> bool {
    let Ok(refs) = repo.all_refs().await else {
        return false;
    };
    ["heads", "tags"]
        .iter()
        .any(|dir| refs.contains_key(&RefName::Ref(format!("{dir}/{name}").into_bytes())))
}

/// Resolve a `?h=` value to the commit it names: a tag, a branch, or a commit
/// hash, whole or abbreviated to at least four characters.
///
/// Refs are consulted first, and cost nothing to consult — the ref snapshot is
/// fetched once per session and this is a lookup in it — so every link the app
/// writes for itself resolves without a request of its own. Only a value naming
/// no ref is read as an object id, through the same [`resolve_sha`] that
/// `#!/commit/<sha>` uses: a full hash decodes with no I/O, and an abbreviation
/// is expanded against the commit-graph and the pack indexes, reporting an
/// ambiguous prefix rather than picking between the objects that share it.
///
/// The ref-before-hash order is the reverse of `git rev-parse`, which reads a
/// full hash as an object before it consults refs. It matters only for a ref
/// whose own name is hash-shaped, and this way every `?h=` URL that resolved
/// before still resolves to exactly what it did.
async fn resolve_revision(
    repo: &CachingRepo,
    name: &str,
) -> anyhow::Result<(gib::object::Commit, RefKind)> {
    let refs = repo.all_refs().await.context("list refs")?;
    let tags_ref = RefName::Ref(format!("tags/{name}").into_bytes());
    if let Some(entry) = refs.get(&tags_ref)
        && let Some(commit) = commit_for_entry(entry, repo).await
    {
        return Ok((commit, RefKind::Tag));
    }
    let heads_ref = RefName::Ref(format!("heads/{name}").into_bytes());
    if let Some(entry) = refs.get(&heads_ref) {
        let commit = commit_for_entry(entry, repo)
            .await
            .ok_or_else(|| anyhow::anyhow!("ref {name} does not point to a commit"))?;
        return Ok((commit, RefKind::Branch));
    }

    // A value that isn't hash-shaped at all is a misspelled ref far more often
    // than a broken hash, so it gets the error naming everything `?h=` accepts
    // rather than `resolve_sha`'s "invalid SHA".
    if ObjectIdPrefix::from_hex(name.as_bytes()).is_none() {
        anyhow::bail!("not a branch, a tag, HEAD, or a commit hash: {name}");
    }
    let oid = resolve_sha(repo, name).await?;
    let object = repo
        .lookup_object(oid)
        .await
        .context(format!("lookup {name}"))?;
    // A hash may name an annotated tag object as readily as a commit — the tag
    // pages link to tags by name, but a hash copied out of the refs listing is
    // whatever that row pointed at — so peel before deciding it isn't a commit.
    let commit = repo
        .peel_to_commit(&object)
        .await
        .context(format!("peel {name}"))?
        .ok_or_else(|| anyhow::anyhow!("{name} is not a commit"))?;
    Ok((commit, RefKind::Commit))
}

/// The label + kind shown in the path bar / log header: the explicit `?h=`
/// revision, or the implicit HEAD branch. `None` if it can't be resolved — the
/// content view reports the real error.
///
/// A ref is labelled by the name that was asked for. A commit is labelled by the
/// short hash of what it resolved *to*, which is the same thing for a hash that
/// named a commit and the more useful one for a hash that named a tag object.
/// The URL keeps all 40 characters either way: they are what makes the link
/// stable, but spelled out in the path bar they crowd out the breadcrumb.
pub(crate) async fn resolve_display_head(
    repo: &CachingRepo,
    head: Option<&str>,
) -> Option<(String, RefKind)> {
    match effective_head(repo, head).await {
        Some(name) => {
            let (commit, kind) = resolve_revision(repo, name).await.ok()?;
            let label = match kind {
                RefKind::Tag | RefKind::Branch => name.to_string(),
                RefKind::Commit => short_hash(&format!("{}", commit.id())),
            };
            Some((label, kind))
        }
        None => head_branch_name(repo).await.map(|n| (n, RefKind::Branch)),
    }
}

/// A route's resolved content, ready to render. The chrome (nav, path bar) is
/// handled separately by `RouteView`/`NavBar` in `lib.rs`.
pub(crate) enum LoadedView {
    About(AboutProps),
    Readme(ReadmeProps),
    Summary(SummaryProps),
    Log(LogProps),

    Commit(Box<CommitProps>),
    RefsHeads(RefsHeadsProps),
    RefsTags(RefsTagsProps),
    RefsAll(RefsAllProps),
    Tag(TagProps),
    Tree(TreeProps),
    Blob(BlobProps),
    Blame(Box<BlameProps>),
    Snapshot(SnapshotProps),
    /// A tree path that resolved to neither a subtree nor a blob.
    NotFound(String),
}

/// Resolve `hash` into the props for the view it names. Errors (bad refs,
/// missing objects) propagate so `RouteView` can show them in the content area.
pub(crate) async fn build_route(
    hash: &str,
    bundle: &RepoBundle,
    on_partial: &dyn Fn(LoadedView),
) -> anyhow::Result<LoadedView> {
    let repo = &bundle.repo;
    let head_commit: &gib::object::Commit = &bundle.head_commit;
    let root_tree: &Tree = &bundle.root_tree;
    let mailmap: &Mailmap = &bundle.mailmap;
    let clone_url = &bundle.clone_url;
    let repo_name = &bundle.repo_name;

    match parse_hash(hash) {
        Route::About => Ok(LoadedView::About(build_about(repo, clone_url).await)),
        // The README always comes from HEAD's tree, never a `?h=` ref.
        Route::Readme => Ok(LoadedView::Readme(build_readme(root_tree, repo).await)),
        Route::Summary => Ok(LoadedView::Summary(
            build_summary(head_commit, repo, mailmap, clone_url, repo_name, |p| {
                on_partial(LoadedView::Summary(p))
            })
            .await,
        )),
        Route::Log {
            offset,
            head,
            path,
            showmsg,
        } => {
            let resolved;
            let log_commit: &gib::object::Commit = match effective_head(repo, head.as_deref()).await
            {
                Some(name) => {
                    resolved = resolve_revision(repo, name).await?.0;
                    &resolved
                }
                None => head_commit,
            };
            let query = LogQuery {
                path: &path,
                offset,
                head: head.as_deref(),
                showmsg,
            };
            Ok(LoadedView::Log(
                build_log(log_commit, repo, mailmap, &query, |p| {
                    on_partial(LoadedView::Log(p))
                })
                .await,
            ))
        }
        // `#!/commit` with no id follows HEAD, so the diff controls have to
        // rebuild it without an id too — hence the empty `url_sha`, which is
        // what [`commit_url`] turns back into the id-less form.
        Route::CommitHead(view) => Ok(LoadedView::Commit(Box::new(
            build_commit(
                repo,
                mailmap,
                &format!("{}", head_commit.id()),
                "",
                view,
                |p| on_partial(LoadedView::Commit(Box::new(p))),
            )
            .await?,
        ))),
        Route::Commit(sha, view) => Ok(LoadedView::Commit(Box::new(
            build_commit(repo, mailmap, &sha, &sha, view, |p| {
                on_partial(LoadedView::Commit(Box::new(p)))
            })
            .await?,
        ))),
        Route::Refs(RefsRoute::Heads) => {
            Ok(LoadedView::RefsHeads(build_refs_heads(repo, mailmap).await))
        }
        Route::Refs(RefsRoute::Tags) => Ok(LoadedView::RefsTags(
            build_refs_tags(repo, mailmap, repo_name).await,
        )),
        Route::Refs(RefsRoute::All) => Ok(LoadedView::RefsAll(
            build_refs_all(repo, mailmap, repo_name).await,
        )),
        Route::Refs(RefsRoute::Tag(tag)) => Ok(LoadedView::Tag(
            build_tag(repo, mailmap, tag, repo_name).await?,
        )),
        Route::Tree { path, head, render } => {
            let resolved_tree;
            let tree: &Tree = if let Some(ref_name) = effective_head(repo, head.as_deref()).await {
                let (commit, _kind) = resolve_revision(repo, ref_name).await?;
                resolved_tree = repo
                    .lookup_object(commit.tree())
                    .await
                    .context(format!("lookup tree for {ref_name}"))?
                    .tree()
                    .map_err(gib::error::Error::from)
                    .context(format!("expected tree for {ref_name}"))?;
                &resolved_tree
            } else {
                root_tree
            };

            if let Some(subtree) = walk_to_tree(tree, &path, repo).await {
                Ok(LoadedView::Tree(
                    build_tree_props(&subtree, &path, head.as_deref(), repo, |p| {
                        on_partial(LoadedView::Tree(p))
                    })
                    .await,
                ))
            } else if let Some((id, data)) = walk_to_blob(tree, &path, repo).await {
                Ok(LoadedView::Blob(build_blob_props(
                    id,
                    &path,
                    data,
                    head.as_deref(),
                    render,
                )))
            } else {
                Ok(LoadedView::NotFound(path))
            }
        }
        Route::Blame { path, head } => {
            // Unlike the tree route this needs the commit itself, not just its
            // tree: the walk starts from it.
            let resolved;
            let commit: &gib::object::Commit = match effective_head(repo, head.as_deref()).await {
                Some(name) => {
                    resolved = resolve_revision(repo, name).await?.0;
                    &resolved
                }
                None => head_commit,
            };
            let tree = repo
                .lookup_object(commit.tree())
                .await
                .context("lookup tree to blame")?
                .tree()
                .map_err(gib::error::Error::from)
                .context("expected a tree to blame")?;
            // Resolving the blob here rather than in the engine is what lets
            // the file paint before the walk starts, and it decides "not a
            // file" the same way every other path-shaped route does.
            let Some((id, data)) = walk_to_blob(&tree, &path, repo).await else {
                return Ok(LoadedView::NotFound(path));
            };
            let props = build_blame(repo, commit, &path, head.as_deref(), id, data, |p| {
                on_partial(LoadedView::Blame(Box::new(p)))
            })
            .await
            .map_err(|e| anyhow::anyhow!("blame {path}: {e}"))?;
            Ok(LoadedView::Blame(Box::new(props)))
        }
        Route::Snapshot { head } => {
            let head = effective_head(repo, head.as_deref()).await;

            // Both the commit and its tree, where the tree route needs only the
            // tree: the commit's id and date go into the archive itself.
            let resolved_commit;
            let mut resolved_kind = None;
            let commit: &gib::object::Commit = match head {
                Some(name) => {
                    let (commit, kind) = resolve_revision(repo, name).await?;
                    resolved_commit = commit;
                    resolved_kind = Some(kind);
                    &resolved_commit
                }
                None => head_commit,
            };
            let resolved_tree;
            let tree: &Tree = if head.is_some() {
                resolved_tree = repo
                    .lookup_object(commit.tree())
                    .await
                    .context("lookup tree to archive")?
                    .tree()
                    .map_err(gib::error::Error::from)
                    .context("expected a tree to archive")?;
                &resolved_tree
            } else {
                root_tree
            };

            // What the archive is named after: the ref asked for, the branch
            // HEAD is on, or — for a `?h=` that named a commit outright, and for
            // a detached HEAD — the commit itself, abbreviated. All 40 digits in
            // a filename tell the reader nothing the first eight don't.
            let ref_label = match (head, resolved_kind) {
                (Some(_), Some(RefKind::Commit)) => short_hash(&format!("{}", commit.id())),
                (Some(name), _) => name.to_string(),
                (None, _) => match head_branch_name(repo).await {
                    Some(name) => name,
                    None => short_hash(&format!("{}", commit.id())),
                },
            };
            Ok(LoadedView::Snapshot(
                build_snapshot(repo, tree, commit, &ref_label, repo_name, &|p| {
                    on_partial(LoadedView::Snapshot(p))
                })
                .await?,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_hash() {
        assert_eq!(
            short_hash("6121d0b97779278fcc32cc8a02754e7c588d9c18"),
            "6121d0b9"
        );
        // Shorter than the abbreviation: itself, not a panic.
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }
}
