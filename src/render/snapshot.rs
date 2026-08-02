//! The snapshot view: the page you land on while a `.tar.gz` is being built,
//! and the download once it is.
//!
//! Building one is almost entirely object fetching, and on a large repository
//! that is long enough to need saying out loud, so the view has a progress bar
//! of its own: how many objects have been fetched out of how many the walk has
//! asked for so far. It is not the chrome's persistent fetch line, which counts
//! every request the page has ever made and can't say when this archive is
//! done.

use crate::archive::{ArchiveEntry, EntryKind, collect_entries, stream_tar_gz};
use crate::cache::CachingRepo;
use crate::render::{use_blob_url, yield_to_browser};
use crate::stats::format_bytes;
use git_async::object::{Commit, Tree};
use std::cell::Cell;
use wasm_bindgen::JsCast;
use web_sys::Blob;
use yew::prelude::*;

/// A snapshot in progress, or one that is ready to download.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct SnapshotProps {
    /// The archive's file name, e.g. `webgit-main.tar.gz`. Known before the
    /// first object is fetched, so the page can say what it is building.
    pub name: String,
    pub state: SnapshotState,
}

/// How far the snapshot has got.
#[derive(PartialEq, Clone)]
pub(crate) enum SnapshotState {
    /// Still walking. `total` is how many objects have been requested so far,
    /// which grows as the walk uncovers more of the tree — see `archive`'s
    /// `Progress` for why there is no fixed denominator to show instead.
    Building { fetched: usize, total: usize },
    /// Fetched, now being written and gzipped. Unlike the walk this has a
    /// denominator that was known before it started: every entry is in hand.
    Compressing { written: usize, total: usize },
    /// Built.
    Ready {
        /// The gzipped tar, as the browser handed it back. A `Blob` is a handle
        /// — the bytes live wherever the browser put them, which for a large
        /// archive may be disk rather than memory — so the props clone on every
        /// re-render costs nothing, and neither does keeping it around.
        archive: Blob,
        /// The archive's size. Read off the blob once, here, rather than at
        /// every render.
        size: usize,
        /// How many files (not directories) went into it.
        files: usize,
    },
}

/// How often, in milliseconds of wall time, to re-render the progress bar.
///
/// The walk reports every object, which on a cached repository is a burst far
/// faster than anything can be seen — and each report costs a re-render. Same
/// reasoning, and the same interval, as the commit view's streamed diff.
const PROGRESS_EMIT_INTERVAL_MS: f64 = 50.0;

/// Walk `tree`, build the archive, and describe it.
///
/// `ref_label` is the ref the tree was reached by, which only affects what the
/// file is called; `commit` supplies the id recorded in the archive and the
/// timestamp stamped on its entries. `on_partial` is called with the building
/// state as objects land, and is what puts the progress bar on screen.
pub(crate) async fn build_snapshot(
    repo: &CachingRepo,
    tree: &Tree,
    commit: &Commit,
    ref_label: &str,
    clone_url: &str,
    on_partial: &dyn Fn(SnapshotProps),
) -> anyhow::Result<SnapshotProps> {
    let stem = snapshot_stem(clone_url, ref_label);
    let name = format!("{stem}.tar.gz");
    let building = |fetched, total| SnapshotProps {
        name: name.clone(),
        state: SnapshotState::Building { fetched, total },
    };

    // An empty bar before the first object lands, rather than the route's
    // loading dots: the walk starts by reading the root tree, and on a slow
    // connection that alone is a visible wait.
    on_partial(building(0, 0));

    // Repainting on every object would cost a render per fetch, so emits are
    // rate-limited on wall time. The counts themselves are recorded every time,
    // so the phase's last emit below is the real total even when the tick that
    // would have carried it was skipped.
    let seen = Cell::new((0usize, 0usize));
    let last_emit = Cell::new(0.0f64);
    let due = || {
        let now = js_sys::Date::now();
        let due = now - last_emit.get() >= PROGRESS_EMIT_INTERVAL_MS;
        if due {
            last_emit.set(now);
        }
        due
    };

    let entries = collect_entries(repo, tree, "", &|fetched, total| {
        seen.set((fetched, total));
        if due() {
            on_partial(building(fetched, total));
        }
    })
    .await?;

    let (fetched, total) = seen.get();
    on_partial(building(fetched, total));

    let files = count_files(&entries);
    let compressing = |written, total| SnapshotProps {
        name: name.clone(),
        state: SnapshotState::Compressing { written, total },
    };
    on_partial(compressing(0, entries.len()));
    // So the switch of phase is actually seen: everything below this point runs
    // off promise resolutions, which don't let the browser paint on their own.
    yield_to_browser().await;

    let archive = stream_tar_gz(
        entries,
        &format!("{stem}/"),
        &commit.id().to_string(),
        // A commit before the epoch has no sensible tar mtime; clamp rather
        // than wrap it into a date in 2106.
        commit.commit_date().timestamp().as_second().max(0) as u64,
        // Unthrottled, unlike the walk's: this one already reports on a
        // wall-clock budget, since it has to pace its repaints anyway.
        &|written, total| on_partial(compressing(written, total)),
    )
    .await?;

    Ok(SnapshotProps {
        name,
        state: SnapshotState::Ready {
            size: archive.size() as usize,
            archive,
            files,
        },
    })
}

