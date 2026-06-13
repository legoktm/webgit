use crate::cache::CachingRepo;
use git_async::object::{Commit, ObjectId};
use git_async::reference::{RefEntry, RefName, RefTarget};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use tera::{Context, Tera};

pub(crate) mod about;
pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod listing;
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
    tera.add_raw_templates(vec![
        ("about.html", include_str!("../templates/about.html")),
        ("blob.html", include_str!("../templates/blob.html")),
        ("listing.html", include_str!("../templates/listing.html")),
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
    age: Age,
}

#[derive(Serialize)]
pub(crate) struct CommitRow {
    hash: String,
    short_hash: String,
    message: String,
    author: String,
    age: Age,
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

/// A commit/ref timestamp that keeps both representations: the elapsed seconds
/// (for sorting by recency and choosing a format) and the absolute timestamp.
/// It sorts by recency and serializes — at render time — to a coarse relative
/// age within the last two weeks, or an absolute `YYYY-MM-DD` date (in the
/// commit's own timezone) beyond that.
#[derive(Clone, Copy)]
pub(crate) struct Age {
    secs: u64,
    when: chrono::DateTime<chrono::FixedOffset>,
}

impl Age {
    fn new(when: &chrono::DateTime<chrono::FixedOffset>) -> Self {
        Self {
            secs: age(when),
            when: *when,
        }
    }

    /// Elapsed seconds, the sort key (smaller is more recent).
    pub(crate) fn secs(&self) -> u64 {
        self.secs
    }
}

impl serde::Serialize for Age {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_age(self.secs, &self.when))
    }
}

/// The display rule, split out as a pure function so the bucket boundaries can
/// be tested without depending on the wall clock.
fn format_age(secs: u64, dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    match secs {
        s if s < 90 => format!("{s} seconds"),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 36 * 3600 => format!("{} hours", s / 3600),
        s if s < 14 * 86400 => format!("{} days", s / 86400),
        _ => dt.format("%Y-%m-%d").to_string(),
    }
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
    // Ids whose objects we've already issued a (concurrent) prefetch for, so we
    // don't re-request them on later iterations.
    let mut prefetched: BTreeSet<ObjectId> = BTreeSet::new();

    while !heap.is_empty() {
        // Look-ahead: concurrently fetch the parent objects of every commit
        // currently on the frontier, warming the cache so the `lookup_parents`
        // calls below resolve without further round-trips. This only populates
        // the cache — emission order is still driven solely by the heap, so the
        // output is identical to a purely sequential walk. A linear history has
        // a frontier of one and sees no benefit (it is inherently sequential);
        // merges and parallel branches are fetched as wide as the frontier.
        if commits.len() < limit {
            let to_prefetch: Vec<ObjectId> = heap
                .iter()
                .flat_map(|(_, commit)| commit.parents().iter().copied())
                .filter(|id| !visited.contains(id) && prefetched.insert(*id))
                .collect();
            if !to_prefetch.is_empty() {
                futures::future::join_all(to_prefetch.iter().map(|id| repo.lookup_object(*id)))
                    .await;
            }
        }

        let (_, current) = heap.pop().unwrap();
        if count >= skip && commits.len() < limit {
            let hash = format!("{}", current.id());
            commits.push(CommitRow {
                short_hash: hash[..8].to_string(),
                hash,
                message: commit_first_line(current.message()),
                author: String::from_utf8_lossy(current.author_name()).into_owned(),
                age: Age::new(&current.author_date()),
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
        age: Age::new(&c.author_date()),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::{Age, CommitRow, RefLabel, RefLabelKind, RefRow};

    /// An [`Age`] that renders as a relative bucket; `secs` must be under the
    /// two-week cutoff for the (placeholder) date to stay hidden.
    pub(crate) fn relative_age(secs: u64) -> Age {
        Age {
            secs,
            when: ymd("2000-01-01"),
        }
    }

    /// An [`Age`] old enough to render as the given absolute `YYYY-MM-DD` date.
    pub(crate) fn date_age(date: &str) -> Age {
        Age {
            secs: 365 * 86400,
            when: ymd(date),
        }
    }

    fn ymd(date: &str) -> chrono::DateTime<chrono::FixedOffset> {
        use chrono::TimeZone;
        let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        chrono::FixedOffset::east_opt(0)
            .unwrap()
            .from_local_datetime(&naive)
            .unwrap()
    }

    pub(crate) fn ref_row(name: &str, message: &str, author: &str, age: Age) -> RefRow {
        RefRow {
            name: name.to_string(),
            short_hash: "0123abcd".to_string(),
            message: message.to_string(),
            author: author.to_string(),
            age,
        }
    }

    pub(crate) fn commit_row(short_hash: &str, message: &str, author: &str, age: Age) -> CommitRow {
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
        age: Age,
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

    fn fixed_dt() -> chrono::DateTime<chrono::FixedOffset> {
        use chrono::TimeZone;
        chrono::FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2001, 2, 3, 4, 5, 6)
            .unwrap()
    }

    #[test]
    fn test_format_age_relative_buckets() {
        let dt = fixed_dt();
        assert_eq!(format_age(0, &dt), "0 seconds");
        assert_eq!(format_age(89, &dt), "89 seconds");
        assert_eq!(format_age(90, &dt), "1 minutes");
        assert_eq!(format_age(89 * 60, &dt), "89 minutes");
        assert_eq!(format_age(90 * 60, &dt), "1 hours");
        assert_eq!(format_age(35 * 3600, &dt), "35 hours");
        assert_eq!(format_age(36 * 3600, &dt), "1 days");
        assert_eq!(format_age(13 * 86400, &dt), "13 days");
    }

    #[test]
    fn test_format_age_two_weeks_and_older_is_date() {
        let dt = fixed_dt();
        // From exactly two weeks on, show the commit's own date instead.
        assert_eq!(format_age(14 * 86400, &dt), "2001-02-03");
        assert_eq!(format_age(86400 * 400, &dt), "2001-02-03");
    }

    #[test]
    fn age_sorts_by_recency_regardless_of_display() {
        let dt = fixed_dt();
        // A mix of relative-rendered and date-rendered ages; sorting must order
        // them by elapsed seconds (most recent first), not by the display text.
        let mut ages = [
            Age {
                secs: 86400 * 400,
                when: dt,
            },
            Age { secs: 60, when: dt },
            Age {
                secs: 3600,
                when: dt,
            },
        ];
        ages.sort_by_key(Age::secs);
        assert_eq!(
            ages.map(|a| a.secs()),
            [60, 3600, 86400 * 400],
            "expected ascending recency order"
        );
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
