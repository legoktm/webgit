//! Differential tests for the object database, against the host's `git`.
//!
//! `git cat-file --batch --batch-all-objects` enumerates every object in a
//! repository and prints its bytes, which is exactly what an object database
//! is for. Comparing the two covers pack discovery, index search, delta
//! reconstruction and loose reading in one pass, on every repository shape.

use crate::{ObjectDb, test_support::open_odb, test_support::open_odb_with};
use futures::executor::block_on;
use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};
use gib_object::ObjectType;
use gib_testkit::{
    TestFileSystem, TestRepo, make_basic_repo, make_packfile_repo, make_similar_commits,
};
use hex_literal::hex;
use std::fs::{create_dir, rename};

/// A corpus with enough distinct objects that pack indexes and abbreviations
/// have something to work with.
fn populated_repo() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    let root = test_repo.location.path();
    create_dir(root.join("bulk")).unwrap();
    for i in 0..300 {
        std::fs::write(
            root.join("bulk").join(format!("file{i}")),
            format!("odb differential object {i}\n"),
        )
        .unwrap();
    }
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit("bulk", "a user", "an-email-address", "2000-01-01T00:00:00Z")
        .unwrap();
    test_repo
}

fn loose_repo() -> TestRepo {
    populated_repo()
}

fn packed_repo() -> TestRepo {
    let test_repo = populated_repo();
    test_repo.run_git(["gc"]).unwrap();
    test_repo
}

fn delta_repo() -> TestRepo {
    let test_repo = populated_repo();
    test_repo
        .run_git(["repack", "-a", "-d", "-f", "--depth=50", "--window=250"])
        .unwrap();
    test_repo
}