/// How many of `entries` are files, which is what the view reports: the
/// directories are structure rather than content.
fn count_files(entries: &[ArchiveEntry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::File { .. }))
        .count()
}

/// The repository's own name, from its URL: the last path component, without
/// the `.git` suffix — `…/public/webgit.git/` becomes `webgit`.
fn repo_stem(clone_url: &str) -> String {
    let trimmed = clone_url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        "repository".to_string()
    } else {
        name.to_string()
    }
}

/// The archive's name and its top-level directory (they are the same string):
/// `<repo>-<ref>`, as cgit names its snapshots.
///
/// Both halves are flattened to a plain file name, since a ref may contain `/`
/// (`release/2.0`), which can't appear in something the browser is about to
/// save to disk.
pub(crate) fn snapshot_stem(clone_url: &str, ref_label: &str) -> String {
    format!("{}-{}", flatten(&repo_stem(clone_url)), flatten(ref_label))
}

/// What the browser will save a snapshot of `ref_label` as.
///
/// Shared with the download links in the ref tables, which show the file name
/// rather than a bare "tar.gz" — so what the link says and what lands in the
/// downloads folder are the same string by construction.
pub(crate) fn snapshot_file_name(clone_url: &str, ref_label: &str) -> String {
    format!("{}.tar.gz", snapshot_stem(clone_url, ref_label))
}

fn flatten(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if matches!(c, '/' | '\\') { '-' } else { c })
        .collect()
}

/// The Yew component used to mount the snapshot view.
///
/// The two states are separate components rather than two branches of one, so
/// that the object-URL and auto-download hooks only exist once there is an
/// archive: run while building, they would mint a URL to an empty file and save
/// it. Yew forbids hooks under a condition, but it is happy to render a
/// different child.
#[function_component(SnapshotView)]
pub(crate) fn snapshot_view_component(props: &SnapshotProps) -> Html {
    match &props.state {
        SnapshotState::Building { fetched, total } => {
            progress_view(&props.name, "fetching objects", *fetched, *total)
        }
        SnapshotState::Compressing { written, total } => {
            progress_view(&props.name, "compressing", *written, *total)
        }
        SnapshotState::Ready {
            archive,
            size,
            files,
        } => html! {
            <ReadySnapshot
                name={props.name.clone()}
                archive={archive.clone()}
                size={*size}
                files={*files}
            />
        },
    }
}

/// The archive being built: what stage it is at, and a bar that fills as the
/// work lands.
///
/// A `<progress>` element rather than a `<div>` whose width is set on the tag,
/// because the CSP forbids inline styles — and this way the numbers are on the
/// element itself, so a screen reader gets them without the text having to be
/// read out again.
fn progress_view(name: &str, stage: &str, done: usize, total: usize) -> Html {
    html! {
        <div class="snapshot">
            <p class="snapshot-info">
                { name }{ " \u{2014} " }{ format!("{stage}\u{2026} {done}/{total}") }
            </p>
            <progress
                class="snapshot-progress"
                value={done.to_string()}
                // A max of zero is not a valid progress element, and before the
                // first object is queued that is exactly where the walk is.
                max={total.max(1).to_string()}
            >
                { format!("{done}/{total}") }
            </progress>
        </div>
    }
}

