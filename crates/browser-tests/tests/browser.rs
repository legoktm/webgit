//! End-to-end tests: headless Firefox against the real `trunk build` output.
//!
//! Run them with scripts/browser-tests.sh, which builds `dist/` and supplies
//! Firefox, geckodriver and miniserve inside a container. Directly:
//!
//! ```sh
//! cargo test -p browser-tests --features browser -- --test-threads=1
//! ```
//!
//! `--test-threads=1` is required: geckodriver serves one session at a time.

mod harness;

use anyhow::Result;
use fantoccini::Locator;
use harness::{Harness, RepoFixture, fixtures, server};
use std::time::Duration;

/// The harness's own smoke test, and the reason it is first.
///
/// webgit fetches git objects with byte ranges, but `classify()` in
/// `src/fetch.rs` deliberately accepts a 200 answer to a range request and
/// slices the body client-side — servers and CDNs are allowed to ignore
/// `Range`. That tolerance means a server which never returns 206 produces a
/// *completely green* suite that has not once exercised the path webgit's
/// design rests on. So assert the server's behaviour directly, before drawing
/// any conclusion from the tests that follow.
#[test]
fn fixture_server_answers_range_requests_with_206() -> Result<()> {
    let fixtures = fixtures::get()?;
    let server = server::Server::start(&fixtures.webroot)?;

    let whole = server::get(server.port(), "/index.html", None)?;
    assert_eq!(whole.status, 200, "plain GET should succeed");
    assert!(whole.body.len() > 32, "index.html is unexpectedly small");

    let ranged = server::get(server.port(), "/index.html", Some("bytes=0-9"))?;
    assert_eq!(
        ranged.status, 206,
        "the fixture server ignored `Range` and answered {} — every other test in \
         this file would still pass while silently exercising the client-side \
         fallback instead of the 206 path",
        ranged.status
    );
    assert_eq!(
        ranged.header("Content-Range"),
        Some(format!("bytes 0-9/{}", whole.body.len()).as_str()),
        "Content-Range did not describe the slice that was asked for"
    );
    assert_eq!(ranged.body, &whole.body[..10], "wrong bytes came back");

    Ok(())
}

macro_rules! route_test {
    ($name:ident, $check:ident) => {
        #[tokio::test]
        async fn $name() -> Result<()> {
            let h = Harness::start().await?;
            for repo in h.fixtures.all() {
                $check(&h, repo).await?;
            }
            h.finish().await
        }
    };
}

route_test!(summary_renders_real_content, check_summary);
route_test!(log_renders_real_content, check_log);
route_test!(tree_renders_real_content, check_tree);
route_test!(blob_renders_real_content, check_blob);
route_test!(commit_renders_real_content, check_commit);
route_test!(refs_render_real_content, check_refs);
route_test!(about_renders_real_content, check_about);

async fn check_summary(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/summary").await?;
    h.wait_for(".summary-table").await?;
    h.assert_no_error().await?;

    let clone_url = h.text_of(".clone-url").await?;
    assert!(
        clone_url.contains(repo.name),
        "[{}] clone URL did not name the repo: {clone_url}",
        repo.name
    );

    let text = h.content_text().await?;
    for branch in &repo.branches {
        assert!(
            text.contains(branch.as_str()),
            "[{}] summary omitted branch {branch}",
            repo.name
        );
    }
    assert!(
        text.contains(&repo.head().subject),
        "[{}] summary omitted the HEAD commit subject",
        repo.name
    );
    Ok(())
}

async fn check_log(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/log").await?;
    h.wait_for(".summary-table").await?;
    h.assert_no_error().await?;

    // The log table renders abbreviated hashes in `td.name`; compare against
    // what `git log` reported for this fixture, in the same order.
    let shown = h.texts_of(".summary-table td.name").await?;
    let expected: Vec<&str> = repo.commits.iter().map(|c| c.short_sha()).collect();
    assert_eq!(
        shown.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "[{}] log did not match `git log` on main",
        repo.name
    );

    // The message cell carries the subject followed by any ref decorations for
    // that commit — "Add docs and a binary asset main v1.0.0" — so match on the
    // subject as a prefix rather than the whole cell.
    let subjects = h.texts_of(".summary-table td.msg").await?;
    assert_eq!(
        subjects.len(),
        repo.commits.len(),
        "[{}] log rendered {} rows for {} commits",
        repo.name,
        subjects.len(),
        repo.commits.len()
    );
    for (shown, commit) in subjects.iter().zip(repo.commits.iter()) {
        assert!(
            shown.starts_with(&commit.subject),
            "[{}] log row {shown:?} did not start with git's subject {:?}",
            repo.name,
            commit.subject
        );
    }

    // HEAD is decorated with the branch and the tag pointing at it, which is
    // the decoration path itself rather than just the subject.
    assert!(
        subjects[0].contains("main") && subjects[0].contains("v1.0.0"),
        "[{}] the HEAD row lost its ref decorations: {:?}",
        repo.name,
        subjects[0]
    );
    Ok(())
}

