use crate::cache::CachingRepo;
use git_async::object::{Commit, ObjectId};
use git_async::reference::RefName;
use serde::Serialize;
use std::collections::{BTreeSet, BinaryHeap};
use tera::{Context, Kwargs, State, Tera, TeraResult, Value};

pub(crate) mod about;
pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod log;
pub(crate) mod refs_all;
pub(crate) mod refs_heads;
pub(crate) mod refs_tags;
pub(crate) mod summary;
pub(crate) mod tag;
pub(crate) mod tree;

pub(crate) fn render_template(
    tera: &Tera,
    name: &str,
    data: &impl Serialize,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let ctx = Context::from_serialize(data)?;
    let html = tera.render(name, &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}

pub(crate) fn init_tera() -> Tera {
    let mut tera = Tera::default();
    tera.register_filter("age_string", age_string);
    tera.add_raw_templates(vec![
        ("about.html", include_str!("../templates/about.html")),
        ("blob.html", include_str!("../templates/blob.html")),
        (
            "refs_heads.html",
            include_str!("../templates/refs_heads.html"),
        ),
        (
            "refs_tags.html",
            include_str!("../templates/refs_tags.html"),
        ),
        ("refs_all.html", include_str!("../templates/refs_all.html")),
        ("summary.html", include_str!("../templates/summary.html")),
        ("tree.html", include_str!("../templates/tree.html")),
        ("tag.html", include_str!("../templates/tag.html")),
        ("commit.html", include_str!("../templates/commit.html")),
        ("commits.html", include_str!("../templates/commits.html")),
        ("log.html", include_str!("../templates/log.html")),
    ])
    .unwrap();
    tera
}

#[derive(Serialize)]
pub(crate) struct RefRow {
    name: String,
    short_hash: String,
    message: String,
    author: String,
    age: u64,
}

#[derive(Serialize)]
pub(crate) struct CommitRow {
    hash: String,
    short_hash: String,
    message: String,
    author: String,
    age: u64,
}

fn age(dt: &chrono::DateTime<chrono::FixedOffset>) -> u64 {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp_millis() as f64;
    ((now_ms - then_ms) / 1000.0).max(0.0) as u64
}

fn age_string(value: Value, _: Kwargs, _: &State) -> TeraResult<Value> {
    let secs = value.as_u128().unwrap() as u64;
    let formatted = match secs {
        s if s < 90 => format!("{} seconds", s),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 36 * 3600 => format!("{} hours", s / 3600),
        s if s < 14 * 86400 => format!("{} days", s / 86400),
        s if s < 8 * 7 * 86400 => format!("{} weeks", s / (7 * 86400)),
        s if s < 24 * 30 * 86400 => format!("{} months", s / (30 * 86400)),
        s => format!("{} years", s / (365 * 86400)),
    };
    Ok(Value::from(formatted))
}

fn commit_first_line(c: &Commit) -> String {
    String::from_utf8_lossy(c.message())
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn collect_ref_names(repo: &CachingRepo) -> (Vec<String>, Vec<String>) {
    let ref_names = repo.ref_names().await.unwrap_or_default();
    let mut branch_names = Vec::new();
    let mut tag_names = Vec::new();
    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            branch_names.push(short.to_string());
        } else if let Some(short) = label.strip_prefix("tags/") {
            tag_names.push(short.to_string());
        }
    }
    (branch_names, tag_names)
}

async fn fetch_ref_rows(prefix: &'static str, names: &[String], repo: &CachingRepo) -> Vec<RefRow> {
    futures::future::join_all(names.iter().map(|short| {
        let short = short.clone();
        async move {
            let rn = RefName::Ref(format!("{prefix}/{short}").into_bytes());
            let Ok(r) = repo.lookup_ref(&rn).await else { return None };
            let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await else { return None };
            Some(ref_row(short, &commit))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

pub(crate) async fn fetch_branch_rows(branch_names: &[String], repo: &CachingRepo) -> Vec<RefRow> {
    fetch_ref_rows("heads", branch_names, repo).await
}

pub(crate) async fn fetch_tag_rows(tag_names: &[String], repo: &CachingRepo) -> Vec<RefRow> {
    fetch_ref_rows("tags", tag_names, repo).await
}

pub(crate) async fn walk_commits(
    head_commit: &Commit,
    repo: &CachingRepo,
    skip: usize,
    limit: usize,
) -> (Vec<CommitRow>, bool) {
    let mut heap: BinaryHeap<(chrono::DateTime<chrono::FixedOffset>, Commit)> = BinaryHeap::new();
    let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
    heap.push((head_commit.commit_date(), head_commit.clone()));
    visited.insert(head_commit.id());

    let mut count = 0usize;
    let mut commits: Vec<CommitRow> = Vec::new();
    let mut has_more = false;

    while let Some((_, current)) = heap.pop() {
        if count >= skip && commits.len() < limit {
            let hash = format!("{}", current.id());
            commits.push(CommitRow {
                short_hash: hash[..8].to_string(),
                hash,
                message: commit_first_line(&current),
                author: String::from_utf8_lossy(current.author_name()).into_owned(),
                age: age(&current.author_date()),
            });
        } else if commits.len() == limit {
            has_more = true;
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

    (commits, has_more)
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    let hash = format!("{}", c.id());
    RefRow {
        name,
        short_hash: hash[..8].to_string(),
        message: commit_first_line(c),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: age(&c.author_date()),
    }
}
