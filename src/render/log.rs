use crate::{
    cache::CachingRepo,
    render::{CommitRow, age, commit_first_line},
};
use git_async::object::Commit;
use git_async::object::ObjectId;
use serde::Serialize;
use std::collections::{BTreeSet, BinaryHeap};
use tera::{Context, Tera};

const PAGE_SIZE: usize = 50;

async fn build_log(head_commit: &Commit, repo: &CachingRepo, offset: usize) -> LogTemplate {
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
        has_prev: offset > 0,
        prev_offset: offset.saturating_sub(PAGE_SIZE),
        has_next,
        next_offset: offset + PAGE_SIZE,
    }
}

#[derive(Serialize)]
struct LogTemplate {
    commits: Vec<CommitRow>,
    has_prev: bool,
    prev_offset: usize,
    has_next: bool,
    next_offset: usize,
}

pub(crate) async fn render_log(
    tera: &Tera,
    head_commit: &Commit,
    repo: &CachingRepo,
    offset: usize,
    output: &web_sys::Element,
) {
    let template = build_log(head_commit, repo, offset).await;
    let ctx = Context::from_serialize(&template).unwrap();
    match tera.render("log.html", &ctx) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}
