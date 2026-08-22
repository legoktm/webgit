//! Differential tests for the log walk, against `git rev-list`.
//!
//! A repository is built with the `git` CLI and then walked from its own object
//! store, so what is compared is the whole thing: the commit-time frontier, the
//! `skip`/`limit` window, and the path filter — against the commit ids git
//! itself prints for the same query.
//!
//! Each filtered case is run three ways over the identical repository — with no
//! commit-graph, with one, and with one carrying changed-path Bloom filters —
//! since those are three quite different routes to the same answer: reading
//! commit objects, reading graph records, and skipping commits on a filter
//! without reading a tree at all. All three must agree with git.
//!
//! Two behaviours get fixtures of their own, because the main one cannot show
//! them: `equal_timestamps` pins the order of commits that share a second, and
//! `pruning` pins that a branch whose changes the merge discarded is followed
//! no further — git's parent rewriting.

use crate::{CommitSource, GraphRecord, recent_commits, walk_commits};
use futures::FutureExt;
use futures::executor::block_on;
use futures::future::LocalBoxFuture;
use gib_commitgraph::CommitGraph;
use gib_commitgraph::bloom::BloomSettings;
use gib_fs::Directory;
use gib_object::{Commit, Object, ObjectId};
use gib_odb::ObjectDb;
use gib_testkit::{TestFileSystem, TestRepo};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::rc::Rc;

/// The repository's objects and (optionally) its commit-graph, as a
/// [`CommitSource`]. Graph records are memoised the way a real caller's would
/// be, so a walk doesn't re-read the file once per parent edge.
struct Source {
    odb: ObjectDb<TestFileSystem>,
    graph: Option<CommitGraph<TestFileSystem>>,
    records: RefCell<BTreeMap<ObjectId, Rc<GraphRecord>>>,
}

impl CommitSource for Source {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move {
            let raw = self
                .odb
                .lookup(id)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .ok_or_else(|| anyhow::anyhow!("missing object {id}"))?;
            Object::from_raw(id, raw).map_err(|e| anyhow::anyhow!("{e:?}"))
        }
        .boxed_local()
    }

    fn graph_record(&self, id: ObjectId) -> LocalBoxFuture<'_, Option<Rc<GraphRecord>>> {
        async move {
            if let Some(rec) = self.records.borrow().get(&id) {
                return Some(Rc::clone(rec));
            }
            let (entry, bloom) = self.graph.as_ref()?.record(id).await.ok().flatten()?;
            let rec = Rc::new(GraphRecord {
                tree: entry.tree,
                parents: entry.parents,
                commit_time: entry.commit_time,
                bloom,
            });
            self.records.borrow_mut().insert(id, Rc::clone(&rec));
            Some(rec)
        }
        .boxed_local()
    }

    fn bloom_settings(&self) -> Option<BloomSettings> {
        self.graph.as_ref()?.bloom_settings()
    }
}

/// Which commit-graph, if any, the repository is walked with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Graph {
    /// No commit-graph file: every commit's metadata comes from its object.
    None,
    /// A commit-graph without changed-path filters: metadata is cheap, but
    /// every filtered candidate still needs a real tree diff.
    Plain,
    /// A commit-graph with changed-path Bloom filters, which is what the
    /// filtered walk is built around.
    Bloom,
}

impl Graph {
    /// Write (or remove) the repository's commit-graph to match, then open it.
    fn open(self, repo: &TestRepo) -> Option<CommitGraph<TestFileSystem>> {
        let info = repo.location.path().join(".git/objects/info/commit-graph");
        let _ = std::fs::remove_file(&info);
        match self {
            Graph::None => return None,
            Graph::Plain => repo.run_git(["commit-graph", "write", "--reachable"]),
            Graph::Bloom => {
                repo.run_git(["commit-graph", "write", "--reachable", "--changed-paths"])
            }
        }
        .unwrap();
        assert!(info.exists(), "git wrote no commit-graph for {self:?}");
        let objects = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
        let graph = block_on(CommitGraph::open(&objects)).unwrap();
        assert!(
            graph.is_some(),
            "the commit-graph we just wrote is unusable"
        );
        if self == Graph::Bloom {
            assert!(
                graph.as_ref().unwrap().has_bloom(),
                "expected changed-path filters in the commit-graph"
            );
        }
        graph
    }
}

fn open_source(repo: &TestRepo, graph: Graph) -> Source {
    let graph = graph.open(repo);
    let objects = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
    Source {
        odb: block_on(ObjectDb::open(objects, 64 * 1024 * 1024)).unwrap(),
        graph,
        records: RefCell::new(BTreeMap::new()),
    }
}

