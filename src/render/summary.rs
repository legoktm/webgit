use crate::{
    cache::CachingRepo,
    render::{CommitRow, RefRow, age, commit_first_line, fetch_branch_rows, fetch_tag_rows},
};
use git_async::object::Commit;
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use std::collections::BinaryHeap;
use tera::{Context, Tera};

async fn fetch_recent_commits(head_commit: &Commit, repo: &CachingRepo) -> Vec<CommitRow> {
    let mut heap: BinaryHeap<(chrono::DateTime<chrono::FixedOffset>, Commit)> = BinaryHeap::new();
    heap.push((head_commit.commit_date(), head_commit.clone()));
    let mut commits = Vec::new();
    while commits.len() < 10 {
        let (_, current) = match heap.pop() {
            Some(e) => e,
            None => break,
        };
        let hash = format!("{}", current.id());
        commits.push(CommitRow {
            short_hash: hash[..8].to_string(),
            hash,
            message: commit_first_line(&current),
            author: String::from_utf8_lossy(current.author_name()).into_owned(),
            age: age(&current.author_date()),
        });
        let parents = match repo.lookup_parents(&current).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        for parent in parents {
            heap.push((parent.commit_date(), parent));
        }
    }
    commits
}

async fn build_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
) -> SummaryTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();

    let head_branch: Option<String> = repo.head().await.ok().and_then(|r| {
        match r.target() {
            RefTarget::Symbolic(RefName::Ref(b)) => b
                .strip_prefix(b"heads/")
                .map(|s| String::from_utf8_lossy(s).into_owned()),
            _ => None,
        }
    });

    // --- Collect and select branch names before fetching any commits ---
    // HEAD branch goes first; remaining are alpha-sorted.
    // Total cap: 1 primary + 9 others = 10.
    let mut primary: Option<String> = None;
    let mut other_branches: Vec<String> = Vec::new();
    let mut tag_names: Vec<String> = Vec::new();

    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b).into_owned(),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            if head_branch.as_deref() == Some(short) {
                primary = Some(short.to_string());
            } else {
                other_branches.push(short.to_string());
            }
        } else if let Some(short) = label.strip_prefix("tags/") {
            tag_names.push(short.to_string());
        }
    }

    other_branches.sort();
    let others_limit = if primary.is_some() { 9 } else { 10 };
    let more_branches = other_branches.len() > others_limit;
    other_branches.truncate(others_limit);
    let branch_names: Vec<String> = primary.into_iter().chain(other_branches).collect();

    let more_tags = tag_names.len() > 10;
    // Tags: reverse alpha, cap at 10.
    tag_names.sort_by(|a, b| b.cmp(a));
    tag_names.truncate(10);

    // --- Fetch branch commits, tag commits, and recent HEAD commits concurrently ---
    let (branches, tags, commits) = futures::join!(
        fetch_branch_rows(&branch_names, repo),
        fetch_tag_rows(&tag_names, repo),
        fetch_recent_commits(head_commit, repo),
    );

    SummaryTemplate {
        branches,
        more_branches,
        tags,
        more_tags,
        commits,
        clone_url: clone_url.to_string(),
    }
}

#[derive(Serialize)]
struct SummaryTemplate {
    branches: Vec<RefRow>,
    more_branches: bool,
    tags: Vec<RefRow>,
    more_tags: bool,
    commits: Vec<CommitRow>,
    clone_url: String,
}

pub(crate) async fn render_summary(
    tera: &Tera,
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_summary(head_commit, repo, clone_url).await;
    let ctx = Context::from_serialize(&template)?;
    let html = tera.render("summary.html", &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}
