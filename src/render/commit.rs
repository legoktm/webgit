use crate::cache::CachingRepo;
use crate::error::GitContext;
use git_async::diff::{DiffEntry, TreeDiff};
use git_async::error::Error as GitError;
use git_async::object::{Object, ObjectId};
use serde::Serialize;
use similar::TextDiffConfig;
use tera::{Context, Tera};

#[derive(Serialize)]
struct ParentRef {
    hash: String,
    short: String,
}

#[derive(Serialize)]
struct DiffLine {
    kind: String,
    content: String,
}

#[derive(Serialize)]
struct FileDiff {
    path: String,
    additions: usize,
    deletions: usize,
    bar_add: usize,
    bar_del: usize,
}

#[derive(Serialize)]
struct CommitTemplate {
    hash: String,
    short_hash: String,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    parents: Vec<ParentRef>,
    tree_hash: String,
    message: String,
    total_additions: usize,
    total_deletions: usize,
    files: Vec<FileDiff>,
    diff_lines: Vec<DiffLine>,
}

async fn build_commit(repo: &CachingRepo, sha: &str) -> anyhow::Result<CommitTemplate> {
    let oid = ObjectId::from_hex(sha.as_bytes())
        .ok_or_else(|| anyhow::anyhow!("invalid SHA: {sha}"))?;
    let commit = repo
        .lookup_object(oid).await
        .context(format!("lookup {sha}"))?
        .commit()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let parent_commits = repo.lookup_parents(&commit).await.unwrap_or_default();

    let parents: Vec<ParentRef> = parent_commits
        .iter()
        .map(|p| {
            let h = format!("{}", p.id());
            ParentRef { short: h[..8].to_string(), hash: h }
        })
        .collect();

    let commit_tree = repo
        .lookup_object(commit.tree()).await
        .context("lookup commit tree")?
        .tree()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let (files, diff_lines) = if let Some(parent) = parent_commits.first() {
        let parent_tree = repo
            .lookup_object(parent.tree()).await
            .context("lookup parent tree")?
            .tree()
            .map_err(GitError::from)
        .context("unexpected object type")?;
        let td = repo
            .tree_diff(&parent_tree, &commit_tree).await
            .context("tree diff")?;
        build_diff(repo, &td).await
    } else {
        (Vec::new(), Vec::new())
    };

    let total_additions: usize = files.iter().map(|f| f.additions).sum();
    let total_deletions: usize = files.iter().map(|f| f.deletions).sum();

    let hash = format!("{}", commit.id());
    Ok(CommitTemplate {
        short_hash: hash[..8].to_string(),
        hash,
        author_name: String::from_utf8_lossy(commit.author_name()).into_owned(),
        author_email: String::from_utf8_lossy(commit.author_email()).into_owned(),
        author_date: commit.author_date().to_string(),
        committer_name: String::from_utf8_lossy(commit.committer_name()).into_owned(),
        committer_email: String::from_utf8_lossy(commit.committer_email()).into_owned(),
        committer_date: commit.commit_date().to_string(),
        parents,
        tree_hash: format!("{}", commit.tree()),
        message: String::from_utf8_lossy(commit.message()).into_owned(),
        total_additions,
        total_deletions,
        files,
        diff_lines,
    })
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

async fn build_diff(repo: &CachingRepo, td: &TreeDiff) -> (Vec<FileDiff>, Vec<DiffLine>) {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut diff_lines: Vec<DiffLine> = Vec::new();

    for entry in td.entries() {
        let path = String::from_utf8_lossy(entry.path().as_slice()).into_owned();

        let (old_data, new_data) = match entry {
            DiffEntry::LeftOnly { content: (old_id, _), .. } => {
                (load_blob(repo, *old_id).await, Vec::new())
            }
            DiffEntry::RightOnly { content: (_, new_id), .. } => {
                (Vec::new(), load_blob(repo, *new_id).await)
            }
            DiffEntry::Both { content: (old_id, new_id), .. } => {
                (load_blob(repo, *old_id).await, load_blob(repo, *new_id).await)
            }
        };

        let text_diff = TextDiffConfig::default().diff_lines(old_data, new_data);
        let udiff = text_diff
            .unified_diff()
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string();

        diff_lines.push(DiffLine {
            kind: "hunk".to_string(),
            content: format!("diff --git a/{path} b/{path}"),
        });

        let mut additions = 0usize;
        let mut deletions = 0usize;

        for line in udiff.lines() {
            let lkind = match line.chars().next() {
                Some('+') => {
                    if !line.starts_with("+++") { additions += 1; }
                    "add"
                }
                Some('-') => {
                    if !line.starts_with("---") { deletions += 1; }
                    "del"
                }
                Some('@') => "hunk",
                _ => "ctx",
            };
            diff_lines.push(DiffLine { kind: lkind.to_string(), content: line.to_string() });
        }

        files.push(FileDiff { path, additions, deletions, bar_add: 0, bar_del: 0 });
    }

    let max_changes = files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(1)
        .max(1);

    for f in &mut files {
        let total = f.additions + f.deletions;
        let bar_total = total * 40 / max_changes;
        f.bar_add = f.additions.checked_mul(bar_total).and_then(|n| n.checked_div(total)).unwrap_or(0);
        f.bar_del = bar_total - f.bar_add;
    }

    (files, diff_lines)
}

pub(crate) async fn render_commit(
    tera: &Tera,
    repo: &CachingRepo,
    sha: String,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_commit(repo, &sha).await?;
    let ctx = Context::from_serialize(&template)?;
    let html = tera.render("commit.html", &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}