async fn check_tree(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/tree").await?;
    h.wait_for(".tree-table").await?;
    h.assert_no_error().await?;

    let names = h.texts_of(".tree-table td.name").await?;
    for expected in ["README.md", "src", "docs", "assets"] {
        assert!(
            names.iter().any(|n| n == expected),
            "[{}] tree is missing {expected}: {names:?}",
            repo.name
        );
    }

    // Descend into a subdirectory by URL, which the path bar also tracks.
    h.open(repo, "#!/tree/src").await?;
    h.wait_for(".tree-table").await?;
    let names = h.texts_of(".tree-table td.name").await?;
    assert!(
        names.iter().any(|n| n == "main.rs") && names.iter().any(|n| n == "lib.rs"),
        "[{}] src/ listing was wrong: {names:?}",
        repo.name
    );
    Ok(())
}

async fn check_blob(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/tree/src/lib.rs").await?;
    h.wait_for(".blob-table").await?;
    h.assert_no_error().await?;

    // Compare trimmed: WebDriver's text accessor normalises whitespace, so the
    // file's indentation is not reliably observable through it. The line count
    // and content still pin down that this is the real blob.
    let code: Vec<String> = h
        .texts_of(".blob-table td.code")
        .await?
        .iter()
        .map(|l| l.trim().to_string())
        .collect();
    assert_eq!(
        code,
        vec!["pub fn answer() -> u32 {", "42", "}"],
        "[{}] blob did not render the file's real contents",
        repo.name
    );
    Ok(())
}

async fn check_commit(h: &Harness, repo: &RepoFixture) -> Result<()> {
    let head = repo.head();
    h.open(repo, &format!("#!/commit/{}", head.sha)).await?;
    h.wait_for(".tag-table").await?;
    h.assert_no_error().await?;

    let text = h.content_text().await?;
    assert!(
        text.contains(&head.sha),
        "[{}] commit page did not show the full hash",
        repo.name
    );
    assert!(
        text.contains(&head.subject),
        "[{}] commit page did not show the subject",
        repo.name
    );
    assert!(
        text.contains("A Test Author"),
        "[{}] commit page did not show the author",
        repo.name
    );
    Ok(())
}

async fn check_refs(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/refs").await?;
    h.wait_for(".summary-table").await?;
    h.assert_no_error().await?;

    let text = h.content_text().await?;
    for name in repo.branches.iter().chain(repo.tags.iter()) {
        assert!(
            text.contains(name.as_str()),
            "[{}] refs page omitted {name}",
            repo.name
        );
    }
    Ok(())
}

async fn check_about(h: &Harness, repo: &RepoFixture) -> Result<()> {
    h.open(repo, "#!/about").await?;
    h.wait_for(".tag-table").await?;
    h.assert_no_error().await?;

    // The about page reports counts, which is a cheap cross-check that the app
    // enumerated exactly the refs git says exist.
    let rows = h.texts_of(".tag-table tr").await?;
    let find = |label: &str| {
        rows.iter()
            .find(|r| r.starts_with(label))
            .cloned()
            .unwrap_or_default()
    };
    assert!(
        find("branches").contains(&repo.branches.len().to_string()),
        "[{}] about page branch count disagreed with git ({} expected): {:?}",
        repo.name,
        repo.branches.len(),
        find("branches")
    );
    assert!(
        find("tags").contains(&repo.tags.len().to_string()),
        "[{}] about page tag count disagreed with git ({} expected): {:?}",
        repo.name,
        repo.tags.len(),
        find("tags")
    );
    Ok(())
}

