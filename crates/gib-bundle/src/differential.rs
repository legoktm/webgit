//! Differential tests for the whole bundle, against the `git` CLI.
//!
//! A repository is built with `git`, walked through [`collect_objects`] from
//! its own object store, written out by [`BundleWriter`], and then handed back
//! to git: `git bundle verify` reads it, `git clone` clones it, and what comes
//! out is compared with the repository it was built from.
//!
//! The comparison is deliberately of *contents* rather than of bytes, which is
//! where this differs from `gib-archive`'s tests. `git bundle create` deltifies
//! its pack; every object here is written whole, so the two files cannot be
//! byte-equal and matching them would mean implementing delta compression to no
//! one's benefit. What must match is everything a reader can observe: the refs
//! the bundle names, the objects it carries (against `git rev-list --objects`,
//! which is how git chooses them), and the repository cloning it produces.
//!
//! The fixture is shaped around what a history walk has to get right: a merge,
//! so both parents are followed; a file left untouched across every commit, so
//! its blob and tree are packed once rather than per revision; an annotated tag,
//! which is an object of its own above the commit; a lightweight tag, which is
//! not; a symlink, whose target is blob content; and a submodule, whose commit
//! belongs to a repository we don't have and must not be packed.

use crate::{BundleRef, BundleWriter, ObjectSource, collect_objects};
use futures::FutureExt;
use futures::executor::block_on;
use futures::future::LocalBoxFuture;
use gib_fs::Directory;
use gib_object::{Object, ObjectId};
use gib_odb::ObjectDb;
use gib_testkit::{TestFileSystem, TestRepo};
use std::collections::BTreeSet;
use std::path::Path;
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

/// The commit a submodule entry points at. Nothing ever looks it up — which is
/// the point: a walk that followed it would fail here rather than quietly
/// packing someone else's object.
const SUBMODULE_ID: &str = "1234567890123456789012345678901234567890";

/// Build the fixture: two commits on `main`, a branch off the first, a merge of
/// the two, and the tags over the result.
fn fixture() -> TestRepo {
    let repo = TestRepo::new().expect("a repository");
    let root = repo.location.path().to_path_buf();

    write(&root, "README.md", "webgit\n");
    write(&root, "src/lib.rs", "fn one() {}\n");
    // Never touched again, so every later revision shares this blob and the
    // tree above it: a bundle that packed per revision would carry them twice.
    write(&root, "src/stable.rs", "fn stable() {}\n");
    std::os::unix::fs::symlink("README.md", root.join("readme-link")).unwrap();
    repo.run_git(["add", "-A"]).unwrap();
    // A gitlink staged directly: a real submodule would need a second
    // repository, and nothing may resolve the commit anyway.
    repo.run_git([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{SUBMODULE_ID},vendor"),
    ])
    .unwrap();
    repo.commit("one", "a user", "an-email", "2023-11-14T17:13:20Z")
        .unwrap();

    repo.run_git(["checkout", "-q", "-b", "topic"]).unwrap();
    write(&root, "src/topic.rs", "fn topic() {}\n");
    repo.run_git(["add", "-A"]).unwrap();
    repo.commit("topic work", "a user", "an-email", "2023-11-15T10:00:00Z")
        .unwrap();

    repo.run_git(["checkout", "-q", "main"]).unwrap();
    write(&root, "src/lib.rs", "fn one() {}\nfn two() {}\n");
    write(&root, "src/deep/nested.txt", "deep\n");
    repo.run_git(["add", "-A"]).unwrap();
    repo.commit("two", "a user", "an-email", "2023-11-16T09:00:00Z")
        .unwrap();

    // A merge, so the walk has to follow both parents to reach the topic
    // branch's commit and its blob.
    repo.run_git_with_env(
        [
            ("GIT_AUTHOR_DATE", "2023-11-17T09:00:00Z"),
            ("GIT_COMMITTER_DATE", "2023-11-17T09:00:00Z"),
        ],
        ["merge", "--no-ff", "-m", "merge topic", "topic"],
    )
    .unwrap();

    repo.tag_annotated(
        "v1.0.0",
        "HEAD",
        "release 1.0.0",
        "a user",
        "an-email",
        "2023-11-18T09:00:00Z",
    )
    .unwrap();
    repo.run_git(["tag", "light", "main~1"]).unwrap();
    repo
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn open_odb(repo: &TestRepo) -> Odb {
    let objects = block_on(repo.git_dir().open_subdir(b"objects")).unwrap();
    Odb(block_on(ObjectDb::open(objects, 64 * 1024 * 1024)).unwrap())
}

/// One line of `git`'s output, trimmed.
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git speaks UTF-8 here")
}

