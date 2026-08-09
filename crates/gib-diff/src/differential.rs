//! Differential tests for tree diffing, against `git diff-tree --raw`.
//!
//! `diff-tree --raw` reports exactly what a [`TreeDiff`] does: a status, the
//! blob ID on each side, and the path. Rendering both into the same lines lets
//! them be compared as sets, so ordering differences don't matter.
//!
//! The fixture deliberately includes changes that leave a blob's ID untouched —
//! a file gaining the executable bit, and a file replaced by a symlink to the
//! same content — because those are only visible if entry modes are compared
//! as well as object IDs.

use crate::{DiffEntry, TreeDiff, test_support::open_odb, test_support::tree_at};
use futures::executor::block_on;
use gib_hash::ObjectId;
use gib_object::TreeEntryType;
use gib_testkit::{TestRepo, make_basic_repo, make_similar_commits};
use std::collections::BTreeSet;

/// A history whose successive commits add, delete, modify and type-change
/// files across nested directories.
fn mixed_repo() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    let root = test_repo.location.path();

    std::fs::create_dir(root.join("keep")).unwrap();
    std::fs::create_dir(root.join("keep").join("deeper")).unwrap();
    std::fs::create_dir(root.join("gone")).unwrap();
    std::fs::write(root.join("keep").join("stable"), b"unchanged\n").unwrap();
    std::fs::write(root.join("keep").join("changing"), b"before\n").unwrap();
    std::fs::write(root.join("keep").join("deeper").join("nested"), b"deep\n").unwrap();
    std::fs::write(root.join("gone").join("doomed"), b"about to vanish\n").unwrap();
    std::fs::write(root.join("becomes-link"), b"a regular file\n").unwrap();
    std::fs::write(root.join("becomes-executable"), b"#!/bin/sh\n").unwrap();
    // Its content is exactly the symlink target written below, so the two share
    // one blob and only their modes will differ.
    std::fs::write(root.join("link-target-name"), b"keep/stable").unwrap();
    commit(&test_repo, "before");

    std::fs::write(root.join("keep").join("changing"), b"after\n").unwrap();
    std::fs::remove_file(root.join("gone").join("doomed")).unwrap();
    std::fs::remove_dir(root.join("gone")).unwrap();
    std::fs::create_dir(root.join("fresh")).unwrap();
    std::fs::write(root.join("fresh").join("added"), b"brand new\n").unwrap();
    // A file replaced by a symlink with different content: a typechange.
    std::fs::remove_file(root.join("becomes-link")).unwrap();
    std::os::unix::fs::symlink("keep/stable", root.join("becomes-link")).unwrap();
    // A mode-only change: same blob, executable bit added.
    set_mode(root.join("becomes-executable"), 0o755);
    // A typechange that *also* keeps the blob: git stores a symlink's target as
    // a blob, so this file and the symlink replacing it are the same object.
    std::fs::remove_file(root.join("link-target-name")).unwrap();
    std::os::unix::fs::symlink("keep/stable", root.join("link-target-name")).unwrap();
    commit(&test_repo, "after");

    test_repo
}

fn set_mode(path: std::path::PathBuf, mode: u32) {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
}

