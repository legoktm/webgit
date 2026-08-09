//! Differential tests for the ref parsers, against the host's `git`.
//!
//! The files are read straight off disk with `std::fs` — this crate has no
//! filesystem abstraction — and the parse results are compared with what git
//! reports for the same repository.

use crate::{RefEntry, RefName, RefTarget, parse_info_refs, parse_packed_refs};
use gib_hash::ObjectId;
use gib_testkit::{TestRepo, make_basic_repo};
use std::collections::BTreeMap;

/// A repository with branches, lightweight and annotated tags, and a remote
/// ref, all packed into `packed-refs`.
fn packed_fixture() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    test_repo.run_git(["branch", "a-branch"]).unwrap();
    test_repo.run_git(["branch", "foo/nested"]).unwrap();
    test_repo.run_git(["tag", "thin-tag"]).unwrap();
    test_repo
        .run_git(["tag", "-a", "-m", "another tag", "another-fat-tag"])
        .unwrap();
    test_repo
        .run_git(["update-ref", "refs/remotes/origin/main", "HEAD"])
        .unwrap();
    test_repo.run_git(["pack-refs", "--all"]).unwrap();
    test_repo.run_git(["update-server-info"]).unwrap();
    test_repo
}

fn git_text(test_repo: &TestRepo, args: &[&str]) -> String {
    String::from_utf8(test_repo.run_git(args).unwrap().trim_ascii_end().to_vec()).unwrap()
}

fn read(test_repo: &TestRepo, path: &[&str]) -> Vec<u8> {
    let mut full = test_repo.location.path().join(".git");
    for component in path {
        full = full.join(component);
    }
    std::fs::read(full).unwrap()
}

/// What git says every ref is, as `refs/…` -> (target, peeled).
fn git_refs(test_repo: &TestRepo) -> BTreeMap<String, (ObjectId, Option<ObjectId>)> {
    git_text(
        test_repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(*objectname)",
        ],
    )
    .lines()
    .map(|line| {
        let mut fields = line.split(' ');
        let name = fields.next().unwrap().to_string();
        let target = ObjectId::from_hex(fields.next().unwrap().as_bytes()).unwrap();
        // `*objectname` is empty unless the ref is an annotated tag.
        let peeled = fields
            .next()
            .filter(|field| !field.is_empty())
            .map(|field| ObjectId::from_hex(field.as_bytes()).unwrap());
        (name, (target, peeled))
    })
    .collect()
}

fn parsed_to_map(refs: Vec<(RefName, RefEntry)>) -> BTreeMap<String, (ObjectId, Option<ObjectId>)> {
    refs.into_iter()
        .map(|(name, entry)| {
            let RefName::Ref(path) = name else {
                panic!("ref listings never contain HEAD")
            };
            (
                format!("refs/{}", String::from_utf8(path).unwrap()),
                (entry.target, entry.peeled),
            )
        })
        .collect()
}

#[test]
fn packed_refs_matches_for_each_ref() {
    let test_repo = packed_fixture();
    let parsed = parsed_to_map(parse_packed_refs(&read(&test_repo, &["packed-refs"])).unwrap());
    assert_eq!(parsed, git_refs(&test_repo));
    // The fixture must actually exercise the `^`-peeled lines.
    assert!(parsed.values().any(|(_, peeled)| peeled.is_some()));
}

#[test]
fn packed_refs_targets_match_show_ref() {
    let test_repo = packed_fixture();
    // `show-ref` reports the ref's direct target, ignoring peeling.
    let expected: BTreeMap<String, ObjectId> = git_text(&test_repo, &["show-ref"])
        .lines()
        .map(|line| {
            let (oid, name) = line.split_once(' ').unwrap();
            (
                name.to_string(),
                ObjectId::from_hex(oid.as_bytes()).unwrap(),
            )
        })
        .collect();
    let parsed: BTreeMap<String, ObjectId> =
        parsed_to_map(parse_packed_refs(&read(&test_repo, &["packed-refs"])).unwrap())
            .into_iter()
            .map(|(name, (target, _))| (name, target))
            .collect();
    assert_eq!(parsed, expected);
}

#[test]
fn info_refs_matches_for_each_ref() {
    let test_repo = packed_fixture();
    let parsed = parsed_to_map(parse_info_refs(&read(&test_repo, &["info", "refs"])).unwrap());
    assert_eq!(parsed, git_refs(&test_repo));
}

#[test]
fn head_matches_symbolic_ref() {
    let test_repo = packed_fixture();
    let (_, target) = RefTarget::parse_loose_ref(&read(&test_repo, &["HEAD"])).unwrap();
    let expected = git_text(&test_repo, &["symbolic-ref", "HEAD"]);
    let expected = expected.strip_prefix("refs/").unwrap();
    assert_eq!(
        target,
        RefTarget::Symbolic(RefName::Ref(expected.as_bytes().to_vec()))
    );
}

#[test]
fn loose_ref_matches_rev_parse() {
    let test_repo = make_basic_repo().unwrap();
    test_repo.run_git(["branch", "a-branch"]).unwrap();
    let (_, target) =
        RefTarget::parse_loose_ref(&read(&test_repo, &["refs", "heads", "a-branch"])).unwrap();
    assert_eq!(
        target,
        RefTarget::Direct(
            ObjectId::from_hex(git_text(&test_repo, &["rev-parse", "a-branch"]).as_bytes())
                .unwrap()
        )
    );
}