fn rev_parse(repo: &TestRepo, rev: &str) -> ObjectId {
    let out = git(repo.location.path(), &["rev-parse", rev]);
    ObjectId::from_hex(out.trim().as_bytes()).expect("a hash")
}

/// The objects `git rev-list --objects` reaches from `revs` — how git itself
/// decides what belongs in a bundle.
fn git_objects(repo: &TestRepo, revs: &[&str]) -> BTreeSet<String> {
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(revs);
    git(repo.location.path(), &args)
        .lines()
        // `<oid> [<path>]`; the path is what the object was reached by, and
        // several objects can share one.
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Every object in a repository, as `<type> <oid>` lines — what a clone of the
/// bundle must have ended up with.
fn all_objects(repo: &Path) -> BTreeSet<String> {
    git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objecttype) %(objectname)",
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

/// Walk the repository and write the bundle the browser would produce.
///
/// The two passes are the real ones: [`collect_objects`] settles the ids, and
/// each object is fetched again as it is written, exactly as webgit does it.
fn our_bundle(odb: &Odb, refs: &[BundleRef]) -> Vec<u8> {
    let tips: Vec<ObjectId> = refs.iter().map(|r| r.id).collect();
    let ids = block_on(collect_objects(odb, &tips, &|_, _| {})).expect("the walk succeeds");

    let mut writer = BundleWriter::new(refs, ids.len()).unwrap();
    let mut out = Vec::new();
    for id in ids {
        let object = block_on(odb.object(id)).expect("the object is still there");
        writer.append_object(&object).unwrap();
        out.append(&mut writer.take());
    }
    out.extend_from_slice(&writer.finish().unwrap());
    out
}

/// Write a bundle out and hand back its path, kept alive by `dir`.
fn save(dir: &tempfile::TempDir, name: &str, bundle: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bundle).unwrap();
    path
}

/// The refs the fixture's release tag names: the tag object itself, and a HEAD
/// so a clone has something to check out (see [`crate::BundleRef`]).
fn tag_refs(repo: &TestRepo) -> Vec<BundleRef> {
    vec![
        BundleRef::new("refs/tags/v1.0.0", rev_parse(repo, "refs/tags/v1.0.0")),
        BundleRef::new("HEAD", rev_parse(repo, "v1.0.0^{commit}")),
    ]
}

/// The objects we pack are the ones git would pack: same set, no more (the
/// submodule's commit) and no fewer (the topic branch reached through the
/// merge's second parent).
#[test]
fn test_packs_what_git_rev_list_packs() {
    let repo = fixture();
    let odb = open_odb(&repo);

    let tips = vec![rev_parse(&repo, "refs/tags/v1.0.0")];
    let ours: BTreeSet<String> = block_on(collect_objects(&odb, &tips, &|_, _| {}))
        .unwrap()
        .iter()
        .map(ObjectId::to_string)
        .collect();

    assert_eq!(ours, git_objects(&repo, &["v1.0.0"]));
    assert!(
        !ours.contains(SUBMODULE_ID),
        "the submodule's commit was packed"
    );
}

