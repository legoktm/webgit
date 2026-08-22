//! Differential tests for the whole archive, against `git archive`.
//!
//! A repository is built with the `git` CLI, walked through [`collect_entries`]
//! from its own object store, written out by [`TarWriter`], and compared byte
//! for byte with what `git archive --format=tar` makes of the same commit.
//! `writer.rs` pins the tar format against a hand-built entry list; this pins
//! everything in front of it — which entries there are, in what order, and
//! which ones `.gitattributes` keeps out.
//!
//! The fixture is shaped around `export-ignore`, since that is the only
//! attribute the walk reads: a directory-only pattern, a bare name matching a
//! directory, an unanchored pattern that reaches every depth, an anchored one
//! that does not, a subdirectory file overriding its parent, and an attributes
//! file that excludes itself.

use crate::{ArchiveEntry, ObjectSource, TarWriter, collect_entries};
use futures::FutureExt;
use futures::executor::block_on;
use futures::future::LocalBoxFuture;
use gib_fs::Directory;
use gib_object::{Object, ObjectId, Tree};
use gib_odb::ObjectDb;
use gib_testkit::{TestFileSystem, TestRepo};
use std::process::Command;

/// The repository's object store, as an [`ObjectSource`] for the walk.
struct Odb(ObjectDb<TestFileSystem>);

impl ObjectSource for Odb {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move {
            let raw = self
                .0
                .lookup(id)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .ok_or_else(|| anyhow::anyhow!("missing object {id}"))?;
            Object::from_raw(id, raw).map_err(|e| anyhow::anyhow!("{e:?}"))
        }
        .boxed_local()
    }
}

/// Files to write into the fixture, as `(path, contents)`. Directories are
/// created as needed.
const FILES: &[(&str, &str)] = &[
    (
        ".gitattributes",
        "\
drop-dir/ export-ignore
noslash export-ignore
*.tmp export-ignore
/root-only.txt export-ignore
keep/**/deep.txt export-ignore
",
    ),
    ("top.txt", "top\n"),
    ("root-only.txt", "gone\n"),
    ("a.tmp", "gone\n"),
    ("drop-dir/x.txt", "gone\n"),
    ("drop-dir/nested/y.txt", "gone\n"),
    ("noslash/z.txt", "gone\n"),
    ("keep/mid/deep.txt", "gone\n"),
    ("keep/mid/other.txt", "kept\n"),
    // Anchored at the root, so this namesake survives; the unanchored `*.tmp`
    // reaches down here and its namesake does not.
    ("nested/root-only.txt", "kept\n"),
    ("nested/b.tmp", "gone\n"),
    (
        "sub/.gitattributes",
        "\
*.log export-ignore
keep.log -export-ignore
",
    ),
    ("sub/a.log", "gone\n"),
    ("sub/keep.log", "kept\n"),
    ("sub/deeper/b.log", "gone\n"),
    ("sub/plain.txt", "kept\n"),
    // An attributes file that excludes itself: its bytes are still needed to
    // decide the directory, and then thrown away rather than archived.
    (
        "selfless/.gitattributes",
        "\
.gitattributes export-ignore
*.no export-ignore
",
    ),
    ("selfless/kept.txt", "kept\n"),
    ("selfless/dropped.no", "gone\n"),
];

/// What `git archive` writes for the fixture's `HEAD`.
fn git_archive(repo: &TestRepo, prefix: &str) -> Vec<u8> {
    let out = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            &format!("--prefix={prefix}"),
            "HEAD",
        ])
        .current_dir(repo.location.path())
        .output()
        .expect("git archive runs");
    assert!(out.status.success(), "git archive failed: {out:?}");
    out.stdout
}

