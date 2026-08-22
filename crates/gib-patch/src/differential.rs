//! Differential tests for patch formatting, against `git format-patch`.
//!
//! Each commit in the fixture history is rendered twice — once by this crate,
//! once by `git format-patch -1 --stdout` — and the two are compared byte for
//! byte. git is invoked with the two options that turn off the behaviour this
//! crate deliberately doesn't have (`--no-binary`, `--no-renames`), and told to
//! sign the patch with its own version so the signature lines match; everything
//! else is left at its defaults, including the 80-column diffstat.
//!
//! The history is built to walk the header block's cases as well as the diff's:
//! creations, deletions, a mode-only change, a file that becomes a symlink, a
//! binary file, a file with no newline at its end, a subject too long for one
//! line, a non-ASCII subject and author, and a change wide enough to make the
//! diffstat scale its bars.

use crate::{FileDiff, PatchMeta, Side, diff_file, format_patch};
use futures::executor::block_on;
use gib_diff::{DiffEntry, TreeDiff};
use gib_fs::Directory;
use gib_hash::ObjectId;
use gib_object::{Commit, Object, Tree};
use gib_odb::ObjectDb;
use gib_testkit::{TestFileSystem, TestRepo};
use std::path::Path;

type Odb = ObjectDb<TestFileSystem>;

fn open_odb(test_repo: &TestRepo) -> Odb {
    let objects_dir = block_on(test_repo.git_dir().open_subdir(b"objects")).unwrap();
    block_on(ObjectDb::open(objects_dir, 64 * 1024 * 1024)).unwrap()
}

fn object(odb: &Odb, id: ObjectId) -> Object {
    let raw = block_on(odb.lookup(id)).unwrap().unwrap();
    Object::from_raw(id, raw).unwrap()
}

fn commit_at(test_repo: &TestRepo, odb: &Odb, rev: &str) -> Commit {
    let out = test_repo.run_git(["rev-parse", rev]).unwrap();
    let id = ObjectId::from_hex(out.trim_ascii_end()).unwrap();
    object(odb, id).commit().unwrap()
}

fn tree_of(odb: &Odb, commit: &Commit) -> Tree {
    object(odb, commit.tree()).tree().unwrap()
}

/// The two sides of a diff entry, absent where the file did not exist.
fn sides(entry: &DiffEntry<(ObjectId, ObjectId)>) -> (Option<Side>, Option<Side>) {
    match entry {
        DiffEntry::LeftOnly {
            entry_type,
            content: (old, _),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *entry_type,
            }),
            None,
        ),
        DiffEntry::RightOnly {
            entry_type,
            content: (_, new),
            ..
        } => (
            None,
            Some(Side {
                id: *new,
                entry_type: *entry_type,
            }),
        ),
        DiffEntry::Both {
            left_type,
            right_type,
            content: (old, new),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *left_type,
            }),
            Some(Side {
                id: *new,
                entry_type: *right_type,
            }),
        ),
    }
}

fn blob(odb: &Odb, side: Option<Side>) -> Vec<u8> {
    match side {
        Some(side) => object(odb, side.id).blob().unwrap().data_owned(),
        None => Vec::new(),
    }
}

/// This crate's patch for `rev`, diffed against its first parent.
fn our_patch(test_repo: &TestRepo, odb: &Odb, rev: &str, generator: &str) -> String {
    let commit = commit_at(test_repo, odb, rev);
    let parent = object(odb, commit.parents()[0]).commit().unwrap();
    let diff = block_on(TreeDiff::new(
        odb,
        &tree_of(odb, &parent),
        &tree_of(odb, &commit),
    ))
    .unwrap();

    let files: Vec<FileDiff> = diff
        .entries()
        .iter()
        .map(|entry| {
            let (old, new) = sides(entry);
            diff_file(
                &String::from_utf8_lossy(entry.path().as_slice()),
                old,
                new,
                &blob(odb, old),
                &blob(odb, new),
            )
        })
        .collect();

    format_patch(&PatchMeta::from_commit(&commit), &files, generator)
}

/// git's own patch for `rev`, with the two features this crate does not have
/// switched off.
fn git_patch(test_repo: &TestRepo, rev: &str) -> String {
    let out = test_repo
        .run_git([
            "format-patch",
            "-1",
            "--stdout",
            "--no-binary",
            "--no-renames",
            rev,
        ])
        .unwrap();
    String::from_utf8(out).unwrap()
}

/// The version git will sign its patches with, so ours can be signed the same.
fn git_generator(test_repo: &TestRepo) -> String {
    let out = String::from_utf8(test_repo.run_git(["--version"]).unwrap()).unwrap();
    out.trim().rsplit(' ').next().unwrap().to_string()
}