/// The file git reads: `git bundle verify` accepts it, and names the refs we
/// put in its header.
#[test]
fn test_git_verifies_our_bundle() {
    let repo = fixture();
    let odb = open_odb(&repo);
    let dir = tempfile::tempdir().unwrap();
    let path = save(&dir, "ours.bundle", &our_bundle(&odb, &tag_refs(&repo)));

    // `verify` reads the whole pack and checks its checksum, so this is the
    // container being validated by git rather than by us.
    git(
        repo.location.path(),
        &["bundle", "verify", path.to_str().unwrap()],
    );

    let heads = git(
        repo.location.path(),
        &["bundle", "list-heads", path.to_str().unwrap()],
    );
    let tag = rev_parse(&repo, "refs/tags/v1.0.0");
    let head = rev_parse(&repo, "v1.0.0^{commit}");
    assert_eq!(
        heads,
        format!("{tag} refs/tags/v1.0.0\n{head} HEAD\n"),
        "the header names different refs"
    );
}

/// Cloning the bundle produces the repository it was built from: every object,
/// the tag, and a checked-out worktree.
#[test]
fn test_git_clones_our_bundle() {
    let repo = fixture();
    let odb = open_odb(&repo);
    let dir = tempfile::tempdir().unwrap();
    let path = save(&dir, "ours.bundle", &our_bundle(&odb, &tag_refs(&repo)));

    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            path.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    // `git clone` runs `index-pack`, which reconstructs and re-hashes every
    // object in the pack: had a single body been written wrong, the clone would
    // have failed above.
    assert_eq!(
        all_objects(&clone),
        all_objects(repo.location.path()),
        "the clone holds different objects"
    );
    assert_eq!(
        git(&clone, &["tag"]),
        "v1.0.0\n",
        "the annotated tag didn't survive"
    );
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        git(repo.location.path(), &["rev-parse", "HEAD"])
    );
    // The whole worktree, content and modes, against the original.
    assert_eq!(
        git(&clone, &["ls-tree", "-r", "HEAD"]),
        git(repo.location.path(), &["ls-tree", "-r", "HEAD"])
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("src/lib.rs")).unwrap(),
        "fn one() {}\nfn two() {}\n"
    );
}

/// Against git's own bundle of the same ref: the same refs named, and the same
/// objects delivered. The bytes differ (git deltifies, we don't) and are not
/// what either file promises.
#[test]
fn test_carries_what_gits_own_bundle_carries() {
    let repo = fixture();
    let odb = open_odb(&repo);
    let dir = tempfile::tempdir().unwrap();

    let refs = vec![BundleRef::new(
        "refs/tags/v1.0.0",
        rev_parse(&repo, "refs/tags/v1.0.0"),
    )];
    let ours = save(&dir, "ours.bundle", &our_bundle(&odb, &refs));
    let theirs = dir.path().join("theirs.bundle");
    git(
        repo.location.path(),
        &[
            "bundle",
            "create",
            theirs.to_str().unwrap(),
            "refs/tags/v1.0.0",
        ],
    );

    let heads = |bundle: &Path| {
        git(
            repo.location.path(),
            &["bundle", "list-heads", bundle.to_str().unwrap()],
        )
    };
    assert_eq!(heads(&ours), heads(&theirs));

    // Fetched rather than cloned: a tag-only bundle has no HEAD to check out,
    // and what is being compared is the objects that arrive.
    let unpack = |name: &str, bundle: &Path| {
        let into = dir.path().join(name);
        git(dir.path(), &["init", "-q", into.to_str().unwrap()]);
        git(&into, &["fetch", "-q", bundle.to_str().unwrap(), "v1.0.0"]);
        all_objects(&into)
    };
    assert_eq!(unpack("from-ours", &ours), unpack("from-theirs", &theirs));
}