fn head_commit(repo: &TestRepo, source: &Source) -> Commit {
    let id = rev_parse(repo, "HEAD");
    block_on(source.object(id)).unwrap().commit().unwrap()
}

fn rev_parse(repo: &TestRepo, rev: &str) -> ObjectId {
    let out = repo.run_git(["rev-parse", rev]).unwrap();
    ObjectId::from_hex(out.trim_ascii()).unwrap()
}

/// The commit ids `git rev-list` prints for `args`, newest first.
fn rev_list(repo: &TestRepo, args: &[&str]) -> Vec<ObjectId> {
    let mut all = vec!["rev-list"];
    all.extend_from_slice(args);
    String::from_utf8(repo.run_git(all).unwrap())
        .unwrap()
        .lines()
        .map(|line| ObjectId::from_hex(line.trim().as_bytes()).unwrap())
        .collect()
}

fn ids(commits: &[Commit]) -> Vec<ObjectId> {
    commits.iter().map(Commit::id).collect()
}

/// A commit on the current branch, with `date` used for both author and
/// committer so the frontier's ordering is deterministic.
fn commit_at(repo: &TestRepo, message: &str, minute: u32) {
    repo.run_git(["add", "-A"]).unwrap();
    repo.commit(message, "a user", "an-email", &date(minute))
        .unwrap();
}

fn date(minute: u32) -> String {
    format!("2020-01-01T00:{minute:02}:00Z")
}

fn write(repo: &TestRepo, path: &str, contents: &str) {
    let path = repo.location.path().join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A history with a branch and a merge, commits at one-minute intervals so no
/// two share a commit time, and paths at three depths so the filter's lockstep
/// tree descent is exercised below the root.
///
/// ```text
///  1 root          a.txt, dir/b.txt, dir/sub/c.txt
///  2 a.txt
///  3 dir/sub/c.txt
///    ├── side: 4 dir/b.txt
///    └── main: 5 other.txt, 6 a.txt
///  7 merge (side into main)
///  8 dir/sub/c.txt
///  9 (empty commit — touches nothing)
/// ```
fn fixture() -> TestRepo {
    let repo = TestRepo::new().unwrap();
    write(&repo, "a.txt", "a1\n");
    write(&repo, "dir/b.txt", "b1\n");
    write(&repo, "dir/sub/c.txt", "c1\n");
    commit_at(&repo, "root", 1);

    write(&repo, "a.txt", "a2\n");
    commit_at(&repo, "touch a.txt", 2);

    write(&repo, "dir/sub/c.txt", "c2\n");
    commit_at(&repo, "touch dir/sub/c.txt", 3);

    repo.run_git(["checkout", "-b", "side"]).unwrap();
    write(&repo, "dir/b.txt", "b2\n");
    commit_at(&repo, "side: touch dir/b.txt", 4);

    repo.run_git(["checkout", "main"]).unwrap();
    write(&repo, "other.txt", "o1\n");
    commit_at(&repo, "touch other.txt", 5);
    write(&repo, "a.txt", "a3\n");
    commit_at(&repo, "touch a.txt again", 6);

    merge(&repo, "side", 7);

    write(&repo, "dir/sub/c.txt", "c3\n");
    commit_at(&repo, "touch dir/sub/c.txt again", 8);

    // A commit that changes nothing: TREESAME to its parent at every path, so
    // no filtered query may show it.
    repo.run_git(["commit", "--allow-empty", "-m", "empty"])
        .unwrap();
    repo
}

/// `git merge --no-ff`, which `TestRepo` has no helper for.
fn merge(repo: &TestRepo, branch: &str, minute: u32) {
    let status = repo
        .git_command()
        .env("GIT_AUTHOR_DATE", date(minute))
        .env("GIT_COMMITTER_DATE", date(minute))
        .args(["merge", "--no-ff", "-m", "merge side", branch])
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success(), "git merge failed");
}

const GRAPHS: [Graph; 3] = [Graph::None, Graph::Plain, Graph::Bloom];

/// The unfiltered walk visits history in the same order `git rev-list` does,
/// however the commit metadata was obtained.
#[test]
fn test_unfiltered_walk_matches_rev_list() {
    let repo = fixture();
    let expected = rev_list(&repo, &["HEAD"]);
    for graph in GRAPHS {
        let source = open_source(&repo, graph);
        let head = head_commit(&repo, &source);
        let page = block_on(walk_commits(&head, &source, None, 0, 100, |_| {}));
        assert_eq!(ids(&page.commits), expected, "{graph:?}");
        assert!(!page.has_more, "{graph:?}: the whole history fits one page");
    }
}

