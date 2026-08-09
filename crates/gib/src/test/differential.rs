//! Differential tests against the host's `git` binary.
//!
//! Every assertion here compares what the library reports against what the
//! installed `git` CLI reports for the same repository, so the suite pins
//! *behaviour* rather than any particular internal arrangement. It deliberately
//! uses only the public API, which is what makes it a safety net for
//! refactoring: the code underneath may be rearranged freely and these tests
//! keep asserting the same externally-observable facts.
//!
//! The oracles are git's own output (`cat-file`, `for-each-ref`, `log`,
//! `ls-tree`, `diff-tree`, `rev-parse`), never hashes or encodings pinned into
//! this file, so the suite is not tied to a particular git version.

use crate::{
    Repo,
    object::{Object, ObjectId, ObjectIdPrefix, ObjectType, PrefixResolution, Tree, TreeEntryType},
    prelude::*,
    reference::{RefName, RefTarget},
    test::open_test_repo,
};
use futures::executor::block_on;
use gib_testkit::{TestFileSystem, TestRepo, make_basic_repo, make_file, make_similar_commits};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
};

// ---------------------------------------------------------------------------
// Repository shapes
// ---------------------------------------------------------------------------

/// How many distinct blobs each bulk commit adds.
///
/// The corpus is large enough that some pair of object IDs is guaranteed in
/// practice to share a four-character prefix, which is what makes the
/// abbreviation-ambiguity test below possible. Object content is fixed, so the
/// resulting IDs — and therefore that collision — are the same on every run.
const BULK_FILES_PER_COMMIT: usize = 400;
const BULK_COMMITS: usize = 3;

/// Build the corpus every shape shares: a few small commits from the existing
/// helpers, nested directories with mixed entry types, and enough distinct
/// blobs to make pack indexes and abbreviations interesting.
fn populated_repo() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    add_nested_tree(&test_repo);
    for commit in 0..BULK_COMMITS {
        add_bulk_objects(&test_repo, commit);
    }
    test_repo
}