/// Props for [`ReadySnapshot`]: the finished archive.
#[derive(Properties, PartialEq, Clone)]
struct ReadyProps {
    name: String,
    archive: Blob,
    size: usize,
    files: usize,
}

/// The finished snapshot. Like [`crate::render::blob::BlobView`] it mints the
/// object URL in an effect and passes it in, keeping the markup a plain
/// function of its inputs.
#[function_component(ReadySnapshot)]
fn ready_snapshot(props: &ReadyProps) -> Html {
    let url = use_blob_url(&props.archive);
    use_auto_download(&url, &props.name);
    ready_view(&props.name, props.size, props.files, &url)
}

/// The markup for a built archive. `url` is an object URL over it, or empty if
/// one couldn't be made (under SSR, or if the browser refused), in which case
/// the link is omitted rather than emitted pointing at the page.
fn ready_view(name: &str, size: usize, files: usize, url: &str) -> Html {
    let summary = format!(
        "{} file{}, {}",
        files,
        if files == 1 { "" } else { "s" },
        format_bytes(size as u64)
    );

    html! {
        <div class="snapshot">
            <p class="snapshot-info">
                { name }{ " \u{2014} " }{ summary }
            </p>
            if url.is_empty() {
                <p class="msg error">
                    { "This browser wouldn't hand over the archive to download." }
                </p>
            } else {
                <p class="msg">
                    { "The download should have started. If it didn't, " }
                    <a class="snapshot-download" href={url.to_string()} download={name.to_string()}>
                        { "save it here" }
                    </a>
                    { "." }
                </p>
            }
        </div>
    }
}

/// Start the download as soon as the archive has a URL, so that following the
/// link from the tree view saves a file rather than parking on another page.
/// The visible link stays as the fallback for a browser that declines.
#[hook]
fn use_auto_download(url: &str, name: &str) {
    use_effect_with(
        (url.to_string(), name.to_string()),
        |(url, name): &(String, String)| {
            if !url.is_empty() {
                click_download(url, name);
            }
            || ()
        },
    );
}

