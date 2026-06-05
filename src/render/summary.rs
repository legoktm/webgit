use crate::cache::CachingRepo;
use git_async::object::Commit;
use git_async::reference::RefName;
use serde::Serialize;
use std::collections::BinaryHeap;
use tera::{Context, Tera};

const SUMMARY_TEMPLATE: &str = include_str!("../templates/summary.html");

#[derive(Serialize)]
struct RefRow {
    name: String,
    short_hash: String,
    message: String,
    author: String,
    age: String,
}

#[derive(Serialize)]
struct CommitRow {
    short_hash: String,
    message: String,
    author: String,
    age: String,
}

fn age_string(dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp_millis() as f64;
    let secs = ((now_ms - then_ms) / 1000.0).max(0.0) as u64;
    match secs {
        s if s < 90 => format!("{} seconds", s),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 36 * 3600 => format!("{} hours", s / 3600),
        s if s < 14 * 86400 => format!("{} days", s / 86400),
        s if s < 8 * 7 * 86400 => format!("{} weeks", s / (7 * 86400)),
        s if s < 24 * 30 * 86400 => format!("{} months", s / (30 * 86400)),
        s => format!("{} years", s / (365 * 86400)),
    }
}

fn commit_first_line(c: &Commit) -> String {
    String::from_utf8_lossy(c.message())
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    let hash = format!("{}", c.id());
    RefRow {
        name,
        short_hash: hash[..8].to_string(),
        message: commit_first_line(c),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: age_string(&c.author_date()),
    }
}

async fn build_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
) -> (Vec<RefRow>, Vec<RefRow>, Vec<CommitRow>) {
    let ref_names = repo.ref_names().await.unwrap_or_default();

    // --- Collect and select branch names before fetching any commits ---
    // Primary branch (main/master) goes first; remaining are alpha-sorted.
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
            if short == "main" || short == "master" {
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
    other_branches.truncate(others_limit);
    let branch_names: Vec<String> = primary.into_iter().chain(other_branches).collect();

    // Tags: reverse alpha, cap at 10.
    tag_names.sort_by(|a, b| b.cmp(a));
    tag_names.truncate(10);

    // --- Fetch commit data only for the selected refs ---
    let mut branches: Vec<RefRow> = Vec::new();
    for short in &branch_names {
        let rn = RefName::Ref(format!("heads/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await else {
            continue;
        };
        branches.push(ref_row(short.clone(), &commit));
    }

    let mut tags: Vec<RefRow> = Vec::new();
    for short in &tag_names {
        let rn = RefName::Ref(format!("tags/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await else {
            continue;
        };
        tags.push(ref_row(short.clone(), &commit));
    }

    // --- Walk 10 commits from HEAD via full DAG, ordered by commit date ---
    let mut heap: BinaryHeap<(chrono::DateTime<chrono::FixedOffset>, Commit)> = BinaryHeap::new();
    heap.push((head_commit.commit_date(), head_commit.clone()));

    let mut commits: Vec<CommitRow> = Vec::new();
    while commits.len() < 10 {
        let (_, current) = match heap.pop() {
            Some(e) => e,
            None => break,
        };
        let hash = format!("{}", current.id());
        commits.push(CommitRow {
            short_hash: hash[..8].to_string(),
            message: commit_first_line(&current),
            author: String::from_utf8_lossy(current.author_name()).into_owned(),
            age: age_string(&current.author_date()),
        });
        let parents = match repo.lookup_parents(&current).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        for parent in parents {
            heap.push((parent.commit_date(), parent));
        }
    }

    (branches, tags, commits)
}

pub(crate) async fn render_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) {
    let (branches, tags, commits) = build_summary(head_commit, repo).await;
    let mut ctx = Context::new();
    ctx.insert("branches", &branches);
    ctx.insert("tags", &tags);
    ctx.insert("commits", &commits);
    ctx.insert("clone_url", clone_url);
    match Tera::one_off(SUMMARY_TEMPLATE, &ctx, true) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}