/// A repository with both a pack *and* loose objects, so the packed-first,
/// loose-fallback probe order is exercised in one lookup sequence.
fn mixed_repo() -> TestRepo {
    let test_repo = packed_repo();
    let root = test_repo.location.path();
    std::fs::write(root.join("after-gc"), b"written after packing\n").unwrap();
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit(
            "loose on top of a pack",
            "a user",
            "an-email-address",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
    test_repo
}

/// Every object git knows about, with its type and bytes, in one invocation.
/// `--batch` emits `<oid> <type> <size>\n`, then `<size>` bytes, then a newline.
fn all_objects(test_repo: &TestRepo) -> Vec<(ObjectId, ObjectType, Vec<u8>)> {
    let stream = test_repo
        .run_git(["cat-file", "--batch", "--batch-all-objects"])
        .unwrap();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < stream.len() {
        let eol = pos + stream[pos..].iter().position(|&b| b == b'\n').unwrap();
        let mut header = stream[pos..eol].splitn(3, |&b| b == b' ');
        let id = ObjectId::from_hex(header.next().unwrap()).unwrap();
        let object_type = match header.next().unwrap() {
            b"commit" => ObjectType::Commit,
            b"tree" => ObjectType::Tree,
            b"blob" => ObjectType::Blob,
            b"tag" => ObjectType::Tag,
            other => panic!("unknown object type {}", String::from_utf8_lossy(other)),
        };
        let size: usize = str::from_utf8(header.next().unwrap())
            .unwrap()
            .parse()
            .unwrap();
        let body_start = eol + 1;
        let body = stream[body_start..body_start + size].to_vec();
        assert_eq!(stream[body_start + size], b'\n');
        pos = body_start + size + 1;
        out.push((id, object_type, body));
    }
    out
}

fn check_all_objects(test_repo: &TestRepo, odb: &ObjectDb<TestFileSystem>) {
    let objects = all_objects(test_repo);
    assert!(objects.len() > 100, "expected a repo with many objects");
    for (id, object_type, expected) in objects {
        let raw = block_on(odb.lookup(id))
            .unwrap()
            .unwrap_or_else(|| panic!("object {id} not found"));
        assert_eq!(raw.object_type, object_type, "type of {id}");
        assert_eq!(raw.body, expected, "body of {id}");
    }
}

#[test]
fn all_objects_loose() {
    let test_repo = loose_repo();
    check_all_objects(&test_repo, &open_odb(&test_repo));
}

#[test]
fn all_objects_packed() {
    let test_repo = packed_repo();
    check_all_objects(&test_repo, &open_odb(&test_repo));
}

#[test]
fn all_objects_deltified() {
    let test_repo = delta_repo();
    check_all_objects(&test_repo, &open_odb(&test_repo));
}

#[test]
fn all_objects_packed_and_loose() {
    let test_repo = mixed_repo();
    check_all_objects(&test_repo, &open_odb(&test_repo));
}

/// With the offset cache disabled the index's offsets are read per lookup
/// instead of held in memory; the answers must not change.
#[test]
fn all_objects_without_offset_cache() {
    let test_repo = packed_repo();
    check_all_objects(&test_repo, &open_odb_with(&test_repo, 0));
}

#[test]
fn absent_object_is_none() {
    let test_repo = packed_repo();
    let odb = open_odb(&test_repo);
    let missing = ObjectId::from_bytes(hex!("0000000000000000000000000000000000000000"));
    assert!(block_on(odb.lookup(missing)).unwrap().is_none());
}

#[test]
fn prefixes_match_rev_parse() {
    let test_repo = packed_repo();
    let odb = open_odb(&test_repo);
    let ids: Vec<ObjectId> = all_objects(&test_repo)
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    for id in ids.iter().step_by(23) {
        let hex = id.to_string();
        let prefix = &hex[..7];
        let candidates: Vec<ObjectId> = String::from_utf8(
            test_repo
                .run_git(["rev-parse", &format!("--disambiguate={prefix}")])
                .unwrap(),
        )
        .unwrap()
        .lines()
        .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
        .collect();
        let expected = match candidates.len() {
            0 => PrefixResolution::NotFound,
            1 => PrefixResolution::Found(candidates[0]),
            _ => PrefixResolution::Ambiguous,
        };
        let parsed = ObjectIdPrefix::from_hex(prefix.as_bytes()).unwrap();
        assert_eq!(
            block_on(odb.resolve_prefix(&parsed)).unwrap(),
            expected,
            "prefix {prefix}"
        );
    }
}

/// Objects stored as ref-deltas (rather than the usual offset-deltas) resolve
/// their base by searching the pack index, a distinct code path.
#[test]
fn ref_delta_pack() {
    let test_repo = make_packfile_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    test_repo.run_git(["gc"]).unwrap();
    let objects_dir = test_repo.location.path().join(".git").join("objects");
    create_dir(objects_dir.join("pack-new")).unwrap();
    let mut git_process = test_repo
        .git_command()
        .current_dir(objects_dir.join("pack-new"))
        .args([
            "pack-objects",
            "--revs",
            "--no-reuse-delta",
            "--all",
            "pack-refdelta",
            // --delta-base-offset is off by default, which is what we want
        ])
        .spawn()
        .unwrap();
    assert!(git_process.wait().unwrap().success());
    rename(objects_dir.join("pack"), objects_dir.join("pack-old")).unwrap();
    rename(objects_dir.join("pack-new"), objects_dir.join("pack")).unwrap();
    // The earlier `git gc` left an objects/info/packs naming the old pack;
    // refresh it so pack discovery (which prefers the manifest) finds the
    // swapped-in pack.
    test_repo.run_git(["update-server-info"]).unwrap();

    let odb = open_odb(&test_repo);
    for (id, object_type, expected) in all_objects(&test_repo) {
        let raw = block_on(odb.lookup(id))
            .unwrap()
            .unwrap_or_else(|| panic!("object {id} not found"));
        assert_eq!(raw.object_type, object_type, "type of {id}");
        assert_eq!(raw.body, expected, "body of {id}");
    }
}
