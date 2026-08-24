//! Differential tests for blame, against `git blame` itself.
//!
//! A repository is built with the `git` CLI and blamed from its own object
//! store, then compared with what `git blame --line-porcelain` says about the
//! same file at the same revision. The comparison is of *groups*, not just of
//! per-line commits: porcelain's header line carries the run's length on the
//! first line of each run, so the fixture pins the line numbers on both sides
//! and the coalescing that produced the runs, which is the whole output the
//! view renders.
//!
//! Every case is run three ways over the identical repository — with no
//! commit-graph, with one, and with one carrying changed-path Bloom filters —
//! because those are three different routes to the answer: reading commit
//! objects, reading graph records, and skipping a parent's tree on a filter.
//! A Bloom filter that made blame skip a commit it should have examined would
//! be invisible in any single-configuration run.
//!
//! # Why git is invoked with the indent heuristic off
//!
//! `git blame` on the command line diffs with `XDF_INDENT_HEURISTIC`, which
//! `diff.indentHeuristic` turns on by default. cgit's blame does not: it never
//! sets `sb.xdl_opts`, so the scoreboard diffs with no flags at all, and this
//! crate matches cgit (as `gib-xdiff` does throughout — see its own
//! differential tests). The two agree on every file where a changed run has
//! only one minimal placement, and disagree by a line or two of slide where
//! several are possible. `-c diff.indentHeuristic=false` is what makes the CLI
//! answer the same question this crate does; without it the fixtures below
//! would be testing that difference rather than the blame walk.

use crate::{BlameGroup, blame};
use futures::FutureExt;
use futures::executor::block_on;
use futures::future::LocalBoxFuture;
use gib_commitgraph::CommitGraph;
use gib_commitgraph::bloom::BloomSettings;
use gib_fs::Directory;
use gib_log::{CommitSource, GraphRecord};
use gib_object::{Commit, Object, ObjectId};
use gib_odb::ObjectDb;
use gib_testkit::{TestFileSystem, TestRepo};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::rc::Rc;

/// The repository's objects and (optionally) its commit-graph, as a
/// [`CommitSource`]. Graph records are memoised the way a real caller's would
/// be, so the walk doesn't re-read the file once per parent edge.
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

/// Which commit-graph, if any, the repository is blamed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Graph {
    /// No commit-graph file: every commit's metadata comes from its object.
    None,
    /// A commit-graph without changed-path filters: metadata is cheap, but
    /// every parent still needs its tree walked to find the file.
    Plain,
    /// A commit-graph with changed-path Bloom filters, which is what lets
    /// blame skip a parent's tree entirely.
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

const GRAPHS: [Graph; 3] = [Graph::None, Graph::Plain, Graph::Bloom];

fn open_source(repo: &TestRepo, graph: Graph) -> Source {
    let graph = graph.open(repo);
    let objects = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
    Source {
        odb: block_on(ObjectDb::open(objects, 64 * 1024 * 1024)).unwrap(),
        graph,
        records: RefCell::new(BTreeMap::new()),
    }
}

fn rev_parse(repo: &TestRepo, rev: &str) -> ObjectId {
    let out = repo.run_git(["rev-parse", rev]).unwrap();
    ObjectId::from_hex(out.trim_ascii()).unwrap()
}

fn commit_at(repo: &TestRepo, rev: &str, source: &Source) -> Commit {
    let id = rev_parse(repo, rev);
    block_on(source.object(id)).unwrap().commit().unwrap()
}

/// One run of lines as either side reports it, with every number one-based so
/// that a mismatch reads the way `git blame` prints it.
#[derive(Debug, PartialEq, Eq)]
struct Group {
    commit: ObjectId,
    /// First line of the run in the file being blamed.
    line: usize,
    /// The same run's first line in the commit's copy of the file.
    orig_line: usize,
    num_lines: usize,
}

fn ours(groups: &[BlameGroup]) -> Vec<Group> {
    groups
        .iter()
        .map(|g| Group {
            commit: g.commit,
            line: g.start + 1,
            orig_line: g.orig_start + 1,
            num_lines: g.num_lines,
        })
        .collect()
}

