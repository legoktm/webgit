use crate::{
    cache::CachingRepo,
    render::{CommitRow, age, commit_first_line, render_template},
    route::log_url,
};
use git_async::object::Commit;
use git_async::object::ObjectId;
use serde::Serialize;
use std::collections::{BTreeSet, BinaryHeap};
use tera::Tera;

const PAGE_SIZE: usize = 50;

async fn build_log(
    head_commit: &Commit,
    repo: &CachingRepo,
    offset: usize,
    head: Option<&str>,
) -> LogTemplate {
    let mut heap: BinaryHeap<(chrono::DateTime<chrono::FixedOffset>, Commit)> = BinaryHeap::new();
    let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
    heap.push((head_commit.commit_date(), head_commit.clone()));
    visited.insert(head_commit.id());

    let mut count = 0usize;
    let mut commits: Vec<CommitRow> = Vec::new();
    let mut has_next = false;

    while let Some((_, current)) = heap.pop() {
        if count >= offset && commits.len() < PAGE_SIZE {
            let hash = format!("{}", current.id());
            commits.push(CommitRow {
                short_hash: hash[..8].to_string(),
                hash,
                message: commit_first_line(&current),
                author: String::from_utf8_lossy(current.author_name()).into_owned(),
                age: age(&current.author_date()),
            });
        } else if commits.len() == PAGE_SIZE {
            has_next = true;
            break;
        }
        count += 1;

        let parents = match repo.lookup_parents(&current).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        for parent in parents {
            if visited.insert(parent.id()) {
                heap.push((parent.commit_date(), parent));
            }
        }
    }

    LogTemplate {
        commits,
        prev_url: (offset > 0)
            .then(|| log_url(offset.saturating_sub(PAGE_SIZE), head)),
        next_url: has_next.then(|| log_url(offset + PAGE_SIZE, head)),
    }
}

#[derive(Serialize)]
struct LogTemplate {
    commits: Vec<CommitRow>,
    prev_url: Option<String>,
    next_url: Option<String>,
}

pub(crate) async fn render_log(
    tera: &Tera,
    head_commit: &Commit,
    repo: &CachingRepo,
    offset: usize,
    head: Option<&str>,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_log(head_commit, repo, offset, head).await;
    render_template(tera, "log.html", &template, output)
}