/// Nested directories plus an executable and a symlink, so tree walks see more
/// than one level and more than one entry type.
fn add_nested_tree(test_repo: &TestRepo) {
    let root = test_repo.location.path();
    fs::create_dir(root.join("dir")).unwrap();
    fs::create_dir(root.join("dir").join("nested")).unwrap();
    write_file(root, "dir/one", b"the first file\n");
    write_file(root, "dir/nested/two", b"the second file\n");
    write_file(root, "dir/nested/script.sh", b"#!/bin/sh\necho hi\n");
    fs::set_permissions(
        root.join("dir").join("nested").join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("one", root.join("dir").join("a-link")).unwrap();
    commit_all(test_repo, "nested tree");
}

/// A commit adding [`BULK_FILES_PER_COMMIT`] files, each with unique content so
/// each is a distinct blob.
fn add_bulk_objects(test_repo: &TestRepo, commit: usize) {
    let root = test_repo.location.path();
    let dir = format!("bulk{commit}");
    fs::create_dir(root.join(&dir)).unwrap();
    for i in 0..BULK_FILES_PER_COMMIT {
        write_file(
            root,
            &format!("{dir}/file{i}"),
            format!("differential test object {commit} {i}\n").as_bytes(),
        );
    }
    commit_all(test_repo, &format!("bulk commit {commit}"));
}

fn write_file(root: &Path, path: &str, content: &[u8]) {
    fs::write(root.join(path), content).unwrap();
}

fn commit_all(test_repo: &TestRepo, message: &str) {
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

/// Shape (a): every object loose, every ref loose.
fn loose_repo() -> TestRepo {
    populated_repo()
}

/// Shape (b): a single pack plus `packed-refs`, as a gc'd repository has.
fn packed_repo() -> TestRepo {
    let test_repo = populated_repo();
    test_repo.run_git(["gc"]).unwrap();
    test_repo.run_git(["pack-refs", "--all"]).unwrap();
    test_repo
}

/// Shape (c): repacked with an aggressive window so most objects are stored as
/// deltas, exercising delta-chain reconstruction rather than whole objects.
fn delta_repo() -> TestRepo {
    let test_repo = populated_repo();
    test_repo
        .run_git(["repack", "-a", "-d", "-f", "--depth=50", "--window=250"])
        .unwrap();
    test_repo
}

/// Shape (d): shape (b) plus a commit-graph carrying changed-path filters.
fn commit_graph_repo() -> TestRepo {
    let test_repo = packed_repo();
    test_repo
        .run_git(["commit-graph", "write", "--changed-paths"])
        .unwrap();
    test_repo
}

// ---------------------------------------------------------------------------
// Small helpers for talking to git
// ---------------------------------------------------------------------------

fn git(test_repo: &TestRepo, args: &[&str]) -> Vec<u8> {
    test_repo.run_git(args).unwrap()
}

/// Run git and split its output into lines, dropping the trailing empty one.
fn git_lines(test_repo: &TestRepo, args: &[&str]) -> Vec<Vec<u8>> {
    git(test_repo, args)
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

fn git_oid(test_repo: &TestRepo, rev: &str) -> ObjectId {
    let out = git(test_repo, &["rev-parse", rev]);
    ObjectId::from_hex(out.trim_ascii_end()).unwrap()
}

fn open(test_repo: &TestRepo) -> Repo<TestFileSystem> {
    open_test_repo(test_repo)
}

fn type_name(object_type: ObjectType) -> &'static str {
    match object_type {
        ObjectType::Commit => "commit",
        ObjectType::Tree => "tree",
        ObjectType::Blob => "blob",
        ObjectType::Tag => "tag",
    }
}

fn parse_type(name: &[u8]) -> ObjectType {
    match name {
        b"commit" => ObjectType::Commit,
        b"tree" => ObjectType::Tree,
        b"blob" => ObjectType::Blob,
        b"tag" => ObjectType::Tag,
        other => panic!("unknown object type {}", String::from_utf8_lossy(other)),
    }
}

// ---------------------------------------------------------------------------
// All-objects byte comparison
// ---------------------------------------------------------------------------

/// Every object git knows about, with the type git reports for it.
fn all_object_ids(test_repo: &TestRepo) -> Vec<(ObjectId, ObjectType)> {
    git_lines(
        test_repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    )
    .into_iter()
    .map(|line| {
        let mut fields = line.splitn(2, |&b| b == b' ');
        let id = ObjectId::from_hex(fields.next().unwrap()).unwrap();
        let object_type = parse_type(fields.next().unwrap());
        (id, object_type)
    })
    .collect()
}

/// Every object git knows about, *with its bytes*, read in a single `git`
/// invocation. `--batch` emits `<oid> <type> <size>\n` followed by exactly
/// `<size>` bytes and a newline, for each object in turn.
fn all_objects_with_bodies(test_repo: &TestRepo) -> Vec<(ObjectId, ObjectType, Vec<u8>)> {
    let stream = git(test_repo, &["cat-file", "--batch", "--batch-all-objects"]);
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < stream.len() {
        let eol = pos + stream[pos..].iter().position(|&b| b == b'\n').unwrap();
        let mut header = stream[pos..eol].splitn(3, |&b| b == b' ');
        let id = ObjectId::from_hex(header.next().unwrap()).unwrap();
        let object_type = parse_type(header.next().unwrap());
        let size: usize = str::from_utf8(header.next().unwrap())
            .unwrap()
            .parse()
            .unwrap();
        let body_start = eol + 1;
        let body = stream[body_start..body_start + size].to_vec();
        // git writes a newline after the body that is not part of the object.
        assert_eq!(stream[body_start + size], b'\n');
        pos = body_start + size + 1;
        out.push((id, object_type, body));
    }
    out
}

/// The keystone test: read every object in the repository through the library
/// and byte-compare it with `git cat-file`. One assertion covers loose reading,
/// pack index lookup, delta reconstruction and object parsing at once.
fn check_all_objects(test_repo: &TestRepo) {
    let repo = open(test_repo);
    let objects = all_objects_with_bodies(test_repo);
    // Guard against the oracle silently returning nothing.
    assert!(
        objects.len() > BULK_FILES_PER_COMMIT,
        "expected a repo with many objects, got {}",
        objects.len()
    );
    for (id, object_type, expected) in objects {
        let raw = block_on(repo.lookup_raw(id))
            .unwrap()
            .unwrap_or_else(|| panic!("object {id} not found by the library"));
        assert_eq!(raw.object_type, object_type, "type mismatch for {id}");
        assert_eq!(raw.body, expected, "body mismatch for {id}");

        // The same object must also parse, and the parsed form must retain the
        // bytes git reported.
        let parsed = block_on(repo.lookup_object(id)).unwrap();
        assert_eq!(parsed.id(), id);
        assert_eq!(parsed.object_type(), object_type);
        let body: &[u8] = match &parsed {
            Object::Commit(c) => c.body(),
            Object::Tree(t) => t.body(),
            Object::Tag(t) => t.body(),
            Object::Blob(b) => b.data(),
        };
        assert_eq!(body, expected, "parsed body mismatch for {id}");
    }
}

#[test]
fn all_objects_loose() {
    check_all_objects(&loose_repo());
}

#[test]
fn all_objects_packed() {
    check_all_objects(&packed_repo());
}

#[test]
fn all_objects_deltified() {
    check_all_objects(&delta_repo());
}

/// The corpus must contain all four object types, or the byte comparison above
/// is quietly testing less than it looks like it is.
#[test]
fn corpus_covers_every_object_type() {
    let test_repo = packed_repo();
    let types: BTreeSet<&'static str> = all_object_ids(&test_repo)
        .into_iter()
        .map(|(_, object_type)| type_name(object_type))
        .collect();
    assert_eq!(
        types,
        ["blob", "commit", "tag", "tree"].into_iter().collect()
    );
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

/// `refs/`-prefixed name of a [`RefName`], to compare against git's own naming.
fn full_ref_name(name: &RefName) -> Vec<u8> {
    match name {
        RefName::Head => b"HEAD".to_vec(),
        RefName::Ref(path) => {
            let mut out = b"refs/".to_vec();
            out.extend_from_slice(path);
            out
        }
    }
}

fn check_refs(test_repo: &TestRepo) {
    let repo = open(test_repo);

    let expected: BTreeMap<Vec<u8>, ObjectId> = git_lines(
        test_repo,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .into_iter()
    .map(|line| {
        let mut fields = line.splitn(2, |&b| b == b' ');
        let name = fields.next().unwrap().to_vec();
        let id = ObjectId::from_hex(fields.next().unwrap()).unwrap();
        (name, id)
    })
    .collect();
    assert!(!expected.is_empty());

    let all_refs = block_on(repo.all_refs()).unwrap();
    let actual: BTreeMap<Vec<u8>, ObjectId> = all_refs
        .iter()
        .map(|(name, entry)| {
            assert_ne!(*name, RefName::Head, "all_refs must not include HEAD");
            (full_ref_name(name), entry.target())
        })
        .collect();
    assert_eq!(actual, expected);

    // Where a peeled target was recorded, it must be the commit git derefs to.
    for (name, entry) in &all_refs {
        if let Some(peeled) = entry.peeled() {
            let full = String::from_utf8(full_ref_name(name)).unwrap();
            assert_eq!(
                peeled,
                git_oid(test_repo, &format!("{full}^{{}}")),
                "bad peeled target for {full}"
            );
        }
    }

    // ref_names() is the same set of names, plus HEAD.
    let mut expected_names: BTreeSet<Vec<u8>> = expected.keys().cloned().collect();
    expected_names.insert(b"HEAD".to_vec());
    let actual_names: BTreeSet<Vec<u8>> = block_on(repo.ref_names())
        .unwrap()
        .iter()
        .map(full_ref_name)
        .collect();
    assert_eq!(actual_names, expected_names);

    // HEAD is symbolic, and points where git says it does.
    let head = block_on(repo.head()).unwrap();
    let symbolic = git(test_repo, &["symbolic-ref", "HEAD"]);
    let symbolic = symbolic.trim_ascii_end().strip_prefix(b"refs/").unwrap();
    assert_eq!(
        head.target(),
        &RefTarget::Symbolic(RefName::Ref(symbolic.to_vec()))
    );
    assert_eq!(
        block_on(head.resolve_object_id(&repo)).unwrap(),
        git_oid(test_repo, "HEAD")
    );
}

#[test]
fn refs_loose() {
    check_refs(&loose_repo());
}

#[test]
fn refs_packed() {
    check_refs(&packed_repo());
}

/// A tag object read through the library reports the same tagger, target and
/// message as `git for-each-ref`.
#[test]
fn annotated_tag_fields() {
    let test_repo = packed_repo();
    let repo = open(&test_repo);
    let tag_id = git_oid(&test_repo, "refs/tags/a-fat-tag");
    let tag = block_on(repo.lookup_object(tag_id)).unwrap().tag().unwrap();

    let field = |format: &str| {
        let out = git(
            &test_repo,
            &[
                "for-each-ref",
                &format!("--format={format}"),
                "refs/tags/a-fat-tag",
            ],
        );
        out.trim_ascii_end().to_vec()
    };
    assert_eq!(tag.name(), field("%(refname:strip=2)"));
    assert_eq!(tag.tagger_name().unwrap(), field("%(taggername)"));
    assert_eq!(tag.tagger_email().unwrap(), field("%(taggeremail:trim)"));
    assert_eq!(tag.target(), git_oid(&test_repo, "refs/tags/a-fat-tag^{}"));
    assert_eq!(tag.tag_type(), ObjectType::Commit);
    assert_eq!(
        tag.date().unwrap().timestamp().as_second(),
        str::from_utf8(&field("%(taggerdate:unix)"))
            .unwrap()
            .parse::<i64>()
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// History walk
// ---------------------------------------------------------------------------

/// One `git log` line: commit, tree, parents, author time, commit time.
struct LogEntry {
    id: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author_time: i64,
    commit_time: i64,
}

fn git_log(test_repo: &TestRepo) -> Vec<LogEntry> {
    git_lines(test_repo, &["log", "--format=%H %T %P|%at|%ct"])
        .into_iter()
        .map(|line| {
            let mut sections = line.split(|&b| b == b'|');
            let oids = sections.next().unwrap();
            let number = |field: &[u8]| str::from_utf8(field).unwrap().parse::<i64>().unwrap();
            let author_time = number(sections.next().unwrap());
            let commit_time = number(sections.next().unwrap());
            let mut oids = oids
                .split(|&b| b == b' ')
                .filter(|field| !field.is_empty())
                .map(|field| ObjectId::from_hex(field).unwrap());
            LogEntry {
                id: oids.next().unwrap(),
                tree: oids.next().unwrap(),
                parents: oids.collect(),
                author_time,
                commit_time,
            }
        })
        .collect()
}

/// Walk parents from HEAD through the library and compare against `git log`.
/// The test repositories have linear history, so following the first parent
/// visits commits in exactly git's order.
fn check_history(test_repo: &TestRepo) {
    let repo = open(test_repo);
    let expected = git_log(test_repo);
    assert!(expected.len() >= 4);

    let head = block_on(repo.head()).unwrap();
    let mut current = Some(block_on(head.peel_to_commit(&repo)).unwrap().unwrap());
    for entry in &expected {
        let commit = current.take().expect("history ended before git's did");
        assert_eq!(commit.id(), entry.id);
        assert_eq!(commit.tree(), entry.tree);
        assert_eq!(commit.parents(), entry.parents.as_slice());
        assert_eq!(
            commit.author_date().timestamp().as_second(),
            entry.author_time
        );
        assert_eq!(
            commit.commit_date().timestamp().as_second(),
            entry.commit_time
        );
        if let Some(parent) = entry.parents.first() {
            let parents = block_on(commit.lookup_parents(&repo)).unwrap();
            assert_eq!(parents.len(), entry.parents.len());
            current = Some(parents.into_iter().next().unwrap());
            assert_eq!(current.as_ref().unwrap().id(), *parent);
        }
    }
    assert!(current.is_none(), "library found more history than git did");
}

#[test]
fn history_loose() {
    check_history(&loose_repo());
}

#[test]
fn history_packed() {
    check_history(&packed_repo());
}

#[test]
fn history_with_commit_graph() {
    let test_repo = commit_graph_repo();
    check_history(&test_repo);

    // Also walk the same history through the commit-graph, which records the
    // tree, parents and commit time without reading a commit object at all.
    let repo = open(&test_repo);
    let graph = repo.commit_graph().expect("commit-graph should be usable");
    let expected = git_log(&test_repo);
    // The graph covers every reachable commit, which includes the stash's, so
    // it is a superset of HEAD's history.
    assert!(usize::try_from(graph.num_commits()).unwrap() >= expected.len());
    assert!(graph.has_bloom());
    for entry in &expected {
        let (_pos, record) = block_on(graph.lookup(entry.id))
            .unwrap()
            .unwrap_or_else(|| panic!("commit {} missing from the graph", entry.id));
        assert_eq!(record.tree, entry.tree);
        assert_eq!(record.parents, entry.parents);
        assert_eq!(record.commit_time, entry.commit_time);
    }

    // A bulk read of the graph must agree with the per-commit lookups.
    let records = block_on(graph.all_records()).unwrap();
    assert_eq!(records.len(), usize::try_from(graph.num_commits()).unwrap());
    let by_id: BTreeMap<ObjectId, i64> = records
        .iter()
        .map(|(id, entry, _)| (*id, entry.commit_time))
        .collect();
    for entry in &expected {
        assert_eq!(by_id.get(&entry.id), Some(&entry.commit_time));
    }
}

/// A changed-path Bloom filter may report a false positive but never a false
/// negative: every commit git says touched a path must be reported as possibly
/// having changed it.
#[test]
fn commit_graph_bloom_has_no_false_negatives() {
    let test_repo = commit_graph_repo();
    let repo = open(&test_repo);
    let graph = repo.commit_graph().unwrap();
    for path in ["m", "t", "a-file", "dir/one", "bulk0/file0", "bulk2/file7"] {
        let touching: BTreeSet<ObjectId> =
            git_lines(&test_repo, &["log", "--format=%H", "--", path])
                .into_iter()
                .map(|line| ObjectId::from_hex(&line).unwrap())
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

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

fn mode_of(entry_type: TreeEntryType) -> &'static str {
    match entry_type {
        TreeEntryType::File => "100644",
        TreeEntryType::Executable => "100755",
        TreeEntryType::Symlink => "120000",
        TreeEntryType::Tree => "040000",
        TreeEntryType::Commit => "160000",
    }
}

fn entry_type_name(entry_type: TreeEntryType) -> &'static str {
    match entry_type {
        TreeEntryType::File | TreeEntryType::Executable | TreeEntryType::Symlink => "blob",
        TreeEntryType::Tree => "tree",
        TreeEntryType::Commit => "commit",
    }
}

/// Recursively flatten a tree into `git ls-tree -r -t` formatted lines.
fn flatten_tree(
    repo: &Repo<TestFileSystem>,
    tree: &Tree,
    prefix: &[u8],
    out: &mut BTreeSet<Vec<u8>>,
) {
    for entry in tree.entries() {
        let mut path = prefix.to_vec();
        path.extend_from_slice(entry.name());
        let mut line = Vec::new();
        line.extend_from_slice(mode_of(entry.entry_type()).as_bytes());
        line.push(b' ');
        line.extend_from_slice(entry_type_name(entry.entry_type()).as_bytes());
        line.push(b' ');
        line.extend_from_slice(entry.id().to_string().as_bytes());
        line.push(b'\t');
        line.extend_from_slice(&path);
        out.insert(line);
        if entry.entry_type() == TreeEntryType::Tree {
            let subtree = block_on(entry.lookup(repo))
                .unwrap()
                .unwrap()
                .tree()
                .unwrap();
            path.push(b'/');
            flatten_tree(repo, &subtree, &path, out);
        }
    }
}

fn check_trees(test_repo: &TestRepo) {
    let repo = open(test_repo);
    let expected: BTreeSet<Vec<u8>> = git_lines(test_repo, &["ls-tree", "-r", "-t", "HEAD"])
        .into_iter()
        .collect();
    assert!(!expected.is_empty());

    let head = block_on(repo.head()).unwrap();
    let tree = block_on(head.peel_to_tree(&repo)).unwrap().unwrap();
    let mut actual = BTreeSet::new();
    flatten_tree(&repo, &tree, b"", &mut actual);
    assert_eq!(actual, expected);
}

#[test]
fn trees_loose() {
    check_trees(&loose_repo());
}

#[test]
fn trees_packed() {
    check_trees(&packed_repo());
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

mod diff_tests {
    use super::*;
    use crate::diff::DiffEntry;

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

    /// Render a diff entry as `<status> <left-oid> <right-oid> <path>`, the same
    /// facts `git diff-tree --raw -r` reports. Rendering both sides as text
    /// keeps a failing comparison readable.
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
    fn parse_raw_line(line: &[u8]) -> String {
        let line = str::from_utf8(line.strip_prefix(b":").unwrap()).unwrap();
        let (meta, path) = line.split_once('\t').unwrap();
        let meta: Vec<&str> = meta.split(' ').collect();
        let (src, dst, status) = (meta[2], meta[3], meta[4]);
        format!("{status} {src} {dst} {path}")
    }

    fn tree_of(repo: &Repo<TestFileSystem>, id: ObjectId) -> Tree {
        let object = block_on(repo.lookup_object(id)).unwrap();
        block_on(object.peel_to_tree(repo)).unwrap().unwrap()
    }

    fn check_diff(test_repo: &TestRepo, left_rev: &str, right_rev: &str) {
        let repo = open(test_repo);
        let expected: BTreeSet<String> = git_lines(
            test_repo,
            &["diff-tree", "--raw", "-r", left_rev, right_rev],
        )
        .into_iter()
        .map(|line| parse_raw_line(&line))
        .collect();
        assert!(!expected.is_empty());

        let left = tree_of(&repo, git_oid(test_repo, left_rev));
        let right = tree_of(&repo, git_oid(test_repo, right_rev));
        let diff = block_on(repo.tree_diff(&left, &right)).unwrap();
        let actual: BTreeSet<String> = diff.entries().iter().map(render).collect();
        let only_ours: Vec<&String> = actual.difference(&expected).collect();
        let only_gits: Vec<&String> = expected.difference(&actual).collect();
        assert!(
            only_ours.is_empty() && only_gits.is_empty(),
            "{left_rev}..{right_rev}: only in library: {:?}; only in git: {:?}",
            &only_ours[..only_ours.len().min(5)],
            &only_gits[..only_gits.len().min(5)],
        );
    }

    /// A commit that adds, deletes, modifies and type-changes files across
    /// nested directories, so one diff covers every raw status.
    fn add_mixed_change(test_repo: &TestRepo) {
        let root = test_repo.location.path();
        fs::create_dir(root.join("gone")).unwrap();
        write_file(root, "gone/doomed", b"about to vanish\n");
        write_file(root, "dir/one", b"changed content\n");
        // A symlink where dir/nested/two was: a typechange rather than an edit.
        fs::remove_file(root.join("dir").join("nested").join("two")).unwrap();
        std::os::unix::fs::symlink("one", root.join("dir").join("nested").join("two")).unwrap();
        // Note: a *mode-only* change (e.g. 100755 -> 100644 with identical
        // content) is deliberately not part of this fixture. `TreeDiff` reports
        // paths whose object IDs differ, so it does not see one, while
        // `git diff-tree` reports it as a modification. That is a standing
        // scope limitation of the diff API, not something this suite pins.
        commit_all(test_repo, "before");

        fs::remove_file(root.join("gone").join("doomed")).unwrap();
        fs::remove_dir(root.join("gone")).unwrap();
        fs::create_dir(root.join("fresh")).unwrap();
        write_file(root, "fresh/added", b"brand new\n");
        write_file(root, "dir/one", b"changed again\n");
        commit_all(test_repo, "after");
    }

    fn mixed_repo() -> TestRepo {
        let test_repo = populated_repo();
        add_mixed_change(&test_repo);
        test_repo
    }

    #[test]
    fn diff_loose() {
        let test_repo = mixed_repo();
        check_diff(&test_repo, "HEAD~1", "HEAD");
        check_diff(&test_repo, "HEAD", "HEAD~1");
        check_diff(&test_repo, "HEAD~2", "HEAD");
    }

    #[test]
    fn diff_packed() {
        let test_repo = mixed_repo();
        test_repo.run_git(["gc"]).unwrap();
        check_diff(&test_repo, "HEAD~1", "HEAD");
        check_diff(&test_repo, "HEAD~3", "HEAD");
    }
}

// ---------------------------------------------------------------------------
// Prefix resolution
// ---------------------------------------------------------------------------

/// Everything git thinks an abbreviation could name.
fn disambiguate(test_repo: &TestRepo, prefix: &str) -> Vec<ObjectId> {
    git_lines(
        test_repo,
        &["rev-parse", &format!("--disambiguate={prefix}")],
    )
    .into_iter()
    .map(|line| ObjectId::from_hex(&line).unwrap())
    .collect()
}

/// The longest prefix (in hex characters) shared by two distinct object IDs.
/// IDs are sorted, so only adjacent pairs need comparing.
fn longest_shared_prefix(ids: &[ObjectId]) -> String {
    let mut hexes: Vec<String> = ids.iter().map(ObjectId::to_string).collect();
    hexes.sort_unstable();
    let mut best = String::new();
    for pair in hexes.windows(2) {
        let shared = pair[0]
            .chars()
            .zip(pair[1].chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared > best.len() {
            best = pair[0][..shared].to_string();
        }
    }
    best
}

fn check_prefixes(test_repo: &TestRepo) {
    let repo = open(test_repo);
    let objects = all_object_ids(test_repo);
    let ids: Vec<ObjectId> = objects.iter().map(|(id, _)| *id).collect();

    // Abbreviations of real objects, compared against git's own expansion.
    for id in ids.iter().step_by(37) {
        let hex = id.to_string();
        let prefix = &hex[..6];
        let candidates = disambiguate(test_repo, prefix);
        let expected = match candidates.len() {
            0 => PrefixResolution::NotFound,
            1 => PrefixResolution::Found(candidates[0]),
            _ => PrefixResolution::Ambiguous,
        };
        let parsed = ObjectIdPrefix::from_hex(prefix.as_bytes()).unwrap();
        assert_eq!(
            block_on(repo.resolve_prefix(&parsed)).unwrap(),
            expected,
            "prefix {prefix} resolved wrongly"
        );
    }

    // A prefix short enough to be shared: git refuses to guess, and so must we.
    let shared = longest_shared_prefix(&ids);
    assert!(
        shared.len() >= 4,
        "corpus has no abbreviation shared by two objects (longest is {shared:?})"
    );
    assert!(disambiguate(test_repo, &shared).len() > 1);
    let parsed = ObjectIdPrefix::from_hex(shared.as_bytes()).unwrap();
    assert_eq!(
        block_on(repo.resolve_prefix(&parsed)).unwrap(),
        PrefixResolution::Ambiguous,
        "prefix {shared} should be ambiguous"
    );

    // A prefix no object has resolves to nothing.
    let missing = ObjectIdPrefix::from_hex(b"ffffffffffffffffffff").unwrap();
    assert_eq!(
        block_on(repo.resolve_prefix(&missing)).unwrap(),
        PrefixResolution::NotFound
    );
}

#[test]
fn prefixes_packed() {
    check_prefixes(&packed_repo());
}

#[test]
fn prefixes_deltified() {
    check_prefixes(&delta_repo());
}

/// A repository built entirely by the helpers still resolves the objects the
/// existing suite's fixtures rely on, so the corpus above is not the only shape
/// covered.
#[test]
fn basic_repo_objects() {
    let test_repo = make_basic_repo().unwrap();
    make_file(&test_repo, "extra").unwrap();
    commit_all(&test_repo, "extra commit");
    check_all_objects_small(&test_repo);
}

/// Like [`check_all_objects`] but without the corpus-size guard, for the small
/// fixtures the rest of the suite uses.
fn check_all_objects_small(test_repo: &TestRepo) {
    let repo = open(test_repo);
    for (id, object_type, expected) in all_objects_with_bodies(test_repo) {
        let raw = block_on(repo.lookup_raw(id)).unwrap().unwrap();
        assert_eq!(raw.object_type, object_type);
        assert_eq!(raw.body, expected, "body mismatch for {id}");
    }
}