/// What `git blame` makes of `path` at `rev`.
///
/// `--line-porcelain` repeats each line's header, so the structure is regular:
/// a header, some metadata, then the line's content prefixed with a tab. Only
/// the headers carrying a fourth field start a run — the rest continue one —
/// which is exactly git's list of blame entries after coalescing.
fn git_blame(repo: &TestRepo, rev: &str, path: &str) -> Vec<Group> {
    let out = repo
        .run_git([
            // See this module's header: cgit's blame diffs with no flags, and
            // this is what turns the same one off in the CLI.
            "-c",
            "diff.indentHeuristic=false",
            "blame",
            "--line-porcelain",
            rev,
            "--",
            path,
        ])
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let mut groups = Vec::new();
    let mut expect_header = true;
    for line in text.lines() {
        if expect_header {
            expect_header = false;
            let mut fields = line.split(' ');
            let oid = ObjectId::from_hex(fields.next().unwrap().as_bytes())
                .unwrap_or_else(|| panic!("unparsable blame header: {line:?}"));
            let orig_line: usize = fields.next().unwrap().parse().unwrap();
            let final_line: usize = fields.next().unwrap().parse().unwrap();
            // A fourth field means this line opens a new run.
            if let Some(num_lines) = fields.next() {
                groups.push(Group {
                    commit: oid,
                    line: final_line,
                    orig_line,
                    num_lines: num_lines.parse().unwrap(),
                });
            }
        } else if line.starts_with('\t') {
            // The line's content, which ends this line's record.
            expect_header = true;
        }
    }
    groups
}