/// Click a detached `<a download>`, the one way to start a download that isn't
/// a navigation. Nothing is done on failure: the view's own link is the
/// fallback, and it is already on screen.
fn click_download(url: &str, name: &str) {
    let anchor = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.create_element("a").ok())
        .and_then(|e| e.dyn_into::<web_sys::HtmlAnchorElement>().ok());
    if let Some(anchor) = anchor {
        anchor.set_href(url);
        anchor.set_download(name);
        anchor.click();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::EntryKind;

    #[test]
    fn test_repo_stem() {
        assert_eq!(repo_stem("https://example.org/public/webgit.git"), "webgit");
        assert_eq!(
            repo_stem("https://example.org/public/webgit.git/"),
            "webgit"
        );
        assert_eq!(repo_stem("https://example.org/public/webgit"), "webgit");
        assert_eq!(repo_stem("webgit.git"), "webgit");
        // Nothing left to name it after.
        assert_eq!(repo_stem("https://example.org/"), "example.org");
        assert_eq!(repo_stem(""), "repository");
    }

    #[test]
    fn test_snapshot_stem() {
        let url = "https://example.org/public/webgit.git";
        assert_eq!(snapshot_stem(url, "v1.0.0"), "webgit-v1.0.0");
        // A ref name with a slash in it is still one file name.
        assert_eq!(snapshot_stem(url, "release/2.0"), "webgit-release-2.0");
    }

    /// The link text in the ref tables, and what the browser saves.
    #[test]
    fn test_snapshot_file_name() {
        assert_eq!(
            snapshot_file_name("https://example.org/public/webgit.git", "v1.0.0"),
            "webgit-v1.0.0.tar.gz"
        );
    }

    /// Render a finished snapshot's markup to a static HTML string via SSR. See
    /// the equivalent helper in `render::tag` for why we go through SSR.
    fn render(name: &str, bytes: usize, files: usize, url: &str) -> String {
        let (name, url) = (name.to_string(), url.to_string());
        let html = futures::executor::block_on(
            yew::ServerRenderer::<SvHost>::with_props(move || SvHostProps {
                name,
                size: bytes,
                files,
                url,
            })
            .hydratable(false)
            .render(),
        );
        html.replace("><", ">\n<")
    }

    // A host component so the plain `ready_view` fn can go through SSR with an
    // object URL supplied, which the real component only has in a browser.
    #[derive(Properties, PartialEq, Clone)]
    struct SvHostProps {
        name: String,
        size: usize,
        files: usize,
        url: String,
    }

    #[function_component(SvHost)]
    fn sv_host(p: &SvHostProps) -> Html {
        ready_view(&p.name, p.size, p.files, &p.url)
    }

    /// The building state goes through the real component: it has no object
    /// URL to stand in for, so nothing needs hosting.
    /// The state is built by a closure rather than passed in, because
    /// `SnapshotState::Ready` holds a `Blob` and so the enum isn't `Send` — the
    /// same reason `render` above builds its props inside the closure. The
    /// in-progress states capture nothing but numbers.
    fn render_state(state: impl FnOnce() -> SnapshotState + Send + 'static) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<SnapshotView>::with_props(move || SnapshotProps {
                name: "webgit-main.tar.gz".to_string(),
                state: state(),
            })
            .hydratable(false)
            .render(),
        );
        html.replace("><", ">\n<")
    }

    fn render_building(fetched: usize, total: usize) -> String {
        render_state(move || SnapshotState::Building { fetched, total })
    }

    #[test]
    fn test_snapshot_html() {
        insta::assert_snapshot!(render("webgit-main.tar.gz", 4096, 12, "blob:fake"));
    }

    /// Without a URL there is nothing to link to, so the view says so instead
    /// of rendering a link back to the page it is already on.
    #[test]
    fn test_snapshot_html_no_url() {
        insta::assert_snapshot!(render("webgit-main.tar.gz", 4096, 12, ""));
    }

    /// One file, and the "files" in the summary line is singular.
    #[test]
    fn test_snapshot_html_single_file() {
        insta::assert_snapshot!(render("webgit-main-src.tar.gz", 12, 1, "blob:fake"));
    }

    /// Mid-walk: the counts, and a bar at the ratio between them.
    #[test]
    fn test_snapshot_html_building() {
        insta::assert_snapshot!(render_building(37, 120));
    }

    /// The very first paint, before anything has been queued. `max` is floored
    /// at 1 because `<progress max="0">` isn't valid, and the bar is empty
    /// either way.
    #[test]
    fn test_snapshot_html_building_empty() {
        insta::assert_snapshot!(render_building(0, 0));
    }

    /// The second phase: everything is fetched and the archive is being written
    /// into the encoder. Same bar, and unlike the walk this denominator is
    /// fixed — the entries are all in hand before it starts.
    #[test]
    fn test_snapshot_html_compressing() {
        insta::assert_snapshot!(render_state(|| SnapshotState::Compressing {
            written: 4096,
            total: 13658,
        }));
    }

    /// Directories and symlinks are archived, but the count the view reports is
    /// of files.
    #[test]
    fn test_count_files() {
        let entries = vec![
            ArchiveEntry {
                path: "src".to_string(),
                kind: EntryKind::Directory,
                data: Vec::new(),
            },
            ArchiveEntry {
                path: "src/lib.rs".to_string(),
                kind: EntryKind::File { executable: false },
                data: b"x".to_vec(),
            },
            ArchiveEntry {
                path: "src/build.sh".to_string(),
                kind: EntryKind::File { executable: true },
                data: b"x".to_vec(),
            },
            ArchiveEntry {
                path: "link".to_string(),
                kind: EntryKind::Symlink {
                    target: b"src/lib.rs".to_vec(),
                },
                data: Vec::new(),
            },
        ];
        assert_eq!(count_files(&entries), 2);
    }
}