/// Build the fixture repository and commit it.
fn fixture() -> TestRepo {
    let repo = TestRepo::new().expect("a repository");
    for (path, contents) in FILES {
        let path = repo.location.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    repo.run_git(["add", "-A"]).unwrap();
    repo.commit("archive me", "a user", "an-email", "2023-11-14T17:13:20Z")
        .unwrap();
    repo
}

/// The commit's tree, its id, and the timestamp git stamps its archive with.
fn head(repo: &TestRepo, odb: &Odb) -> (Tree, String, u64) {
    let sha = String::from_utf8(repo.run_git(["rev-parse", "HEAD"]).unwrap()).unwrap();
    let sha = sha.trim().to_string();
    let mtime: u64 = String::from_utf8(repo.run_git(["log", "-1", "--format=%ct"]).unwrap())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let id = ObjectId::from_hex(sha.as_bytes()).unwrap();
    let commit = block_on(odb.object(id)).unwrap().commit().unwrap();
    let tree = block_on(odb.object(commit.tree())).unwrap().tree().unwrap();
    (tree, sha, mtime)
}

fn open_odb(repo: &TestRepo) -> Odb {
    let objects = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
    Odb(block_on(ObjectDb::open(objects, 64 * 1024 * 1024)).unwrap())
}

/// Walk the repository and write the tar the browser would produce.
fn our_archive(odb: &Odb, tree: &Tree, sha: &str, mtime: u64, prefix: &str) -> Vec<u8> {
    let entries: Vec<ArchiveEntry> =
        block_on(collect_entries(odb, tree, "", &|_, _| {})).expect("the walk succeeds");
    let mut writer = TarWriter::new(prefix, sha, mtime).unwrap();
    for entry in &entries {
        writer.append(entry).unwrap();
    }
    let mut out = writer.take();
    out.extend_from_slice(&writer.finish().unwrap());
    out
}

/// The names in a tar, in order, which is what a mismatch is worth reporting.
fn names(tar: &[u8]) -> Vec<String> {
    let mut archive = tar::Archive::new(tar);
    archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn test_matches_git_archive_with_export_ignore() {
    let prefix = "fixture/";
    let repo = fixture();
    let odb = open_odb(&repo);
    let (tree, sha, mtime) = head(&repo, &odb);

    let theirs = git_archive(&repo, prefix);
    let ours = our_archive(&odb, &tree, &sha, mtime, prefix);

    assert_eq!(
        names(&ours),
        names(&theirs),
        "the archives hold different entries"
    );
    assert_eq!(
        ours.len(),
        theirs.len(),
        "archive length differs from git's"
    );
    assert!(ours == theirs, "archive bytes differ from `git archive`");
}

/// The walk must not fetch what it is not going to archive — the reason the
/// attributes are read before a directory's entries are queued rather than
/// filtered out at the end. An ignored directory's contents are the clearest
/// case: nothing under `drop-dir/` should ever be asked for.
#[test]
fn test_ignored_objects_are_never_fetched() {
    let repo = fixture();
    let odb = open_odb(&repo);
    let (tree, ..) = head(&repo, &odb);

    let ignored: Vec<ObjectId> = ["drop-dir/x.txt", "drop-dir/nested/y.txt", "sub/a.log"]
        .iter()
        .map(|path| {
            let out = repo
                .run_git(["rev-parse", &format!("HEAD:{path}")])
                .unwrap();
            ObjectId::from_hex(out.trim_ascii_end()).unwrap()
        })
        .collect();

    let counting = Counting {
        inner: odb,
        asked: std::cell::RefCell::new(Vec::new()),
    };
    block_on(collect_entries(&counting, &tree, "", &|_, _| {})).unwrap();

    let asked = counting.asked.into_inner();
    for id in ignored {
        assert!(
            !asked.contains(&id),
            "the walk fetched {id}, which export-ignore keeps out of the archive"
        );
    }
}

/// An object source that records what it was asked for.
struct Counting {
    inner: Odb,
    asked: std::cell::RefCell<Vec<ObjectId>>,
}

impl ObjectSource for Counting {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        self.asked.borrow_mut().push(id);
        self.inner.object(id)
    }
}