/// Blame `path` at `rev` every way the object store can be read, and assert
/// each run matches git's. Returns the groups, so a caller can make further
/// assertions about a case it knows the shape of.
fn check(repo: &TestRepo, rev: &str, path: &str) -> Vec<BlameGroup> {
    let expected = git_blame(repo, rev, path);
    assert!(
        !expected.is_empty(),
        "`git blame {rev} -- {path}` produced nothing to compare against"
    );
    let mut last = None;
    for graph in GRAPHS {
        let source = open_source(repo, graph);
        let head = commit_at(repo, rev, &source);
        let blame = block_on(blame(&head, &source, path, |_| async {})).unwrap();
        assert_eq!(
            ours(&blame.groups),
            expected,
            "{path} at {rev} with {graph:?}"
        );
        // Every line of the file is covered exactly once, in order.
        let covered: usize = blame.groups.iter().map(|g| g.num_lines).sum();
        assert_eq!(covered, blame.num_lines, "{path} at {rev} with {graph:?}");
        let mut next = 0;
        for group in &blame.groups {
            assert_eq!(group.start, next, "{path} at {rev} with {graph:?}");
            next += group.num_lines;
        }
        last = Some(blame.groups);
    }
    last.expect("GRAPHS is not empty")
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

fn write(repo: &TestRepo, path: &str, contents: &str) {
    let path = repo.location.path().join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn date(minute: u32) -> String {
    format!("2020-01-01T00:{minute:02}:00Z")
}

/// A commit on the current branch, with `date` used for both author and
/// committer so the frontier's ordering is deterministic.
fn commit(repo: &TestRepo, message: &str, minute: u32) {
    repo.run_git(["add", "-A"]).unwrap();
    repo.commit(message, "a user", "an-email", &date(minute))
        .unwrap();
}

/// `git merge --no-ff`, which `TestRepo` has no helper for. Conflicts are not
/// expected: the branches below touch different parts of the file.
fn merge(repo: &TestRepo, branch: &str, minute: u32) {
    let status = repo
        .git_command()
        .env("GIT_AUTHOR_DATE", date(minute))
        .env("GIT_COMMITTER_DATE", date(minute))
        .args(["merge", "--no-ff", "-m", &format!("merge {branch}"), branch])
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success(), "git merge failed");
}

/// A history that puts every shape of edit through the same file, then merges
/// two branches that both touched it.
///
/// ```text
///  1 root        src/app.c (8 lines), notes.txt, dir/sub/deep.txt
///  2 src/app.c   one line replaced in the middle
///  3 src/app.c   two lines inserted near the top
///    ├── side: 4 src/app.c  the tail rewritten
///    └── main: 5 src/app.c  the head rewritten
///  6 merge (side into main)   — both sides' edits survive
///  7 src/app.c   a line deleted
///  8 late.txt    added, so a file whose history starts late has one
///  9 (empty commit)
/// ```
///
/// The file is C-shaped on purpose: repeated `}` and blank lines are what make
/// a changed run slidable, which is where a blame that merely resembles git's
/// starts attributing lines to the wrong commit.
fn fixture() -> TestRepo {
    let repo = TestRepo::new().unwrap();
    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         \n\
         int main(void)\n\
         {\n\
         \tputs(\"one\");\n\
         \treturn 0;\n\
         }\n\
         \n",
    );
    write(&repo, "notes.txt", "a\nb\nc\n");
    write(&repo, "dir/sub/deep.txt", "deep\n");
    commit(&repo, "root", 1);

    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         \n\
         int main(void)\n\
         {\n\
         \tputs(\"two\");\n\
         \treturn 0;\n\
         }\n\
         \n",
    );
    commit(&repo, "replace a line", 2);

    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         #include <stdlib.h>\n\
         \n\
         static void helper(void)\n\
         {\n\
         }\n\
         \n\
         int main(void)\n\
         {\n\
         \tputs(\"two\");\n\
         \treturn 0;\n\
         }\n\
         \n",
    );
    commit(&repo, "insert a helper", 3);

    repo.run_git(["checkout", "-b", "side"]).unwrap();
    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         #include <stdlib.h>\n\
         \n\
         static void helper(void)\n\
         {\n\
         }\n\
         \n\
         int main(void)\n\
         {\n\
         \tputs(\"two\");\n\
         \thelper();\n\
         \treturn EXIT_SUCCESS;\n\
         }\n\
         \n",
    );
    commit(&repo, "side: rewrite the tail", 4);

    repo.run_git(["checkout", "main"]).unwrap();
    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         #include <stdlib.h>\n\
         #include <string.h>\n\
         \n\
         static void helper(void)\n\
         {\n\
         }\n\
         \n\
         int main(void)\n\
         {\n\
         \tputs(\"two\");\n\
         \treturn 0;\n\
         }\n\
         \n",
    );
    commit(&repo, "main: rewrite the head", 5);

    merge(&repo, "side", 6);

    // Post-merge, so the deletion is decided against the merge's own file.
    write(
        &repo,
        "src/app.c",
        "#include <stdio.h>\n\
         #include <stdlib.h>\n\
         #include <string.h>\n\
         \n\
         static void helper(void)\n\
         {\n\
         }\n\
         \n\
         int main(void)\n\
         {\n\
         \thelper();\n\
         \treturn EXIT_SUCCESS;\n\
         }\n\
         \n",
    );
    commit(&repo, "delete a line", 7);

    // A file with no history before this commit: blame has nowhere to go.
    write(&repo, "late.txt", "late\nlines\n");
    commit(&repo, "add a late file", 8);

    repo.run_git(["commit", "--allow-empty", "-m", "empty"])
        .unwrap();
    repo
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The whole point: every run of lines in a file with a branching, merging
/// history is attributed exactly as `git blame` attributes it — the same
/// commits, the same runs, the same line numbers on both sides.
#[test]
fn test_blame_matches_git_blame() {
    let repo = fixture();
    for path in ["src/app.c", "notes.txt", "dir/sub/deep.txt", "late.txt"] {
        check(&repo, "HEAD", path);
    }
}

/// Blame is answered from the revision asked for, not from HEAD: at each point
/// in the history the file is attributed to whatever had touched it *by then*.
#[test]
fn test_blame_at_older_revisions_matches_git() {
    let repo = fixture();
    for rev in ["HEAD~1", "HEAD~2", "HEAD~3", "side", "HEAD~3^2"] {
        check(&repo, rev, "src/app.c");
    }
}

/// A merge is where blame has a choice to make, and both parents' work has to
/// survive it: the merge commit itself introduced nothing, so it should own no
/// lines, while the two branch tips each keep the lines they wrote.
#[test]
fn test_merge_attributes_lines_to_both_branches() {
    let repo = fixture();
    let groups = check(&repo, "HEAD", "src/app.c");
    let merge = rev_parse(&repo, "HEAD~3");
    assert!(
        !groups.iter().any(|g| g.commit == merge),
        "the merge itself introduced no lines, so it should own none"
    );
    for rev in ["side", "HEAD~4"] {
        let id = rev_parse(&repo, rev);
        assert!(
            groups.iter().any(|g| g.commit == id),
            "{rev}'s lines did not survive the merge"
        );
    }
}

/// Every run carries its commit's first parent — the revision the view's `^`
/// link blames next — and it is the parent git names for the same commit.
#[test]
fn test_groups_carry_their_commits_first_parent() {
    let repo = fixture();
    let groups = check(&repo, "HEAD", "src/app.c");
    assert!(groups.iter().any(|g| g.parent.is_some()));
    for group in &groups {
        // `rev-list --parents -1` prints the commit followed by its parents,
        // which covers a root commit (no parents) without failing.
        let out = repo
            .run_git(["rev-list", "--parents", "-1", &group.commit.to_string()])
            .unwrap();
        let line = String::from_utf8(out).unwrap();
        let expected = line
            .split_whitespace()
            .nth(1)
            .map(|hex| ObjectId::from_hex(hex.as_bytes()).unwrap());
        assert_eq!(group.parent, expected, "wrong parent for {}", group.commit);
    }
}

/// A file added in the newest commit has no ancestry to search: every line is
/// that commit's, in one run.
#[test]
fn test_a_file_with_no_history_is_one_run() {
    let repo = fixture();
    let groups = check(&repo, "HEAD", "late.txt");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].num_lines, 2);
    assert_eq!(groups[0].commit, rev_parse(&repo, "HEAD~1"));
}

/// The progress callback only ever reports lines that are already settled, so
/// what a view paints from a partial result never has to be taken back: every
/// group handed to it is in the finished blame, unchanged.
#[test]
fn test_progress_only_reports_settled_groups() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = commit_at(&repo, "HEAD", &source);
    let seen = RefCell::new(Vec::new());
    let blame = block_on(blame(&head, &source, "src/app.c", |groups| {
        seen.borrow_mut().push(groups.to_vec());
        async {}
    }))
    .unwrap();

    let partials = seen.into_inner();
    assert!(
        partials.len() > 1,
        "the walk reported no intermediate state"
    );
    let mut covered = 0;
    for partial in &partials {
        for group in partial {
            assert!(
                blame.groups.contains(group),
                "a group reported mid-walk is not in the finished blame: {group:?}"
            );
        }
        // Each report is at least as complete as the one before it.
        let lines: usize = partial.iter().map(|g| g.num_lines).sum();
        assert!(lines >= covered, "the walk went backwards");
        covered = lines;
    }
    assert_eq!(
        partials.last().map(Vec::as_slice),
        Some(blame.groups.as_slice()),
        "the last report should be the finished blame"
    );
}

/// Blame reads through the commit-graph's changed-path filters the way the log
/// walk does: with them, the commits that never touched the file cost no tree
/// read at all. Without a graph there is nothing to skip, so the same blame
/// walks every parent's tree — and both answer identically, which is what
/// `check` asserts everywhere above.
#[test]
fn test_bloom_filters_skip_trees_the_walk_would_otherwise_read() {
    let repo = fixture();
    let with_bloom = {
        let source = open_source(&repo, Graph::Bloom);
        let head = commit_at(&repo, "HEAD", &source);
        block_on(blame(&head, &source, "notes.txt", |_| async {}))
            .unwrap()
            .stats
    };
    let without = {
        let source = open_source(&repo, Graph::None);
        let head = commit_at(&repo, "HEAD", &source);
        block_on(blame(&head, &source, "notes.txt", |_| async {}))
            .unwrap()
            .stats
    };
    assert_eq!(without.bloom_skips, 0, "there was no filter to skip on");
    assert!(
        with_bloom.bloom_skips > 0,
        "the filters skipped nothing on a file untouched since the root commit"
    );
    assert!(
        with_bloom.tree_walks < without.tree_walks,
        "the filters saved no tree reads: {} vs {}",
        with_bloom.tree_walks,
        without.tree_walks,
    );
}

/// A path that is not a file in the revision asked for — absent, a directory,
/// or a submodule — is refused rather than blamed as if it were empty.
#[test]
fn test_a_path_that_is_not_a_file_is_refused() {
    let repo = fixture();
    let source = open_source(&repo, Graph::Bloom);
    let head = commit_at(&repo, "HEAD", &source);
    for path in ["no/such/file", "src", "dir/sub", ""] {
        let result = block_on(blame(&head, &source, path, |_| async {}));
        assert!(
            matches!(result, Err(crate::BlameError::NotAFile)),
            "blaming {path:?} should have been refused"
        );
    }
    // A file that exists only in a later revision is equally absent here.
    let older = commit_at(&repo, "HEAD~3", &source);
    assert!(matches!(
        block_on(blame(&older, &source, "late.txt", |_| async {})),
        Err(crate::BlameError::NotAFile)
    ));
}

/// An empty file has no lines to attribute, which is what git says about one
/// too — not an error, just nothing.
#[test]
fn test_an_empty_file_has_no_groups() {
    let repo = fixture();
    write(&repo, "empty.txt", "");
    commit(&repo, "add an empty file", 10);
    assert!(
        git_blame(&repo, "HEAD", "empty.txt").is_empty(),
        "git blamed an empty file as if it had lines"
    );
    let source = open_source(&repo, Graph::Bloom);
    let head = commit_at(&repo, "HEAD", &source);
    let blame = block_on(blame(&head, &source, "empty.txt", |_| async {})).unwrap();
    assert_eq!(blame.num_lines, 0);
    assert!(blame.groups.is_empty());
}

/// A file whose last line has no newline after it still has that line blamed —
/// the count git's `find_line_starts` produces, and the one xdiff splits by.
#[test]
fn test_a_file_without_a_trailing_newline() {
    let repo = fixture();
    write(&repo, "notes.txt", "a\nb\nc\nd");
    commit(&repo, "drop the trailing newline", 11);
    let groups = check(&repo, "HEAD", "notes.txt");
    let covered: usize = groups.iter().map(|g| g.num_lines).sum();
    assert_eq!(covered, 4, "the unterminated last line was not blamed");
}
