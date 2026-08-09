//! Differential tests for the commit-graph reader, against the host's `git`.
//!
//! The graph is a *cache* of what commit objects already say, so git's own
//! reading of those objects is the oracle: every record must agree with
//! `git log`, and no changed-path Bloom filter may exclude a commit that
//! `git log -- <path>` reports as touching that path.

use crate::{CommitGraph, bloom::path_maybe_changed};
use futures::executor::block_on;
use gib_fs::Directory;
use gib_hash::ObjectId;
use gib_testkit::{TestFileSystem, TestRepo, make_basic_repo, make_similar_commits};
use std::collections::{BTreeMap, BTreeSet};

/// A repository with branching history, nested paths, and a commit-graph
/// carrying changed-path filters.
fn graph_repo() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    let root = test_repo.location.path();
    std::fs::create_dir(root.join("dir")).unwrap();
    for i in 0..5 {
        std::fs::write(
            root.join("dir").join(format!("file{i}")),
            format!("v0 {i}\n"),
        )
        .unwrap();
        test_repo.run_git(["add", "--all"]).unwrap();
        test_repo
            .commit(
                &format!("touch dir/file{i}"),
                "a user",
                "an-email-address",
                "2000-01-01T00:00:00Z",
            )
            .unwrap();
    }
    // A branch and a merge, so the graph has a commit with two parents.
    test_repo
        .run_git(["checkout", "-q", "-b", "side", "HEAD~2"])
        .unwrap();
    std::fs::write(root.join("dir").join("side"), "side\n").unwrap();
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit("side", "a user", "an-email-address", "2000-01-01T00:00:00Z")
        .unwrap();
    test_repo.run_git(["checkout", "-q", "main"]).unwrap();
    test_repo
        .run_git(["merge", "--no-ff", "-m", "merge side", "side"])
        .unwrap();
    test_repo
        .run_git(["commit-graph", "write", "--reachable", "--changed-paths"])
        .unwrap();
    test_repo
}

fn open(test_repo: &TestRepo) -> CommitGraph<TestFileSystem> {
    let objects_dir = block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap();
    block_on(CommitGraph::open(&objects_dir))
        .unwrap()
        .expect("the repo has a commit-graph")
}

fn git_lines(test_repo: &TestRepo, args: &[&str]) -> Vec<String> {
    String::from_utf8(test_repo.run_git(args).unwrap())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Every commit git knows about: id -> (tree, parents, commit time).
fn git_commits(test_repo: &TestRepo) -> BTreeMap<ObjectId, (String, String, i64)> {
    git_lines(test_repo, &["log", "--all", "--format=%H|%T|%P|%ct"])
        .into_iter()
        .map(|line| {
            let fields: Vec<&str> = line.split('|').collect();
            (
                ObjectId::from_hex(fields[0].as_bytes()).unwrap(),
                (
                    fields[1].to_string(),
                    fields[2].to_string(),
                    fields[3].parse().unwrap(),
                ),
            )
        })
        .collect()
}

#[test]
fn every_record_matches_git_log() {
    let test_repo = graph_repo();
    let graph = open(&test_repo);
    let expected = git_commits(&test_repo);
    assert!(expected.len() > 8);
    // `--reachable` covers every commit reachable from any ref, which is what
    // `git log --all` walks.
    assert_eq!(
        usize::try_from(graph.num_commits()).unwrap(),
        expected.len()
    );
    assert!(graph.has_bloom());

    for (id, (tree, parents, commit_time)) in &expected {
        let (_pos, entry) = block_on(graph.lookup(*id))
            .unwrap()
            .unwrap_or_else(|| panic!("{id} missing from the graph"));
        assert_eq!(entry.tree.to_string(), *tree, "tree of {id}");
        let got: Vec<String> = entry.parents.iter().map(ObjectId::to_string).collect();
        assert_eq!(got.join(" "), *parents, "parents of {id}");
        assert_eq!(entry.commit_time, *commit_time, "time of {id}");
    }
    // The fixture must include a merge, or the multi-parent paths go untested.
    assert!(
        expected
            .values()
            .any(|(_, parents, _)| parents.contains(' '))
    );
}

/// A bulk read must return the same records as looking each one up.
#[test]
fn all_records_matches_lookups() {
    let test_repo = graph_repo();
    let graph = open(&test_repo);
    let records = block_on(graph.all_records()).unwrap();
    assert_eq!(records.len(), usize::try_from(graph.num_commits()).unwrap());
    for (id, entry, _bloom) in &records {
        let (_pos, looked_up) = block_on(graph.lookup(*id)).unwrap().unwrap();
        assert_eq!(looked_up.tree, entry.tree);
        assert_eq!(looked_up.parents, entry.parents);
        assert_eq!(looked_up.commit_time, entry.commit_time);
    }
}

/// The Bloom filter may say "maybe" about a path a commit never touched, but
/// never "no" about one it did.
#[test]
fn bloom_filters_have_no_false_negatives() {
    let test_repo = graph_repo();
    let graph = open(&test_repo);
    for path in [
        "a-file",
        "m",
        "t",
        "dir/file0",
        "dir/file4",
        "dir/side",
        "dir",
    ] {
        let touching: BTreeSet<ObjectId> =
            git_lines(&test_repo, &["log", "--all", "--format=%H", "--", path])
                .into_iter()
                .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
                .collect();
        assert!(!touching.is_empty(), "no commit touches {path}");
        for id in touching {
            let (pos, _entry) = block_on(graph.lookup(id)).unwrap().unwrap();
            assert!(
                !block_on(graph.path_unchanged(pos, path.as_bytes())).unwrap(),
                "bloom filter wrongly excluded {path} for {id}"
            );
        }
    }
}

/// A filter that answers "maybe" for a path is only useful if it answers "no"
/// for most others; check the raw filter directly so a degenerate
/// always-maybe filter would be caught.
#[test]
fn bloom_filters_exclude_unrelated_paths() {
    let test_repo = graph_repo();
    let graph = open(&test_repo);
    let settings = graph.bloom_settings().unwrap();
    let mut excluded = 0;
    let mut total = 0;
    for (_id, _entry, filter) in block_on(graph.all_records()).unwrap() {
        let Some(filter) = filter else { continue };
        for i in 0..20 {
            total += 1;
            if !path_maybe_changed(&filter, &settings, format!("no/such/path{i}").as_bytes()) {
                excluded += 1;
            }
        }
    }
    assert!(total > 0, "the graph carries no filters at all");
    // Far better than half in practice; this only rules out a filter that
    // matches everything.
    assert!(
        excluded * 2 > total,
        "filters excluded only {excluded} of {total} unrelated paths"
    );
}

/// A repository without a commit-graph reads as `None`, not an error.
#[test]
fn missing_graph_degrades_to_none() {
    let test_repo = make_basic_repo().unwrap();
    let objects_dir = block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap();
    assert!(
        block_on(CommitGraph::<TestFileSystem>::open(&objects_dir))
            .unwrap()
            .is_none()
    );
}

/// So does a file that exists but is not a commit-graph.
#[test]
fn unrecognised_graph_degrades_to_none() {
    let test_repo = graph_repo();
    let path = test_repo
        .location
        .path()
        .join(".git")
        .join("objects")
        .join("info")
        .join("commit-graph");
    // git writes the graph read-only, so replace rather than overwrite it.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"NOPE\x01\x01\x00\x00").unwrap();
    let objects_dir = block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap();
    assert!(
        block_on(CommitGraph::<TestFileSystem>::open(&objects_dir))
            .unwrap()
            .is_none()
    );
}