/// Clicking the nav and using browser history, rather than jumping by URL.
/// This is what exercises the `hashchange` listener the app registers on mount.
#[tokio::test]
async fn nav_clicks_and_history_drive_routing() -> Result<()> {
    let h = Harness::start().await?;
    let repo = &h.fixtures.basic;

    h.open(repo, "#!/summary").await?;
    h.wait_for(".summary-table").await?;

    // Click through to the tree tab.
    h.client
        .find(Locator::Css("#nav a[href='#!/tree']"))
        .await?
        .click()
        .await?;
    h.wait_for(".tree-table").await?;
    assert_eq!(
        h.hash().await?,
        "#!/tree",
        "clicking nav did not set the hash"
    );

    let active = h.text_of("#nav a.nav-tab.active").await?;
    assert_eq!(
        active, "tree",
        "the active nav tab did not follow the route"
    );

    // Follow a link out of the tree into a blob.
    h.client
        .find(Locator::Css(".tree-table a[href='#!/tree/README.md']"))
        .await?
        .click()
        .await?;
    h.wait_for(".blob-table").await?;
    h.assert_no_error().await?;

    // Back to the tree, then forward again — both are hashchange, not a reload.
    h.client.back().await?;
    h.wait_for(".tree-table").await?;
    assert_eq!(
        h.hash().await?,
        "#!/tree",
        "back did not restore the tree route"
    );

    h.client.forward().await?;
    h.wait_for(".blob-table").await?;
    assert_eq!(
        h.hash().await?,
        "#!/tree/README.md",
        "forward did not restore the blob route"
    );
    h.assert_no_error().await?;

    h.finish().await
}

/// A second visit should be served from IndexedDB rather than the network.
///
/// Measured with the Resource Timing API: timings are per-document and reset on
/// reload, so the first load and the reload can be compared directly.
#[tokio::test]
async fn indexeddb_cache_removes_object_fetches_on_reload() -> Result<()> {
    let h = Harness::start().await?;
    let repo = &h.fixtures.packed;

    h.open(repo, "#!/log").await?;
    h.wait_for(".summary-table").await?;
    let first = h.fetched_object_urls(repo).await?;
    assert!(
        !first.is_empty(),
        "the first load fetched nothing from the repo — the measurement is broken, \
         not the cache"
    );

    // The banner is only shown when IndexedDB is unavailable; if it is visible
    // the cache was never in play and the comparison below means nothing.
    let banner = h.client.find(Locator::Css("#idb-warning")).await?;
    let classes = banner.attr("class").await?.unwrap_or_default();
    assert!(
        classes.contains("hide"),
        "IndexedDB was unavailable in this session, so caching could not be tested"
    );

    h.reload().await?;
    h.wait_for(".summary-table").await?;
    let second = h.fetched_object_urls(repo).await?;

    assert!(
        second.len() < first.len(),
        "the reload fetched {} object URLs against {} on the first load — \
         IndexedDB caching is not taking effect.\nfirst: {first:#?}\nsecond: {second:#?}",
        second.len(),
        first.len()
    );

    h.finish().await
}

