//! Fixture repositories, and the webroot they are served from.
//!
//! The repositories are built with `gib-testkit`'s [`TestRepo`], which already
//! clears `GIT_AUTHOR_*`/`GIT_COMMITTER_*` from the environment and takes an
//! explicit name/email/date per commit — so the object IDs are reproducible.
//!
//! Rather than hard-coding those IDs here, every fixture reports what `git`
//! itself says about it (see [`RepoFixture::commits`]). Assertions then compare
//! the rendered page against `git`'s own output, which is both exact and immune
//! to a git upgrade shifting a hash.

use anyhow::{Context, Result, bail};
use gib_testkit::TestRepo;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A commit as `git log` reports it.
#[derive(Debug, Clone)]
pub struct CommitFacts {
    pub sha: String,
    pub subject: String,
}

impl CommitFacts {
    /// The abbreviation webgit renders in commit/log tables.
    pub fn short_sha(&self) -> &str {
        &self.sha[..8]
    }
}

/// One repository in the webroot, plus what `git` says is in it.
#[derive(Debug, Clone)]
pub struct RepoFixture {
    /// Directory name below `repos/`, e.g. `basic.git`.
    pub name: &'static str,
    /// Commits on `main`, newest first — the order the log page renders them.
    pub commits: Vec<CommitFacts>,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
}

impl RepoFixture {
    /// Path component this repo is served under. The trailing slash matters:
    /// `resolve_repo_url` keys off a URL ending in `.git` or `.git/`, and
    /// miniserve's `--index` only serves the app shell for the directory URL.
    pub fn url_path(&self) -> String {
        format!("/repos/{}/", self.name)
    }

    pub fn head(&self) -> &CommitFacts {
        &self.commits[0]
    }
}

/// The assembled webroot and the repositories inside it.
#[derive(Debug)]
pub struct Fixtures {
    pub webroot: PathBuf,
    pub basic: RepoFixture,
    pub packed: RepoFixture,
    pub graph: RepoFixture,
}

impl Fixtures {
    pub fn all(&self) -> [&RepoFixture; 3] {
        [&self.basic, &self.packed, &self.graph]
    }
}

/// Build the fixtures once per test binary. They are inert files, so unlike the
/// server and browser there is nothing to tear down and no reason to pay for
/// them per test.
pub fn get() -> Result<&'static Fixtures> {
    static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
    if let Some(f) = FIXTURES.get() {
        return Ok(f);
    }
    let built = build().context("failed to build fixtures")?;
    Ok(FIXTURES.get_or_init(|| built))
}

fn build() -> Result<Fixtures> {
    let webroot = Path::new(env!("CARGO_TARGET_TMPDIR")).join("webroot");
    if webroot.exists() {
        fs::remove_dir_all(&webroot).context("failed to clear the previous webroot")?;
    }
    fs::create_dir_all(&webroot)?;

    let dist = dist_dir()?;
    let index_html = dist.join("index.html");
    copy_file(&index_html, &webroot.join("index.html"))?;
    copy_dir(&dist.join("assets"), &webroot.join("assets"))?;

    let repos_dir = webroot.join("repos");
    fs::create_dir_all(&repos_dir)?;

    let basic = install(&repos_dir, &index_html, "basic.git", Packing::Loose)?;
    let packed = install(&repos_dir, &index_html, "packed.git", Packing::Packed)?;
    let graph = install(&repos_dir, &index_html, "graph.git", Packing::CommitGraph)?;

    // The repository index the app renders when the URL names no repo.
    fs::write(
        webroot.join("listing.json"),
        format!(
            r#"[{{"repos": ["{}", "{}", "{}"]}}]"#,
            basic.name, packed.name, graph.name
        ),
    )?;

    Ok(Fixtures {
        webroot,
        basic,
        packed,
        graph,
    })
}

/// How much of git's on-disk packing to apply, so the suite covers loose
/// objects, packfile reads over `Range`, and the commit-graph path.
#[derive(Clone, Copy)]
enum Packing {
    Loose,
    Packed,
    CommitGraph,
}

fn install(
    repos_dir: &Path,
    index_html: &Path,
    name: &'static str,
    packing: Packing,
) -> Result<RepoFixture> {
    let repo = TestRepo::new().context("git init failed")?;
    // `gc.writeCommitGraph` defaults to true, so a plain `git gc` would quietly
    // give the packed fixture a commit-graph too and erase the distinction
    // between these three. Write it only where the fixture is meant to have one.
    repo.run_git(["config", "gc.writeCommitGraph", "false"])?;
    write_history(&repo)?;

    match packing {
        Packing::Loose => {}
        Packing::Packed | Packing::CommitGraph => {
            repo.run_git(["gc"])?;
            repo.run_git(["pack-refs", "--all"])?;
        }
    }
    if let Packing::CommitGraph = packing {
        repo.run_git(["commit-graph", "write", "--reachable", "--changed-paths"])?;
    }

    // Without this there is no `objects/info/packs`, and the dumb-HTTP client
    // has no way to discover the packfiles. It is also the one setup step the
    // README asks deployments to perform.
    repo.run_git(["update-server-info"])?;

    let facts = read_facts(&repo, name)?;

    // Serve the `.git` directory itself: that is the layout the app expects
    // from a bare repository over dumb HTTP.
    let dest = repos_dir.join(name);
    copy_dir(&repo.location.path().join(".git"), &dest)?;
    // Placing the app shell *inside* the repo directory is what makes
    // `miniserve --index index.html` serve webgit at `/repos/<name>.git/` while
    // everything below it stays a real git file. The app never requests
    // `index.html` from a repo, and git does not care that it is there.
    copy_file(index_html, &dest.join("index.html"))?;

    Ok(facts)
}