/// Every `skip`/`limit` window over that history is the same window
/// `git rev-list --skip=N --max-count=M` prints, and `has_more` says exactly
/// whether git would print anything past it.
#[test]
fn test_pagination_windows_match_rev_list() {
    let repo = fixture();
    let total = rev_list(&repo, &["HEAD"]).len();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);

    for limit in [1, 2, 4, total] {
        for skip in 0..=total {
            let expected = rev_list(
                &repo,
                &[
                    "HEAD",
                    &format!("--skip={skip}"),
                    &format!("--max-count={limit}"),
                ],
            );
            let page = block_on(walk_commits(&head, &source, None, skip, limit, |_| {}));
            assert_eq!(ids(&page.commits), expected, "skip={skip} limit={limit}");
            assert_eq!(
                page.has_more,
                skip + limit < total,
                "skip={skip} limit={limit}"
            );
        }
    }
}

/// The path filter picks the same commits `git rev-list -- <path>` does, for a
/// root file, a directory, and a file nested inside one — and does so whether
/// the verdict comes from commit objects, graph records, or a Bloom filter.
#[test]
fn test_path_filter_matches_rev_list() {
    let repo = fixture();
    for path in ["a.txt", "dir", "dir/sub/c.txt", "other.txt"] {
        let expected = rev_list(&repo, &["HEAD", "--", path]);
        assert!(!expected.is_empty(), "{path} should have some history");
        for graph in GRAPHS {
            let source = open_source(&repo, graph);
            let head = head_commit(&repo, &source);
            let page = block_on(walk_commits(&head, &source, Some(path), 0, 100, |_| {}));
            assert_eq!(ids(&page.commits), expected, "{path} with {graph:?}");
        }
    }
}

/// A path that never existed yields nothing, rather than every commit.
#[test]
fn test_filter_on_absent_path_is_empty() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);
    let page = block_on(walk_commits(
        &head,
        &source,
        Some("no/such/path"),
        0,
        100,
        |_| {},
    ));
    assert!(ids(&page.commits).is_empty());
    assert!(!page.has_more);
}

/// Paginating a *filtered* log matches git too — the window is over the
/// matching commits, not over the ones traversed to find them. Run over the
/// pruning fixture as well, so the windowing is checked on a history where
/// commits drop out because a merge pruned their branch away.
#[test]
fn test_filtered_pagination_matches_rev_list() {
    for (repo, path) in [(fixture(), "dir"), (pruning_fixture(), "p.txt")] {
        let source = open_source(&repo, Graph::Bloom);
        let head = head_commit(&repo, &source);
        let total = rev_list(&repo, &["HEAD", "--", path]).len();

        for skip in 0..=total {
            let expected = rev_list(
                &repo,
                &[
                    "HEAD",
                    &format!("--skip={skip}"),
                    "--max-count=2",
                    "--",
                    path,
                ],
            );
            let page = block_on(walk_commits(&head, &source, Some(path), skip, 2, |_| {}));
            assert_eq!(ids(&page.commits), expected, "{path} skip={skip}");
            assert_eq!(page.has_more, skip + 2 < total, "{path} skip={skip}");
        }
    }
}

/// The merge is TREESAME to the side branch at `dir`, so neither git's default
/// simplification nor this walk shows it — while the side commit that did touch
/// `dir` is shown, reached through the merge's second parent.
#[test]
fn test_merge_is_hidden_when_treesame_to_a_parent() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);
    let page = block_on(walk_commits(&head, &source, Some("dir"), 0, 100, |_| {}));
    let shown = ids(&page.commits);
    let merge = rev_parse(&repo, "HEAD~2");
    let side = rev_parse(&repo, "side");
    assert!(!shown.contains(&merge), "the merge changed nothing at dir");
    assert!(shown.contains(&side), "the side commit touched dir/b.txt");
}