/// The readme renders into a sandboxed iframe, styled by a stylesheet the
/// post-build hook demotes from a `<link>` to a `<meta>` so the page does not
/// apply it. This is the end-to-end check that `assets::init()` and
/// scripts/postbuild.py still agree about that hashed URL.
#[tokio::test]
async fn readme_renders_in_a_sandboxed_frame_with_hashed_css() -> Result<()> {
    let h = Harness::start().await?;
    let repo = &h.fixtures.basic;

    h.open(repo, "#!/readme").await?;
    let frame = h.wait_for("iframe.markdown-frame").await?;
    h.assert_no_error().await?;

    let sandbox = frame.attr("sandbox").await?.unwrap_or_default();
    assert!(
        !sandbox.contains("allow-scripts"),
        "the readme frame must not be allowed to run scripts, got: {sandbox}"
    );

    let srcdoc = frame.attr("srcdoc").await?.unwrap_or_default();
    assert!(
        srcdoc.contains("<h1>browser-tests fixture</h1>"),
        "the frame did not contain the rendered README markdown"
    );

    // The stylesheet href must be the real hashed asset, not the empty string
    // the SSR snapshots show when no asset map has been initialised.
    let css = srcdoc
        .split_once(r#"<link rel="stylesheet" href=""#)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(href, _)| href.to_string())
        .unwrap_or_default();
    assert!(
        css.starts_with("/assets/markdown-") && css.ends_with(".css"),
        "the readme frame did not resolve markdown.css to its hashed URL, got {css:?} — \
         assets::init() and scripts/postbuild.py have diverged"
    );

    // And that URL must actually serve something.
    let response = server::get(h.server.port(), &css, None)?;
    assert_eq!(response.status, 200, "the hashed markdown.css URL 404s");

    h.finish().await
}

/// The repository index — the route taken when the URL names no repository.
#[tokio::test]
async fn repository_index_lists_the_fixtures() -> Result<()> {
    let h = Harness::start().await?;

    h.open_index().await?;
    h.wait_for(".repo-listing").await?;
    h.assert_no_error().await?;

    let names = h.texts_of(".repo-listing td.name").await?;
    for repo in h.fixtures.all() {
        assert!(
            names.iter().any(|n| n == repo.name),
            "the index did not list {}: {names:?}",
            repo.name
        );
    }

    // The listing's links must be the ones that actually resolve to a repo.
    h.client
        .find(Locator::Css(".repo-listing a[href='/repos/basic.git/']"))
        .await?
        .click()
        .await?;
    h.wait_for(".summary-table").await?;
    h.assert_no_error().await?;

    h.finish().await
}

/// Snapshots are built in the browser and handed to the user as a blob
/// download — there is no server-side archive endpoint to stand in for it.
#[tokio::test]
async fn snapshot_route_downloads_a_tarball() -> Result<()> {
    let h = Harness::start().await?;
    let repo = &h.fixtures.basic;

    h.open(repo, "#!/snapshot?h=v1.0.0").await?;

    // The page reports progress ("fetching objects… 1/4", then "compressing…")
    // before it finishes. The download link only exists in the finished state,
    // so wait for that rather than racing the status text.
    h.wait_for(".snapshot-download").await?;
    h.assert_no_error().await?;

    let info = h.text_of(".snapshot-info").await?;
    assert!(
        info.contains("v1.0.0") && info.contains("files"),
        "the snapshot page did not report what it built: {info}"
    );

    let file = wait_for_download(&h.downloads, Duration::from_secs(30))
        .unwrap_or_else(|| panic!("no snapshot landed in {}", h.downloads.display()));
    let name = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    assert!(
        name.ends_with(".tar.gz"),
        "downloaded file was not a tarball: {name}"
    );
    let size = std::fs::metadata(&file)?.len();
    assert!(size > 0, "the downloaded snapshot {name} was empty");

    h.finish().await
}

/// Poll for a completed download. Firefox writes `.part` alongside the target
/// while a transfer is in flight, so ignore those.
fn wait_for_download(dir: &std::path::Path, timeout: Duration) -> Option<std::path::PathBuf> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "part") {
                    continue;
                }
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// `?h=` takes an abbreviated commit, not just a full 40-character hash — the
/// form a reader copies out of a log row or a commit message.
#[tokio::test]
async fn h_takes_an_abbreviated_commit() -> Result<()> {
    let h = Harness::start().await?;

    for repo in [&h.fixtures.packed, &h.fixtures.graph] {
        let head = repo.head();
        let abbrev = &head.sha[..10];

        h.open(repo, &format!("#!/tree?h={abbrev}")).await?;
        h.wait_for(".tree-table").await?;
        h.assert_no_error().await?;

        let names = h.texts_of(".tree-table td.name").await?;
        assert!(
            names.iter().any(|n| n == "README.md"),
            "[{}] ?h={abbrev} did not list HEAD's tree: {names:?}",
            repo.name
        );

        // The abbreviation is expanded, not echoed: the path bar labels it a
        // commit by the same eight characters the full hash would produce.
        let bar = h.text_of("#path-bar").await?;
        assert!(
            bar.contains(&format!("commit: {}", head.short_sha())),
            "[{}] path bar did not name the commit ?h={abbrev} resolved to: {bar:?}",
            repo.name
        );
    }

    h.finish().await
}

/// An abbreviation matching nothing is reported rather than silently falling
/// back to HEAD, which would show a tree the URL never asked for.
#[tokio::test]
async fn h_reports_an_unknown_abbreviated_commit() -> Result<()> {
    let h = Harness::start().await?;
    let repo = &h.fixtures.packed;

    h.open(repo, "#!/tree?h=deadbeef").await?;
    h.wait_for(".msg.error").await?;

    let msg = h.text_of(".msg.error").await?;
    assert!(
        msg.contains("deadbeef"),
        "the error did not name the revision that failed: {msg:?}"
    );

    h.finish().await
}