/// The shared history every fixture gets. Dates are fixed and increasing so
/// ordering is stable and the rendered ages are deterministic.
fn write_history(repo: &TestRepo) -> Result<()> {
    const AUTHOR: (&str, &str) = ("A Test Author", "author@example.org");

    write_file(
        repo,
        "README.md",
        b"# browser-tests fixture\n\nA *client-side* Git viewer fixture.\n\n\
          | key | value |\n|---|---|\n| a | 1 |\n",
    )?;
    write_file(
        repo,
        "src/main.rs",
        b"fn main() {\n    println!(\"hi\");\n}\n",
    )?;
    repo.run_git(["add", "--all"])?;
    repo.commit(
        "Add README and main",
        AUTHOR.0,
        AUTHOR.1,
        "2000-01-01T00:00:00Z",
    )?;

    write_file(repo, "src/lib.rs", b"pub fn answer() -> u32 {\n    42\n}\n")?;
    repo.run_git(["add", "--all"])?;
    repo.commit(
        "Add a library module",
        AUTHOR.0,
        AUTHOR.1,
        "2000-01-02T00:00:00Z",
    )?;

    // A binary blob (embedded NULs) so the blob view's binary branch is real,
    // and a nested docs/ path for the path-scoped log.
    write_file(repo, "docs/guide.md", b"# Guide\n\nSome **docs**.\n")?;
    write_file(repo, "assets/logo.bin", &[0u8, 1, 2, 3, 0, 255, 254, 0])?;
    repo.run_git(["add", "--all"])?;
    repo.commit(
        "Add docs and a binary asset",
        AUTHOR.0,
        AUTHOR.1,
        "2000-01-03T00:00:00Z",
    )?;

    // A second branch, so refs/summary have more than one row. Built and then
    // left behind, with `main` checked out again as HEAD.
    repo.run_git(["checkout", "-b", "develop"])?;
    write_file(repo, "src/parser.rs", b"// WIP\n")?;
    repo.run_git(["add", "--all"])?;
    repo.commit(
        "Start the new parser",
        AUTHOR.0,
        AUTHOR.1,
        "2000-01-04T00:00:00Z",
    )?;
    repo.run_git(["checkout", "main"])?;

    // One annotated and one lightweight tag: they peel differently, and the
    // refs page renders them from different code paths.
    repo.tag_annotated(
        "v1.0.0",
        "HEAD",
        "Release 1.0.0",
        AUTHOR.0,
        AUTHOR.1,
        "2000-01-05T00:00:00Z",
    )?;
    repo.run_git(["tag", "v0.9", "HEAD~1"])?;

    // A note on the tip of main, so the commit page has one to render. Added
    // before any packing, which puts `refs/notes/commits` into packed-refs and
    // the note's objects into the packfile for the fixtures that get one.
    repo.run_git(["notes", "add", "-m", HEAD_NOTE])?;

    Ok(())
}

/// The note [`write_history`] attaches to the tip of `main`. Quotes a hash, as
/// notes tend to, so the commit page's linkification of them is exercised too.
pub const HEAD_NOTE: &str = "Cherry-picked from 0123abcd. Reviewed by nobody.";

/// Ask `git` what it just built, so assertions can be exact without hard-coding
/// hashes that a git upgrade could shift.
fn read_facts(repo: &TestRepo, name: &'static str) -> Result<RepoFixture> {
    // NUL-separated so a subject containing whitespace stays intact.
    let log = repo.run_git(["log", "--format=%H%x00%s", "main"])?;
    let commits = String::from_utf8(log)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (sha, subject) = line
                .split_once('\0')
                .with_context(|| format!("unparsable git log line: {line:?}"))?;
            Ok(CommitFacts {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if commits.is_empty() {
        bail!("fixture {name} has no commits");
    }

    Ok(RepoFixture {
        name,
        commits,
        branches: for_each_ref(repo, "refs/heads")?,
        tags: for_each_ref(repo, "refs/tags")?,
    })
}

fn for_each_ref(repo: &TestRepo, prefix: &str) -> Result<Vec<String>> {
    let out = repo.run_git(["for-each-ref", "--format=%(refname:short)", prefix])?;
    Ok(String::from_utf8(out)?
        .lines()
        .map(str::to_string)
        .collect())
}

fn write_file(repo: &TestRepo, rel: &str, content: &[u8]) -> Result<()> {
    let path = repo.location.path().join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Locate `dist/`, the directory `trunk build` produces. Its absence is the
/// single most likely reason for a fresh checkout to fail, so say so plainly.
///
/// `WEBGIT_DIST` overrides the default location. The container sets it, because
/// it mounts the source read-only and builds into a cache volume instead.
fn dist_dir() -> Result<PathBuf> {
    let dist = match std::env::var_os("WEBGIT_DIST") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("dist"),
    };
    if !dist.join("index.html").is_file() || !dist.join("assets").is_dir() {
        bail!(
            "{} does not look like a trunk build output — run `trunk build --release` first \
             (scripts/browser-tests.sh does this for you)",
            dist.display()
        );
    }
    dist.canonicalize().context("failed to resolve dist/")
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to).with_context(|| format!("failed to copy {}", from.display()))?;
    Ok(())
}

/// Copy rather than symlink: miniserve has a `--no-symlinks` mode, and the
/// fixtures are small enough that copying removes the question entirely.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