/// A branch's bundle: the ref keeps its name, and a clone lands on the branch
/// with a worktree rather than on a detached HEAD.
#[test]
fn test_a_branch_bundle_clones_onto_its_branch() {
    let repo = fixture();
    let odb = open_odb(&repo);
    let dir = tempfile::tempdir().unwrap();

    let head = rev_parse(&repo, "refs/heads/main");
    let refs = vec![
        BundleRef::new("refs/heads/main", head),
        BundleRef::new("HEAD", head),
    ];
    let path = save(&dir, "main.bundle", &our_bundle(&odb, &refs));

    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            path.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main\n"
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), format!("{head}\n"));
    // A branch bundle carries the history, not the tags above it.
    assert_eq!(git(&clone, &["tag"]), "");
}

/// The same bundle whether the objects were read loose or out of a pack.
///
/// Every object is written from the body `gib-object` parsed it from, so this
/// is delta reconstruction being held to the loose bytes — and the bundle being
/// a function of the repository's contents rather than of how it happens to be
/// stored.
#[test]
fn test_packed_objects_produce_the_same_bundle() {
    let repo = fixture();
    let loose = our_bundle(&open_odb(&repo), &tag_refs(&repo));

    repo.run_git(["repack", "-a", "-d", "--depth=50", "--window=50"])
        .unwrap();
    let packed = our_bundle(&open_odb(&repo), &tag_refs(&repo));

    assert_eq!(loose, packed, "the bundle changed with the object layout");
}

/// A repository with real history — every commit rewriting one of a handful of
/// files — which is where deltas are the difference between a bundle and a
/// download nobody wants.
fn deep_fixture() -> TestRepo {
    let repo = TestRepo::new().expect("a repository");
    let root = repo.location.path().to_path_buf();
    for revision in 0..40 {
        for file in 0..4 {
            // Each revision rewrites one line in the middle of a long file: the
            // shape a delta should reduce to a few bytes.
            let body: String = (0..300)
                .map(|line| {
                    if line == revision * 7 % 300 {
                        format!("line {line} touched in revision {revision}\n")
                    } else {
                        format!("line {line} of file {file}\n")
                    }
                })
                .collect();
            write(&root, &format!("src/file{file}.txt"), &body);
        }
        repo.run_git(["add", "-A"]).unwrap();
        repo.commit(
            &format!("revision {revision}"),
            "a user",
            "an-email",
            &format!("2023-11-{:02}T09:00:00Z", revision % 28 + 1),
        )
        .unwrap();
    }
    repo.run_git(["tag", "-a", "-m", "release", "v2.0.0"])
        .unwrap();
    repo
}

/// The whole point of deltifying: a bundle of a real history has to be in the
/// same league as the one git writes, not a multiple of it.
///
/// The bound is a regression guard rather than a target. Byte parity is not
/// reachable — git's zlib is not miniz_oxide's, and git orders its delta search
/// by object size where this cannot (see [`collect_objects`]) — so what is
/// pinned is that the gap stays within a few percent instead of the four-fold
/// one that writing every object whole gives. This fixture comes out at
/// roughly 1.02x; the repository this crate lives in, which is bigger and less
/// uniform, at about 1.07x.
#[test]
fn test_bundle_size_is_in_gits_league() {
    let repo = deep_fixture();
    let odb = open_odb(&repo);
    let dir = tempfile::tempdir().unwrap();

    let refs = vec![BundleRef::new(
        "refs/tags/v2.0.0",
        rev_parse(&repo, "refs/tags/v2.0.0"),
    )];
    let ours = our_bundle(&odb, &refs);

    let theirs = dir.path().join("theirs.bundle");
    git(
        repo.location.path(),
        &[
            "bundle",
            "create",
            theirs.to_str().unwrap(),
            "refs/tags/v2.0.0",
        ],
    );
    let theirs = std::fs::metadata(&theirs).unwrap().len() as usize;

    let ratio = ours.len() as f64 / theirs as f64;
    assert!(
        ratio < 1.25,
        "our bundle is {} bytes against git's {theirs} ({ratio:.2}x)",
        ours.len()
    );
}