fn write(test_repo: &TestRepo, path: impl AsRef<Path>, contents: &[u8]) {
    let path = test_repo.location.path().join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn commit(test_repo: &TestRepo, message: &str) {
    commit_as(test_repo, message, "a user", "an-email-address");
}

fn commit_as(test_repo: &TestRepo, message: &str, name: &str, email: &str) {
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit(message, name, email, "2000-01-01T00:00:00Z")
        .unwrap();
}

/// A history covering every shape of change the patch writer has a branch for.
/// Returns the repository and the revisions to compare, oldest first.
fn fixture() -> TestRepo {
    let test_repo = TestRepo::new().unwrap();
    write(&test_repo, "kept.txt", b"alpha\nbeta\ngamma\n");
    write(&test_repo, "doomed.txt", b"here today\n");
    write(&test_repo, "script.sh", b"#!/bin/sh\necho hi\n");
    write(&test_repo, "becomes-link", b"a regular file\n");
    commit(&test_repo, "Add the starting files");

    // A modification and a creation, the two commonest cases, plus a file
    // whose last line has no terminator.
    write(&test_repo, "kept.txt", b"alpha\nbeta changed\ngamma\n");
    write(&test_repo, "no-newline.txt", b"no trailing newline");
    commit(&test_repo, "Change one file and add another");

    // A deletion, and a mode change that leaves the blob alone.
    std::fs::remove_file(test_repo.location.path().join("doomed.txt")).unwrap();
    std::fs::set_permissions(
        test_repo.location.path().join("script.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    commit(&test_repo, "Remove a file and make another executable");

    // A file replaced by a symlink: same path, different type.
    std::fs::remove_file(test_repo.location.path().join("becomes-link")).unwrap();
    std::os::unix::fs::symlink("kept.txt", test_repo.location.path().join("becomes-link")).unwrap();
    commit(&test_repo, "Turn a file into a symlink");

    // A binary file, added and then changed: reported as differing, never
    // spelled out.
    write(&test_repo, "data.bin", b"\x00\x01\x02binary\n");
    commit(&test_repo, "Add a binary file");
    write(&test_repo, "data.bin", b"\x00\x03different binary\n");
    commit(&test_repo, "Change the binary file");

    // A subject too long for one header line, and a body of several paragraphs.
    write(
        &test_repo,
        "kept.txt",
        b"alpha\nbeta changed again\ngamma\n",
    );
    commit(
        &test_repo,
        "A subject line long enough that git has to fold it across two header \
         lines instead of one\n\nThe body explains why.\n\nAt some length, in \
         more than one paragraph.\n",
    );

    // A non-ASCII subject, body and author name, which are encoded three
    // different ways.
    write(&test_repo, "kept.txt", "alpha\nbêta\n".as_bytes());
    commit_as(
        &test_repo,
        "Rename the variable to bêta ✓\n\nBecause naïve names are clearer.\n",
        "Zoë Ünïcode",
        "zoe@example.org",
    );

    // A binary file and a text file changed together, so that the number
    // column has to hold both a count and "Bin".
    write(
        &test_repo,
        "data.bin",
        b"\x00\x04binary again, and longer\n",
    );
    write(&test_repo, "kept.txt", b"alpha\nbeta\ngamma\ndelta\n");
    commit_as(
        &test_repo,
        "Change a binary and a text file at once",
        // A name needing both encoding and the extra escapes a mail phrase
        // reserves for the punctuation in it.
        "Zoë O'Brien, Jr.",
        "zoe@example.org",
    );

    // Changes of wildly different sizes in one commit, so the bars have to be
    // scaled down and a one-line change still has to show.
    write(&test_repo, "kept.txt", "line\n".repeat(400).as_bytes());
    write(&test_repo, "no-newline.txt", b"no trailing newline at all");
    commit(&test_repo, "Rewrite one file and touch another");

    // A change wide enough that the diffstat has to scale its bars, under a
    // path long enough that the name column has to elide it.
    let long_path = "deeply/nested/directory/structure/with/a/very/long/name/inside.txt";
    write(&test_repo, long_path, "one\n".repeat(300).as_bytes());
    write(&test_repo, "kept.txt", b"alpha\nbeta\n");
    commit(&test_repo, "Add a large file under a long path");

    test_repo
}

fn check(test_repo: &TestRepo, odb: &Odb, generator: &str, rev: &str) {
    let subject = String::from_utf8(
        test_repo
            .run_git(["log", "-1", "--format=%s", rev])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        our_patch(test_repo, odb, rev, generator),
        git_patch(test_repo, rev),
        "patch for {rev} ({}) differs from git's",
        subject.trim()
    );
}

/// Every commit in the fixture but the root one, which has no parent to diff
/// against and which the caller of this crate never asks for a patch of.
fn revisions(test_repo: &TestRepo) -> Vec<String> {
    String::from_utf8(
        test_repo
            .run_git(["rev-list", "--reverse", "HEAD"])
            .unwrap(),
    )
    .unwrap()
    .lines()
    .skip(1)
    .map(str::to_string)
    .collect()
}

#[test]
fn patches_match_git_loose() {
    let test_repo = fixture();
    let generator = git_generator(&test_repo);
    let odb = open_odb(&test_repo);
    let revisions = revisions(&test_repo);
    assert!(revisions.len() > 5, "the fixture history is too short");
    for rev in revisions {
        check(&test_repo, &odb, &generator, &rev);
    }
}

#[test]
fn patches_match_git_packed() {
    let test_repo = fixture();
    test_repo.run_git(["gc"]).unwrap();
    let generator = git_generator(&test_repo);
    let odb = open_odb(&test_repo);
    for rev in revisions(&test_repo) {
        check(&test_repo, &odb, &generator, &rev);
    }
}