/// Changed-path filters are actually consulted: with them, most of the history
/// is discarded without a tree diff, and the graph answers the metadata.
#[test]
fn test_bloom_filters_skip_commits_without_diffing() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);
    let page = block_on(walk_commits(
        &head,
        &source,
        Some("other.txt"),
        0,
        100,
        |_| {},
    ));

    assert!(page.stats.bloom_skips > 0, "{}", page.stats);
    assert!(
        page.stats.tree_diffs < page.stats.traversed,
        "{}",
        page.stats
    );
    assert_eq!(page.stats.object_meta_fallbacks, 0, "{}", page.stats);
    assert_eq!(
        page.stats.graph_meta_hits, page.stats.traversed,
        "{}",
        page.stats
    );

    // Without filters the same query needs a diff for all but the root.
    let plain = open_source(&repo, Graph::Plain);
    let head = head_commit(&repo, &plain);
    let page = block_on(walk_commits(
        &head,
        &plain,
        Some("other.txt"),
        0,
        100,
        |_| {},
    ));
    assert_eq!(page.stats.bloom_skips, 0, "{}", page.stats);
}

/// Without a commit-graph every commit's metadata is read from its object, and
/// the walk still produces git's answer (asserted above); this pins the route.
#[test]
fn test_walk_without_commit_graph_reads_objects() {
    let repo = fixture();
    let source = open_source(&repo, Graph::None);
    let head = head_commit(&repo, &source);
    let page = block_on(walk_commits(&head, &source, None, 0, 100, |_| {}));
    assert_eq!(page.stats.graph_meta_hits, 0, "{}", page.stats);
    assert_eq!(page.stats.object_meta_fallbacks, page.stats.traversed);
}

/// `recent_commits` is the same order as the unfiltered walk, capped at `limit`
/// — and reads objects only, never the graph.
#[test]
fn test_recent_commits_matches_rev_list_head() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);
    for limit in [1, 3, 10, 100] {
        let expected = rev_list(&repo, &["HEAD", &format!("--max-count={limit}")]);
        let commits = block_on(recent_commits(&head, &source, limit, |_| {}));
        assert_eq!(ids(&commits), expected, "limit={limit}");
    }
}

/// Both walks stream: `on_batch` sees strictly growing prefixes of the page it
/// finally returns, so a caller can paint rows before the last object lands.
#[test]
fn test_streaming_emits_growing_prefixes() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = head_commit(&repo, &source);

    let seen: RefCell<Vec<Vec<ObjectId>>> = RefCell::new(Vec::new());
    let page = block_on(walk_commits(&head, &source, None, 0, 100, |commits| {
        seen.borrow_mut().push(ids(commits));
    }));
    assert_prefixes(&seen.borrow(), &ids(&page.commits));

    let seen: RefCell<Vec<Vec<ObjectId>>> = RefCell::new(Vec::new());
    let commits = block_on(recent_commits(&head, &source, 5, |commits| {
        seen.borrow_mut().push(ids(commits));
    }));
    assert_prefixes(&seen.borrow(), &ids(&commits));
    // One emission per commit, since `recent_commits` pops one at a time.
    assert_eq!(seen.borrow().len(), commits.len());
}

fn assert_prefixes(batches: &[Vec<ObjectId>], full: &[ObjectId]) {
    assert!(!batches.is_empty(), "nothing was streamed");
    let mut last = 0;
    for batch in batches {
        assert!(
            batch.len() > last,
            "batches must grow: {last} -> {}",
            batch.len()
        );
        assert_eq!(batch.as_slice(), &full[..batch.len()], "not a prefix");
        last = batch.len();
    }
    assert_eq!(batches.last().unwrap().as_slice(), full);
}

/// A history in which *every* commit shares one timestamp, so nothing but the
/// frontier's tie-break decides the order.
///
/// ```text
///  root ── main-1 ── main-2 ── main-3 ─┐
///             └───── side-1 ── side-2 ─┴─ merge
/// ```
fn equal_timestamp_fixture() -> TestRepo {
    let repo = TestRepo::new().unwrap();
    let commit = |msg: &str| {
        repo.run_git(["add", "-A"]).unwrap();
        // The same instant for all of them, author and committer alike.
        repo.commit(msg, "a user", "an-email", &date(0)).unwrap();
    };
    write(&repo, "r.txt", "r\n");
    commit("root");
    for i in 1..=3 {
        write(&repo, &format!("m{i}.txt"), "m\n");
        commit(&format!("main-{i}"));
    }
    repo.run_git(["checkout", "-b", "side", "main~2"]).unwrap();
    for i in 1..=2 {
        write(&repo, &format!("s{i}.txt"), "s\n");
        commit(&format!("side-{i}"));
    }
    repo.run_git(["checkout", "main"]).unwrap();
    merge(&repo, "side", 0);
    repo
}

