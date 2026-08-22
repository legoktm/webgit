//! Differential tests for the object parsers, against the host's `git`.
//!
//! Object bytes are read with `git cat-file` and parsed here directly, with no
//! filesystem or repository layer involved — this crate has neither. Every
//! field is then checked against what git reports for the same object.

use crate::{Object, ObjectId, ObjectType, RawObject, TreeEntryType, parse_header};
use gib_testkit::{TestRepo, make_basic_repo, make_file, make_similar_commits};

/// A repository with a commit, an annotated tag, and a nested tree carrying
/// every entry type the parser understands.
fn fixture() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    let root = test_repo.location.path();
    std::fs::create_dir(root.join("dir")).unwrap();
    std::fs::write(root.join("dir").join("one"), b"the first file\n").unwrap();
    std::fs::write(root.join("dir").join("script.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(
        root.join("dir").join("script.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("one", root.join("dir").join("a-link")).unwrap();
    make_file(&test_repo, "top-level").unwrap();
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit(
            "a mixed tree",
            "a user",
            "an-email-address",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
    test_repo
}

fn git(test_repo: &TestRepo, args: &[&str]) -> Vec<u8> {
    test_repo.run_git(args).unwrap()
}

fn git_text(test_repo: &TestRepo, args: &[&str]) -> String {
    String::from_utf8(git(test_repo, args).trim_ascii_end().to_vec()).unwrap()
}

fn git_oid(test_repo: &TestRepo, rev: &str) -> ObjectId {
    ObjectId::from_hex(git_text(test_repo, &["rev-parse", rev]).as_bytes()).unwrap()
}

/// Read an object's bytes with `git cat-file` and parse them.
fn read(test_repo: &TestRepo, id: ObjectId, object_type: ObjectType) -> Object {
    let body = git(
        test_repo,
        &["cat-file", object_type.name(), &id.to_string()],
    );
    Object::from_raw(id, RawObject { object_type, body }).unwrap()
}

#[test]
fn commit_fields_match_git_log() {
    let test_repo = fixture();
    // Every commit reachable from HEAD, so both root and parented commits are
    // covered.
    for line in git_text(&test_repo, &["log", "--format=%H"]).lines() {
        let id = ObjectId::from_hex(line.as_bytes()).unwrap();
        let commit = read(&test_repo, id, ObjectType::Commit).commit().unwrap();
        let field = |format: &str| {
            git_text(
                &test_repo,
                &["log", "-1", &format!("--format={format}"), line],
            )
        };
        assert_eq!(commit.id(), id);
        assert_eq!(commit.tree().to_string(), field("%T"));
        let parents: Vec<String> = commit.parents().iter().map(ObjectId::to_string).collect();
        assert_eq!(parents.join(" "), field("%P"));
        assert_eq!(commit.author_name(), field("%an").as_bytes());
        assert_eq!(commit.author_email(), field("%ae").as_bytes());
        assert_eq!(commit.committer_name(), field("%cn").as_bytes());
        assert_eq!(commit.committer_email(), field("%ce").as_bytes());
        assert_eq!(
            commit.author_date().timestamp().as_second().to_string(),
            field("%at")
        );
        assert_eq!(
            commit.commit_date().timestamp().as_second().to_string(),
            field("%ct")
        );
        // %B is the raw subject+body. Compare with trailing newlines trimmed
        // from both sides: git's `--format` appends one of its own, so the
        // exact tail is an artefact of the oracle rather than of the object.
        assert!(!commit.message().is_empty());
        assert_eq!(commit.message().trim_ascii_end(), field("%B").as_bytes());
    }
}

/// Fields that are legitimately empty. `git commit --allow-empty-message`
/// writes a commit whose message is zero bytes long, and git tolerates an
/// author line with no name before the email (`git fsck` only warns about it),
/// so both have to parse rather than blow up on the empty slice.
#[test]
fn empty_commit_fields_match_git() {
    let test_repo = make_basic_repo().unwrap();
    test_repo
        .run_git(["commit", "--allow-empty", "--allow-empty-message", "-m", ""])
        .unwrap();
    let id = git_oid(&test_repo, "HEAD");
    let commit = read(&test_repo, id, ObjectType::Commit).commit().unwrap();
    assert!(commit.message().is_empty());
    assert_eq!(commit.author_name(), b"a user".as_slice());

    // Porcelain refuses to commit with an empty author name, so write the
    // object git would have stored for one directly.
    let body = format!(
        "tree {}\nauthor  <an-email-address> 1774735018 +0530\n\
         committer  <an-email-address> 1774735018 +0530\n\na commit\n",
        git_oid(&test_repo, "HEAD^{tree}")
    );
    let path = test_repo.location.path().join("nameless-commit");
    std::fs::write(&path, body).unwrap();
    let id = ObjectId::from_hex(
        git(
            &test_repo,
            &[
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                path.to_str().unwrap(),
            ],
        )
        .trim_ascii_end(),
    )
    .unwrap();
    let commit = read(&test_repo, id, ObjectType::Commit).commit().unwrap();
    assert_eq!(
        commit.author_name(),
        git_text(&test_repo, &["log", "-1", "--format=%an", &id.to_string()]).as_bytes()
    );
    assert!(commit.author_name().is_empty());
    assert_eq!(commit.author_email(), b"an-email-address".as_slice());
    assert_eq!(commit.message(), b"a commit\n".as_slice());
}

#[test]
fn tag_fields_match_for_each_ref() {
    let test_repo = fixture();
    let id = git_oid(&test_repo, "refs/tags/a-fat-tag");
    let tag = read(&test_repo, id, ObjectType::Tag).tag().unwrap();
    let field = |format: &str| {
        git_text(
            &test_repo,
            &[
                "for-each-ref",
                &format!("--format={format}"),
                "refs/tags/a-fat-tag",
            ],
        )
    };
    assert_eq!(tag.id(), id);
    assert_eq!(tag.name(), field("%(refname:strip=2)").as_bytes());
    assert_eq!(tag.target(), git_oid(&test_repo, "refs/tags/a-fat-tag^{}"));
    assert_eq!(tag.tag_type(), ObjectType::Commit);
    assert_eq!(
        tag.tagger_name().unwrap(),
        field("%(taggername)").as_bytes()
    );
    assert_eq!(
        tag.tagger_email().unwrap(),
        field("%(taggeremail:trim)").as_bytes()
    );
    assert_eq!(
        tag.date().unwrap().timestamp().as_second().to_string(),
        field("%(taggerdate:unix)")
    );
    // As for commits, `for-each-ref` appends a newline of its own.
    assert!(!tag.message().is_empty());
    assert_eq!(
        tag.message().trim_ascii_end(),
        field("%(contents)").as_bytes()
    );
}

#[test]
fn tree_entries_match_ls_tree() {
    let test_repo = fixture();
    // `ls-tree` without -r lists one tree's own entries, in tree order — the
    // same order the parser yields them in.
    for tree_rev in ["HEAD^{tree}", "HEAD:dir"] {
        let id = git_oid(&test_repo, tree_rev);
        let tree = read(&test_repo, id, ObjectType::Tree).tree().unwrap();
        let expected: Vec<String> = git_text(&test_repo, &["ls-tree", tree_rev])
            .lines()
            .map(str::to_string)
            .collect();
        let actual: Vec<String> = tree
            .entries()
            .map(|entry| {
                let (mode, kind) = match entry.entry_type() {
                    TreeEntryType::File => ("100644", "blob"),
                    TreeEntryType::Executable => ("100755", "blob"),
                    TreeEntryType::Symlink => ("120000", "blob"),
                    TreeEntryType::Tree => ("040000", "tree"),
                    TreeEntryType::Commit => ("160000", "commit"),
                };
                format!(
                    "{mode} {kind} {}\t{}",
                    entry.id(),
                    str::from_utf8(entry.name()).unwrap()
                )
            })
            .collect();
        assert_eq!(actual, expected, "mismatch for {tree_rev}");
        assert!(!expected.is_empty());
    }
}

#[test]
fn blob_body_is_verbatim() {
    let test_repo = fixture();
    let id = git_oid(&test_repo, "HEAD:dir/script.sh");
    let blob = read(&test_repo, id, ObjectType::Blob).blob().unwrap();
    assert_eq!(
        blob.data(),
        git(&test_repo, &["cat-file", "blob", &id.to_string()])
    );
}

/// A loose object's on-disk header is `<type> <size>\0`, and the size it
/// records is the body length git reports.
#[test]
fn loose_header_matches_cat_file_size() {
    let test_repo = fixture();
    let id = git_oid(&test_repo, "HEAD");
    let size: u64 = git_text(&test_repo, &["cat-file", "-s", &id.to_string()])
        .parse()
        .unwrap();
    let body = git(&test_repo, &["cat-file", "commit", &id.to_string()]);
    let mut loose = format!("commit {size}\0").into_bytes();
    loose.extend_from_slice(&body);
    let (rest, (parsed_size, object_type)) = parse_header(&loose).unwrap();
    assert_eq!(parsed_size.0, size);
    assert_eq!(object_type, ObjectType::Commit);
    assert_eq!(rest, body);
}

/// Every object in the repository must hash to the name git gave it. This is
/// the property object verification rests on, checked against the real thing
/// over a tree that carries all four object types.
#[test]
fn compute_id_names_every_object_as_git_does() {
    let test_repo = fixture();
    let listing = git_text(
        &test_repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );

    let mut seen = 0;
    for line in listing.lines() {
        let (name, type_name) = line.split_once(' ').unwrap();
        let id = ObjectId::from_hex(name.as_bytes()).unwrap();
        let object_type = match type_name {
            "commit" => ObjectType::Commit,
            "tree" => ObjectType::Tree,
            "blob" => ObjectType::Blob,
            "tag" => ObjectType::Tag,
            other => panic!("unexpected object type {other}"),
        };
        let body = git(&test_repo, &["cat-file", object_type.name(), name]);
        let object = RawObject { object_type, body };
        assert_eq!(object.compute_id(), id, "wrong ID computed for {name}");
        object.verify(id).unwrap();
        seen += 1;
    }

    // The fixture has commits, trees, blobs, and an annotated tag; if the
    // listing ever came back empty this test would otherwise pass vacuously.
    assert!(
        seen > 4,
        "expected a populated repository, saw {seen} objects"
    );
}