fn commit(test_repo: &TestRepo, message: &str) {
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit(
            message,
            "a user",
            "an-email-address",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
}

/// Classify a tree entry the way git's raw diff status does: a mode change
/// within one kind is a modification, a change of kind is a typechange.
fn kind(entry_type: TreeEntryType) -> u8 {
    match entry_type {
        TreeEntryType::File | TreeEntryType::Executable => b'b',
        TreeEntryType::Symlink => b'l',
        TreeEntryType::Tree => b't',
        TreeEntryType::Commit => b'c',
    }
}

/// Render a diff entry as `<status> <left-oid> <right-oid> <path>`.
fn render(entry: &DiffEntry<(ObjectId, ObjectId)>) -> String {
    let status = match entry {
        DiffEntry::LeftOnly { .. } => 'D',
        DiffEntry::RightOnly { .. } => 'A',
        DiffEntry::Both {
            left_type,
            right_type,
            ..
        } => {
            if kind(*left_type) == kind(*right_type) {
                'M'
            } else {
                'T'
            }
        }
    };
    let (left, right) = entry.content();
    format!(
        "{status} {left} {right} {}",
        String::from_utf8_lossy(entry.path().as_slice())
    )
}

/// Parse `:<srcmode> <dstmode> <srcsha> <dstsha> <status>\t<path>` into the
/// same rendering.
fn parse_raw_line(line: &str) -> String {
    let line = line.strip_prefix(':').unwrap();
    let (meta, path) = line.split_once('\t').unwrap();
    let meta: Vec<&str> = meta.split(' ').collect();
    let (src, dst, status) = (meta[2], meta[3], meta[4]);
    format!("{status} {src} {dst} {path}")
}

fn check(test_repo: &TestRepo, left_rev: &str, right_rev: &str) {
    let expected: BTreeSet<String> = String::from_utf8(
        test_repo
            .run_git(["diff-tree", "--raw", "-r", left_rev, right_rev])
            .unwrap(),
    )
    .unwrap()
    .lines()
    .map(parse_raw_line)
    .collect();
    assert!(!expected.is_empty(), "{left_rev}..{right_rev} has no diff");

    let odb = open_odb(test_repo);
    let left = tree_at(test_repo, &odb, left_rev);
    let right = tree_at(test_repo, &odb, right_rev);
    let diff = block_on(TreeDiff::new(&odb, &left, &right)).unwrap();
    let actual: BTreeSet<String> = diff.entries().iter().map(render).collect();

    let only_ours: Vec<&String> = actual.difference(&expected).collect();
    let only_gits: Vec<&String> = expected.difference(&actual).collect();
    assert!(
        only_ours.is_empty() && only_gits.is_empty(),
        "{left_rev}..{right_rev}: only in library: {only_ours:?}; only in git: {only_gits:?}"
    );
}

/// A change that leaves the blob alone and only moves the mode is still a
/// change. Asserted directly, because a `check` that compared two empty sets
/// would pass without noticing.
#[test]
fn mode_only_change_is_reported() {
    let test_repo = mixed_repo();
    let odb = open_odb(&test_repo);
    let left = tree_at(&test_repo, &odb, "HEAD~1");
    let right = tree_at(&test_repo, &odb, "HEAD");
    let rendered: Vec<String> = block_on(TreeDiff::new(&odb, &left, &right))
        .unwrap()
        .entries()
        .iter()
        .map(render)
        .collect();

    // Same blob on both sides, reported as a modification: 100644 -> 100755.
    let executable = rendered
        .iter()
        .find(|line| line.ends_with(" becomes-executable"))
        .expect("the chmod'd path is missing from the diff");
    let fields: Vec<&str> = executable.split(' ').collect();
    assert_eq!(fields[0], "M");
    assert_eq!(fields[1], fields[2], "a chmod must not change the blob id");

    // Same blob again, but the mode change crosses from file to symlink, so
    // git calls it a typechange rather than a modification.
    let link = rendered
        .iter()
        .find(|line| line.ends_with(" link-target-name"))
        .expect("the file-to-symlink path is missing from the diff");
    let fields: Vec<&str> = link.split(' ').collect();
    assert_eq!(fields[0], "T");
    assert_eq!(fields[1], fields[2], "the symlink stores the same blob");
}

#[test]
fn diff_loose() {
    let test_repo = mixed_repo();
    check(&test_repo, "HEAD~1", "HEAD");
    // Reversing the arguments must reverse every status.
    check(&test_repo, "HEAD", "HEAD~1");
    // A wider span, crossing the commits that delete files.
    check(&test_repo, "HEAD~4", "HEAD");
}

#[test]
fn diff_packed() {
    let test_repo = mixed_repo();
    test_repo.run_git(["gc"]).unwrap();
    check(&test_repo, "HEAD~1", "HEAD");
    check(&test_repo, "HEAD~4", "HEAD");
}

/// A diff against the empty tree is every file in the repository, added.
#[test]
fn diff_against_empty_tree() {
    let test_repo = mixed_repo();
    // `-w` writes it: git knows the empty tree's hash without storing it, but
    // the odb can only diff a tree it can actually read.
    let empty = test_repo
        .run_git(["hash-object", "-w", "-t", "tree", "/dev/null"])
        .unwrap();
    let empty = String::from_utf8(empty.trim_ascii_end().to_vec()).unwrap();
    check(&test_repo, &empty, "HEAD");
}