/// Commits sharing a commit time come out in git's order, which is the order
/// they were discovered in — not object-id order, which is what an unstable
/// heap would give and what a reader would see as a parent listed above its
/// own child.
#[test]
fn test_equal_timestamps_order_like_git() {
    let repo = equal_timestamp_fixture();
    let expected = rev_list(&repo, &["HEAD"]);

    // Guard the fixture: if the timestamps ever stopped colliding, or if the
    // ids happened to fall in descending order anyway, this would pass without
    // testing anything.
    let times = String::from_utf8(repo.run_git(["log", "--format=%ct"]).unwrap()).unwrap();
    assert_eq!(
        times
            .lines()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "the fixture is supposed to share one timestamp"
    );
    let mut by_id = expected.clone();
    by_id.sort_by(|a, b| b.cmp(a));
    assert_ne!(by_id, expected, "git's order is object-id order here");

    for graph in GRAPHS {
        let source = open_source(&repo, graph);
        let head = head_commit(&repo, &source);
        let page = block_on(walk_commits(&head, &source, None, 0, 100, |_| {}));
        assert_eq!(ids(&page.commits), expected, "{graph:?}");
        let commits = block_on(recent_commits(&head, &source, 100, |_| {}));
        assert_eq!(ids(&commits), expected, "recent_commits with {graph:?}");
    }
}

/// A history where a merge throws away what the side branch did to `p.txt`:
/// the merge keeps its first parent's version, so it is TREESAME to that
/// parent and git follows only it, never reaching `side changes p`.
///
/// ```text
///  root ── main changes p ─┐
///     └─── side changes p ─┴─ merge (keeps main's p.txt)
/// ```
fn pruning_fixture() -> TestRepo {
    let repo = TestRepo::new().unwrap();
    write(&repo, "p.txt", "p0\n");
    write(&repo, "x.txt", "x\n");
    commit_at(&repo, "root", 1);

    repo.run_git(["checkout", "-b", "side"]).unwrap();
    write(&repo, "p.txt", "side\n");
    commit_at(&repo, "side changes p", 2);

    repo.run_git(["checkout", "main"]).unwrap();
    write(&repo, "p.txt", "main\n");
    commit_at(&repo, "main changes p", 3);

    // `-X ours` resolves p.txt to main's version, which is what makes the
    // merge TREESAME to its first parent.
    let status = repo
        .git_command()
        .env("GIT_AUTHOR_DATE", date(4))
        .env("GIT_COMMITTER_DATE", date(4))
        .args(["merge", "--no-ff", "-X", "ours", "-m", "merge", "side"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success(), "git merge failed");
    assert_eq!(
        std::fs::read_to_string(repo.location.path().join("p.txt")).unwrap(),
        "main\n",
        "the merge is supposed to keep main's p.txt"
    );
    repo
}

/// git rewrites a merge's parent list down to the first parent it is TREESAME
/// to and follows only that one, so the side branch's change to `p.txt` — undone
/// by the merge — is not in `git log -- p.txt`, and must not be here either.
#[test]
fn test_merge_prunes_the_parent_it_is_treesame_to() {
    let repo = pruning_fixture();
    let expected = rev_list(&repo, &["HEAD", "--", "p.txt"]);
    let side = rev_parse(&repo, "side");
    assert!(
        !expected.contains(&side),
        "the fixture is supposed to hide the side commit from git too"
    );

    for graph in GRAPHS {
        let source = open_source(&repo, graph);
        let head = head_commit(&repo, &source);
        let page = block_on(walk_commits(&head, &source, Some("p.txt"), 0, 100, |_| {}));
        assert_eq!(ids(&page.commits), expected, "{graph:?}");
    }
}

/// Pruning is about traversal, not just display: the parents git drops are
/// never visited at all, so a walk that reached the pruned branch would be
/// visibly doing more work.
///
/// Both routes to the verdict are covered. With Bloom filters the merge is
/// dismissed against its first parent without a diff, and the walk has to
/// follow that parent alone off the filter's word; without them the same call
/// has to be settled by real diffs first.
#[test]
fn test_pruning_stops_the_walk_from_visiting_the_branch() {
    let repo = pruning_fixture();
    for graph in GRAPHS {
        let source = open_source(&repo, graph);
        let head = head_commit(&repo, &source);
        let filtered = block_on(walk_commits(&head, &source, Some("p.txt"), 0, 100, |_| {}));
        let whole = block_on(walk_commits(&head, &source, None, 0, 100, |_| {}));
        assert!(
            filtered.stats.traversed < whole.stats.traversed,
            "{graph:?}: filtered walk traversed {} of {} commits — nothing was pruned",
            filtered.stats.traversed,
            whole.stats.traversed,
        );
    }
}
