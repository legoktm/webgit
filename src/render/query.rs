//! Reading the repository for a listing: refs into rows, commits into rows,
//! and the streaming walks that let a page paint before either finishes.

use super::commits_table::{CommitRow, RefLabel, RefLabelKind};
use super::refs_table::{RefMeta, RefRow};
use super::short_hash;
use super::time::Age;
use crate::cache::CachingRepo;
use gib::object::{Commit, ObjectId};
use gib::reference::{RefEntry, RefName, RefTarget};
use gib_mailmap::Mailmap;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

fn commit_first_line(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Everything after a commit message's subject line
fn commit_body(message: &[u8]) -> String {
    String::from_utf8_lossy(message)
        .split_once('\n')
        .map_or(String::new(), |(_, body)| {
            body.trim_start_matches('\n').trim_end().to_string()
        })
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

pub(crate) async fn fetch_ref_rows(
    refs: &[(String, RefEntry)],
    repo: &CachingRepo,
    mailmap: &Mailmap,
) -> Vec<RefRow> {
    futures::future::join_all(refs.iter().map(|(short, entry)| {
        let short = short.clone();
        async move {
            let commit = commit_for_entry(entry, repo).await?;
            Some(ref_row(short, &commit, mailmap))
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Releases indexed values in index order no matter what order they resolve in.
///
/// Concurrent fetches complete scrambled relative to their list — an
/// already-cached object resolves in a microtask while its neighbour waits on
/// the network, an annotated tag costs an extra round trip to peel, and over
/// HTTP/1.1 the browser's per-origin connection cap completes requests in waves
/// — and a table filling in scattered order reads as glitchy. Parking each
/// value as it lands and emitting only the contiguous resolved prefix makes the
/// table fill top-down while leaving the fetches fully concurrent: nothing waits
/// on anything, the reveal is merely sequenced.
struct InOrder<T> {
    /// `None` = not resolved yet; `Some(None)` = resolved with nothing to emit.
    slots: RefCell<Vec<Option<Option<T>>>>,
    /// Index of the next value to release; everything below it has been emitted.
    next: Cell<usize>,
}

impl<T> InOrder<T> {
    fn new(len: usize) -> Self {
        InOrder {
            slots: RefCell::new((0..len).map(|_| None).collect()),
            next: Cell::new(0),
        }
    }

    /// Record slot `i` as resolved, then emit however much of the prefix that
    /// unblocked — nothing at all unless `i` was itself the next one due. A
    /// `None` value consumes its slot without being emitted, so a value that
    /// fails to resolve can't stall the ones after it.
    fn resolve(&self, i: usize, value: Option<T>, emit: impl Fn(usize, T)) {
        self.slots.borrow_mut()[i] = Some(value);
        loop {
            let idx = self.next.get();
            // Scoped so the borrow is released before `emit` runs.
            let value = {
                let mut slots = self.slots.borrow_mut();
                match slots.get_mut(idx) {
                    Some(slot) if slot.is_some() => slot.take().flatten(),
                    _ => break,
                }
            };
            self.next.set(idx + 1);
            if let Some(value) = value {
                emit(idx, value);
            }
        }
    }
}

/// Resolve `refs` concurrently, calling `on_row(index, RefRow)` — `index` being
/// the ref's position in `refs` — to backfill a name-only skeleton row by row
/// instead of waiting for the whole list. Fetching is fully concurrent, but the
/// callbacks are delivered strictly in index order, so the table fills top-down
/// (see [`InOrder`]).
pub(crate) async fn fetch_ref_rows_each(
    refs: &[(String, RefEntry)],
    repo: &CachingRepo,
    mailmap: &Mailmap,
    on_row: impl Fn(usize, RefRow),
) {
    let reveal = InOrder::new(refs.len());
    let (reveal, on_row) = (&reveal, &on_row);

    futures::future::join_all(refs.iter().enumerate().map(|(i, (short, entry))| {
        let short = short.clone();
        async move {
            let row = commit_for_entry(entry, repo)
                .await
                .map(|commit| ref_row(short, &commit, mailmap));
            reveal.resolve(i, row, on_row);
        }
    }))
    .await;
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

/// Turn a commit into the row the log and summary tables render, with no ref
/// decorations attached — [`apply_decorations`] folds those in once the
/// (separately, concurrently fetched) decoration map resolves, so peeling every
/// tag never holds up the commit rows.
fn commit_row(commit: &Commit, mailmap: &Mailmap) -> CommitRow {
    CommitRow {
        id: commit.id(),
        short_hash: short_hash(commit.id()),
        message: commit_first_line(commit.message()),
        body: commit_body(commit.message()),
        author: mapped_author_name(commit, mailmap),
        age: Age::new(commit.author_date()),
        refs: Vec::new(),
    }
}

fn commit_rows(commits: &[Commit], mailmap: &Mailmap) -> Vec<CommitRow> {
    commits.iter().map(|c| commit_row(c, mailmap)).collect()
}

fn mapped_author_name(commit: &Commit, mailmap: &Mailmap) -> String {
    let (name, _) = mailmap.map(commit.author_name(), commit.author_email());
    String::from_utf8_lossy(name).into_owned()
}

pub(crate) fn mapped_ident(name: &[u8], email: &[u8], mailmap: &Mailmap) -> (String, String) {
    let (name, email) = mailmap.map(name, email);
    (
        String::from_utf8_lossy(name).into_owned(),
        String::from_utf8_lossy(email).into_owned(),
    )
}

/// Walk history a page at a time, calling `on_batch` with the rows gathered so
/// far after each chunk of commit objects is fetched, so the log can render
/// progressively instead of waiting for the whole page. The return value is
/// still the complete page plus whether a further page exists.
///
/// The walk itself is [`gib_log`]'s; what is left here is turning its commits
/// into rows and reporting what the walk cost to the console, where it is
/// visible whether the commit-graph (and its Bloom filters) is actually doing
/// the work.
pub(crate) async fn walk_commits_streamed(
    head_commit: &Commit,
    repo: &CachingRepo,
    mailmap: &Mailmap,
    path: Option<&str>,
    skip: usize,
    limit: usize,
    on_batch: impl Fn(&[CommitRow]),
) -> (Vec<CommitRow>, bool) {
    let page = gib_log::walk_commits(head_commit, repo, path, skip, limit, |commits| {
        on_batch(&commit_rows(commits, mailmap));
    })
    .await;

    crate::console_log(&format!(
        "webgit: log walk{}: {}, showing {} rows{}",
        path.map(|p| format!(" [{p}]")).unwrap_or_default(),
        page.stats,
        page.commits.len(),
        if page.has_more { " (more pages)" } else { "" },
    ));

    (commit_rows(&page.commits, mailmap), page.has_more)
}

/// The most recent `limit` commits reachable from `head_commit`, as rows,
/// streamed through `on_batch` as each commit object resolves. See
/// [`gib_log::recent_commits`] for why the summary's teaser deliberately
/// bypasses the commit-graph.
pub(crate) async fn recent_commits(
    head_commit: &Commit,
    repo: &CachingRepo,
    mailmap: &Mailmap,
    limit: usize,
    on_batch: impl Fn(&[CommitRow]),
) -> Vec<CommitRow> {
    let commits = gib_log::recent_commits(head_commit, repo, limit, |commits| {
        on_batch(&commit_rows(commits, mailmap));
    })
    .await;
    commit_rows(&commits, mailmap)
}

/// Fold a decoration map into already-built commit rows, matching on commit id,
/// so the summary can stream label-less rows from [`recent_commits`] and add the
/// branch/tag chips once its (separately, concurrently fetched) decoration map
/// resolves. A no-op when there is nothing to decorate.
///
/// The log calls this once per streamed partial as well as once for the finished
/// page, so it stays allocation-free: rows carry their [`ObjectId`], which is
/// looked up in `decorations` directly rather than via a hex-keyed side map.
pub(crate) fn apply_decorations(
    rows: &mut [CommitRow],
    decorations: &BTreeMap<ObjectId, Vec<RefLabel>>,
) {
    if decorations.is_empty() {
        return;
    }
    for row in rows.iter_mut() {
        if let Some(labels) = decorations.get(&row.id) {
            row.refs = labels.clone();
        }
    }
}

fn ref_row(name: String, c: &Commit, mailmap: &Mailmap) -> RefRow {
    RefRow {
        name,
        meta: Some(RefMeta {
            message: commit_first_line(c.message()),
            author: mapped_author_name(c, mailmap),
            age: Age::new(c.author_date()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gib::object::{ObjectType, RawObject};

    /// A commit object authored (and committed) by one contact, as the row
    /// builders receive it.
    fn commit_by(name: &str, email: &str) -> Commit {
        let body = format!(
            "tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb\n\
             author {name} <{email}> 1774735018 +0000\n\
             committer {name} <{email}> 1774735018 +0000\n\
             \n\
             Fix the thing\n"
        );
        gib::object::Object::from_raw(
            ObjectId::from_hex(b"0123abcd0123abcd0123abcd0123abcd0123abcd").unwrap(),
            RawObject {
                object_type: ObjectType::Commit,
                body: body.into_bytes(),
            },
        )
        .unwrap()
        .commit()
        .unwrap()
    }

    /// The author a listing row shows. `commit_row` and `ref_row` are not
    /// called directly: building a row reads the wall clock for its age
    /// column, and that clock is `js_sys`', which panics off the browser. What
    /// both of them do with a contact is this.
    #[test]
    fn listings_show_the_mailmapped_author() {
        let mailmap = Mailmap::parse(b"Proper Name <proper@example.org> <commit@example.org>\n");
        let commit = commit_by("Commit Name", "commit@example.org");

        assert_eq!(mapped_author_name(&commit, &mailmap), "Proper Name");
        // With no mailmap in the repository, the commit's own name shows.
        assert_eq!(
            mapped_author_name(&commit, &Mailmap::default()),
            "Commit Name"
        );
    }

    #[test]
    fn a_contact_is_mapped_on_both_of_its_halves() {
        // A name-keyed entry only applies to that name, so this fails unless
        // the email *and* the name reach the map.
        let mailmap =
            Mailmap::parse(b"Proper Name <proper@example.org> Commit Name <commit@example.org>\n");

        assert_eq!(
            mapped_author_name(&commit_by("Commit Name", "commit@example.org"), &mailmap),
            "Proper Name"
        );
        assert_eq!(
            mapped_author_name(&commit_by("Other Name", "commit@example.org"), &mailmap),
            "Other Name"
        );
    }

    /// Both halves of a contact are mapped together, and the commit view maps
    /// its committer the same way it maps its author.
    #[test]
    fn an_ident_is_mapped_as_a_pair() {
        let mailmap = Mailmap::parse(b"Proper Name <proper@example.org> <commit@example.org>\n");
        let commit = commit_by("Commit Name", "commit@example.org");

        let expected = ("Proper Name".to_string(), "proper@example.org".to_string());
        assert_eq!(
            mapped_ident(commit.author_name(), commit.author_email(), &mailmap),
            expected
        );
        assert_eq!(
            mapped_ident(commit.committer_name(), commit.committer_email(), &mailmap),
            expected
        );
    }

    /// Resolve `order` (a permutation of slot indices) and collect what was
    /// emitted, as `(index, value)` pairs in emission order.
    fn reveal_sequence(len: usize, order: &[usize]) -> Vec<(usize, usize)> {
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(len);
        for &i in order {
            reveal.resolve(i, Some(i * 10), |idx, v| {
                emitted.borrow_mut().push((idx, v))
            });
        }
        emitted.into_inner()
    }

    #[test]
    fn in_order_emits_in_index_order_however_it_resolves() {
        // Reverse, then a scrambled order: emission is by index either way.
        let expected: Vec<(usize, usize)> = (0..4).map(|i| (i, i * 10)).collect();
        assert_eq!(reveal_sequence(4, &[3, 2, 1, 0]), expected);
        assert_eq!(reveal_sequence(4, &[1, 3, 0, 2]), expected);
        assert_eq!(reveal_sequence(4, &[0, 1, 2, 3]), expected);
    }

    #[test]
    fn in_order_holds_values_behind_an_unresolved_slot() {
        // Slot 0 outstanding: nothing may be emitted, however many land after it.
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(3);
        let push = |idx, v| emitted.borrow_mut().push((idx, v));

        reveal.resolve(2, Some(20), push);
        reveal.resolve(1, Some(10), push);
        assert!(emitted.borrow().is_empty(), "nothing precedes slot 0");

        // Slot 0 landing releases the whole run at once.
        reveal.resolve(0, Some(0), push);
        assert_eq!(emitted.into_inner(), vec![(0, 0), (1, 10), (2, 20)]);
    }

    #[test]
    fn in_order_skips_unresolvable_values_without_stalling() {
        // A ref whose commit doesn't resolve consumes its slot silently; the
        // rows after it must still come through.
        let emitted = RefCell::new(Vec::new());
        let reveal = InOrder::new(3);
        let push = |idx, v| emitted.borrow_mut().push((idx, v));

        reveal.resolve(1, Some(10), push);
        reveal.resolve(0, None, push);
        assert_eq!(*emitted.borrow(), vec![(1, 10)]);

        reveal.resolve(2, Some(20), push);
        assert_eq!(emitted.into_inner(), vec![(1, 10), (2, 20)]);
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

    #[test]
    fn test_commit_body() {
        assert_eq!(
            commit_body(b"Fix bug\n\nLonger description\n"),
            "Longer description"
        );
        // Paragraphs and trailers inside the body are kept as written; only the
        // separator above and the whitespace below come off.
        assert_eq!(
            commit_body(b"Fix bug\n\nWhy\n\nSigned-off-by: A <a@example.org>\n\n"),
            "Why\n\nSigned-off-by: A <a@example.org>"
        );
        // Indentation is part of the body, not leading whitespace to trim.
        assert_eq!(commit_body(b"Fix bug\n\n    code\n"), "    code");
        // Nothing after the subject is no body at all, not an empty row.
        assert_eq!(commit_body(b"Single line"), "");
        assert_eq!(commit_body(b"trailing newline\n"), "");
        assert_eq!(commit_body(b""), "");
    }
}
