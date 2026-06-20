use crate::error::GitContext;
use crate::{cache::CachingRepo, render::render_template};
use git_async::diff::{DiffEntry, TreeDiff};
use git_async::error::Error as GitError;
use git_async::object::{Object, ObjectId};
use serde::Serialize;
use similar::TextDiffConfig;
use tera::Tera;

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

    let (files, diff_lines) = if let Some(parent) = parent_commits.first() {
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

async fn build_diff(repo: &CachingRepo, td: &TreeDiff) -> (Vec<FileDiff>, Vec<DiffLine>) {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut diff_lines: Vec<DiffLine> = Vec::new();

    // Phase 1: load every changed file's blobs concurrently, so the per-object
    // IndexedDB/network round-trips overlap across files instead of being
    // serialised one file at a time. The diffing itself (phase 2) is CPU-bound
    // and stays sequential to preserve output order.
    let loaded = futures::future::join_all(td.entries().iter().map(|entry| async move {
        let path = String::from_utf8_lossy(entry.path().as_slice()).into_owned();
        let (old_data, new_data) = match entry {
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
        };
        (path, old_data, new_data)
    }))
    .await;

    for (path, old_data, new_data) in loaded {
        diff_lines.push(DiffLine {
            kind: "hunk".to_string(),
            content: format!("diff --git a/{path} b/{path}"),
        });

        // Git treats a blob as binary if it contains a NUL byte; in that case
        // it shows "Binary files differ" rather than a line-by-line diff, which
        // would be meaningless (and potentially huge). Mirror that here.
        if is_binary(&old_data) || is_binary(&new_data) {
            diff_lines.push(DiffLine {
                kind: "ctx".to_string(),
                content: format!("Binary files a/{path} and b/{path} differ"),
            });
            files.push(FileDiff {
                path,
                additions: 0,
                deletions: 0,
                bar_add: 0,
                bar_del: 0,
            });
            continue;
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
            diff_lines.push(DiffLine {
                kind: lkind.to_string(),
                content: line.to_string(),
            });
        }

        files.push(FileDiff {
            path,
            additions,
            deletions,
            bar_add: 0,
            bar_del: 0,
        });
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
        f.bar_add = f
            .additions
            .checked_mul(bar_total)
            .and_then(|n| n.checked_div(total))
            .unwrap_or(0);
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
    render_template(tera, "commit.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{init_tera, render_to_string};

    fn base_fixture() -> CommitTemplate {
        CommitTemplate {
            hash: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            short_hash: "0123abcd".to_string(),
            author_name: "Kunal Mehta".to_string(),
            author_email: "author@example.org".to_string(),
            author_date: "2026-01-15 12:34:56 +00:00".to_string(),
            committer_name: "Committer Person".to_string(),
            committer_email: "committer@example.org".to_string(),
            committer_date: "2026-01-15 13:00:00 +00:00".to_string(),
            parents: vec![],
            tree_hash: "fedcba98fedcba98fedcba98fedcba98fedcba98".to_string(),
            message: "Fix the thing\n\nLonger explanation with <html> & \"chars\".".to_string(),
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
    fn test_commit_html_root_commit() {
        // No parents and no diff: the diffstat section should be absent.
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "commit.html", &base_fixture()).unwrap()
        );
    }

    #[test]
    fn test_commit_html_merge_with_diff() {
        let mut template = base_fixture();
        template.parents = vec![
            ParentRef {
                hash: "1111111111111111111111111111111111111111".to_string(),
                short: "11111111".to_string(),
            },
            ParentRef {
                hash: "2222222222222222222222222222222222222222".to_string(),
                short: "22222222".to_string(),
            },
        ];
        template.files = vec![
            FileDiff {
                path: "src/main.rs".to_string(),
                additions: 3,
                deletions: 1,
                bar_add: 30,
                bar_del: 10,
            },
            FileDiff {
                path: "README".to_string(),
                additions: 1,
                deletions: 0,
                bar_add: 10,
                bar_del: 0,
            },
        ];
        template.total_additions = 4;
        template.total_deletions = 1;
        template.diff_lines = vec![
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
        insta::assert_snapshot!(render_to_string(&init_tera(), "commit.html", &template).unwrap());
    }
}
