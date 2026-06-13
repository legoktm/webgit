use crate::cache::CachingRepo;
use git_async::object::{Commit, ObjectId};
use git_async::reference::{RefEntry, RefName, RefTarget};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
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

pub(crate) fn render_to_string(
    tera: &Tera,
    name: &str,
    data: &impl Serialize,
) -> anyhow::Result<String> {
    let ctx = Context::from_serialize(data)?;
    Ok(tera.render(name, &ctx)?)
}

pub(crate) fn render_template(
    tera: &Tera,
    name: &str,
    data: &impl Serialize,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    output.set_inner_html(&render_to_string(tera, name, data)?);
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
    refs: Vec<RefLabel>,
}

/// A branch or tag decoration shown next to a commit, cgit-style.
#[derive(Serialize, Clone)]
pub(crate) struct RefLabel {
    name: String,
    kind: RefLabelKind,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RefLabelKind {
    Branch,
    Tag,
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

fn commit_first_line(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

pub(crate) async fn head_branch_name(repo: &CachingRepo) -> Option<String> {
    let head = repo.head().await.ok()?;
    if let RefTarget::Symbolic(RefName::Ref(name)) = head.target() {
        let branch = name.strip_prefix(b"heads/")?;
        Some(String::from_utf8_lossy(branch).into_owned())
    } else {
        None
    }
}

/// Split the session's ref snapshot into short-named branches and tags.
pub(crate) async fn collect_refs(
    repo: &CachingRepo,
) -> (Vec<(String, RefEntry)>, Vec<(String, RefEntry)>) {
    let Ok(all_refs) = repo.all_refs().await else {
        return (Vec::new(), Vec::new());
    };
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for (ref_name, entry) in all_refs.iter() {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            branches.push((short.to_string(), *entry));
        } else if let Some(short) = label.strip_prefix("tags/") {
            tags.push((short.to_string(), *entry));
        }
    }
    (branches, tags)
}

/// Resolve a ref entry to the commit it points at, fetching the tag object
/// only when no peeled OID was recorded.
pub(crate) async fn commit_for_entry(entry: &RefEntry, repo: &CachingRepo) -> Option<Commit> {
    let obj = repo.lookup_object(entry.commit_target()).await.ok()?;
    repo.peel_to_commit(&obj).await.ok().flatten()
}

pub(crate) async fn fetch_ref_rows(refs: &[(String, RefEntry)], repo: &CachingRepo) -> Vec<RefRow> {
    futures::future::join_all(refs.iter().map(|(short, entry)| {
        let short = short.clone();
        async move {
            let commit = commit_for_entry(entry, repo).await?;
            Some(ref_row(short, &commit))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Map each decorated commit to its branch/tag labels, for cgit-style
/// decorations in commit lists.
pub(crate) async fn decoration_map(repo: &CachingRepo) -> BTreeMap<ObjectId, Vec<RefLabel>> {
    let (branches, tags) = collect_refs(repo).await;
    let mut map: BTreeMap<ObjectId, Vec<RefLabel>> = BTreeMap::new();
    for (name, entry) in branches {
        map.entry(entry.commit_target())
            .or_default()
            .push(RefLabel {
                name,
                kind: RefLabelKind::Branch,
            });
    }
    let tag_oids = futures::future::join_all(tags.into_iter().map(|(name, entry)| async move {
        // Without a recorded peeled OID this costs one (cached) object
        // lookup per tag.
        let oid = match entry.peeled() {
            Some(oid) => oid,
            None => commit_for_entry(&entry, repo).await?.id(),
        };
        Some((name, oid))
    }))
    .await;
    for (name, oid) in tag_oids.into_iter().flatten() {
        map.entry(oid).or_default().push(RefLabel {
            name,
            kind: RefLabelKind::Tag,
        });
    }
    map
}

pub(crate) async fn walk_commits(
    head_commit: &Commit,
    repo: &CachingRepo,
    skip: usize,
    limit: usize,
    decorations: &BTreeMap<ObjectId, Vec<RefLabel>>,
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
                message: commit_first_line(current.message()),
                author: String::from_utf8_lossy(current.author_name()).into_owned(),
                age: age(&current.author_date()),
                refs: decorations.get(&current.id()).cloned().unwrap_or_default(),
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
        message: commit_first_line(c.message()),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: age(&c.author_date()),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{CommitRow, RefLabel, RefLabelKind, RefRow};

    pub(crate) fn ref_row(name: &str, message: &str, author: &str, age: u64) -> RefRow {
        RefRow {
            name: name.to_string(),
            short_hash: "0123abcd".to_string(),
            message: message.to_string(),
            author: author.to_string(),
            age,
        }
    }

    pub(crate) fn commit_row(short_hash: &str, message: &str, author: &str, age: u64) -> CommitRow {
        CommitRow {
            hash: format!("{short_hash}{}", "0".repeat(40 - short_hash.len())),
            short_hash: short_hash.to_string(),
            message: message.to_string(),
            author: author.to_string(),
            age,
            refs: Vec::new(),
        }
    }

    pub(crate) fn decorated_commit_row(
        short_hash: &str,
        message: &str,
        author: &str,
        age: u64,
        branches: &[&str],
        tags: &[&str],
    ) -> CommitRow {
        let mut row = commit_row(short_hash, message, author, age);
        for name in branches {
            row.refs.push(RefLabel {
                name: name.to_string(),
                kind: RefLabelKind::Branch,
            });
        }
        for name in tags {
            row.refs.push(RefLabel {
                name: name.to_string(),
                kind: RefLabelKind::Tag,
            });
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_age(secs: u64) -> String {
        let mut tera = Tera::default();
        tera.register_filter("age_string", age_string);
        tera.add_raw_template("age.html", "{{ s | age_string }}")
            .unwrap();
        let mut ctx = Context::new();
        ctx.insert("s", &secs);
        tera.render("age.html", &ctx).unwrap()
    }

    #[test]
    fn test_age_string_buckets() {
        assert_eq!(render_age(0), "0 seconds");
        assert_eq!(render_age(89), "89 seconds");
        assert_eq!(render_age(90), "1 minutes");
        assert_eq!(render_age(89 * 60), "89 minutes");
        assert_eq!(render_age(90 * 60), "1 hours");
        assert_eq!(render_age(35 * 3600), "35 hours");
        assert_eq!(render_age(36 * 3600), "1 days");
        assert_eq!(render_age(13 * 86400), "13 days");
        assert_eq!(render_age(14 * 86400), "2 weeks");
        assert_eq!(render_age(8 * 7 * 86400 - 1), "7 weeks");
        assert_eq!(render_age(8 * 7 * 86400), "1 months");
        assert_eq!(render_age(24 * 30 * 86400 - 1), "23 months");
        assert_eq!(render_age(24 * 30 * 86400), "1 years");
        assert_eq!(render_age(3 * 365 * 86400), "3 years");
    }

    #[test]
    fn test_commit_first_line() {
        assert_eq!(
            commit_first_line(b"Fix bug\n\nLonger description\n"),
            "Fix bug"
        );
        assert_eq!(commit_first_line(b"Single line"), "Single line");
        assert_eq!(commit_first_line(b"trailing newline\n"), "trailing newline");
        assert_eq!(commit_first_line(b""), "");
        // Invalid UTF-8 is replaced, not dropped.
        assert_eq!(
            commit_first_line(b"caf\xc3\xa9 \xff fix"),
            "caf\u{e9} \u{fffd} fix"
        );
    }
}
