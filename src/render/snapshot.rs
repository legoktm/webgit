//! The snapshot view: the page you land on while a download is being built,
//! and the download once it is.
//!
//! Two things are built here, and the view is the same for both. A `.tar.gz` is
//! one revision's tree; a `.bundle` is every object a ref reaches, in the file
//! `git clone` clones from. They differ in what the two phases are doing — see
//! [`stage_labels`] — and in what the finished file is counted in, files or
//! objects.
//!
//! Building either is almost entirely object fetching, and on a large
//! repository that is long enough to need saying out loud, so the view has a
//! progress bar of its own: how many objects have been fetched out of how many
//! the walk has asked for so far. It is not the chrome's persistent fetch line,
//! which counts every request the page has ever made and can't say when this
//! download is done.

use crate::archive::stream_tar_gz;
use crate::bundle::stream_bundle;
use crate::cache::CachingRepo;
use crate::render::{click_download, use_blob_url, yield_to_browser};
use crate::route::SnapshotFormat;
use crate::stats::format_bytes;
use gib::object::{Commit, Tree};
use gib_archive::{ArchiveEntry, EntryKind, collect_entries};
use gib_bundle::{BundleRef, collect_objects};
use std::cell::Cell;
use web_sys::Blob;
use yew::prelude::*;

/// A snapshot in progress, or one that is ready to download.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct SnapshotProps {
    /// The file's name, e.g. `webgit-main.tar.gz`. Known before the first
    /// object is fetched, so the page can say what it is building.
    pub name: String,
    /// Which of the two things is being built, which is what the phase labels
    /// and the finished summary read off.
    pub format: SnapshotFormat,
    pub state: SnapshotState,
}

/// How far the snapshot has got.
#[derive(PartialEq, Clone)]
pub(crate) enum SnapshotState {
    /// Still walking. `total` is how many objects have been requested so far,
    /// which grows as the walk uncovers more of the tree — see `archive`'s
    /// `Progress` for why there is no fixed denominator to show instead.
    Building {
        fetched: usize,
        total: usize,
    },
    Writing {
        written: usize,
        total: usize,
    },
    /// Built.
    Ready {
        archive: Blob,
        /// The file's size. Read off the blob once, here, rather than at every
        /// render.
        size: usize,
        count: usize,
    },
}

fn stage_labels(format: SnapshotFormat) -> (&'static str, &'static str) {
    match format {
        SnapshotFormat::TarGz => ("fetching objects", "compressing"),
        SnapshotFormat::Bundle => ("enumerating objects", "writing objects"),
    }
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
    repo_name: &str,
    on_partial: &dyn Fn(SnapshotProps),
) -> anyhow::Result<SnapshotProps> {
    let format = SnapshotFormat::TarGz;
    let stem = snapshot_stem(repo_name, ref_label);
    let name = snapshot_file_name(repo_name, ref_label, format);
    let building = |fetched, total| SnapshotProps {
        name: name.clone(),
        format,
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
    let writing = |written, total| SnapshotProps {
        name: name.clone(),
        format,
        state: SnapshotState::Writing { written, total },
    };
    on_partial(writing(0, entries.len()));
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
        &|written, total| on_partial(writing(written, total)),
    )
    .await?;

    Ok(SnapshotProps {
        name,
        format,
        state: SnapshotState::Ready {
            size: archive.size() as usize,
            archive,
            count: files,
        },
    })
}

/// Walk the history `refs` reaches, build the bundle, and describe it.
pub(crate) async fn build_bundle(
    repo: &CachingRepo,
    refs: Vec<BundleRef>,
    ref_label: &str,
    repo_name: &str,
    on_partial: &dyn Fn(SnapshotProps),
) -> anyhow::Result<SnapshotProps> {
    let format = SnapshotFormat::Bundle;
    let name = snapshot_file_name(repo_name, ref_label, format);
    let building = |fetched, total| SnapshotProps {
        name: name.clone(),
        format,
        state: SnapshotState::Building { fetched, total },
    };
    on_partial(building(0, 0));

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

    let tips: Vec<_> = refs.iter().map(|r| r.id).collect();
    let ids = collect_objects(repo, &tips, &|fetched, total| {
        seen.set((fetched, total));
        if due() {
            on_partial(building(fetched, total));
        }
    })
    .await?;

    let (fetched, total) = seen.get();
    on_partial(building(fetched, total));

    let count = ids.len();
    let writing = |written, total| SnapshotProps {
        name: name.clone(),
        format,
        state: SnapshotState::Writing { written, total },
    };
    on_partial(writing(0, count));
    // So the switch of phase is actually seen; see `build_snapshot`.
    yield_to_browser().await;

    let bundle = stream_bundle(repo, &refs, ids, &|written, total| {
        on_partial(writing(written, total))
    })
    .await?;

    Ok(SnapshotProps {
        name,
        format,
        state: SnapshotState::Ready {
            size: bundle.size() as usize,
            archive: bundle,
            count,
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

/// The archive's name and its top-level directory (they are the same string):
/// `<repo>-<ref>`, as cgit names its snapshots. `repo` is the repository's name,
/// resolved once when it is opened — see [`crate::repo_name`].
///
/// Both halves are flattened to a plain file name, since a ref may contain `/`
/// (`release/2.0`), which can't appear in something the browser is about to
/// save to disk.
pub(crate) fn snapshot_stem(repo: &str, ref_label: &str) -> String {
    format!("{}-{}", flatten(repo), flatten(ref_label))
}

/// What the browser will save a download of `ref_label` as.
///
/// Shared with the download links in the ref tables and on the tag page, which
/// show the file name rather than a bare "tar.gz" — so what the link says and
/// what lands in the downloads folder are the same string by construction.
pub(crate) fn snapshot_file_name(repo: &str, ref_label: &str, format: SnapshotFormat) -> String {
    format!("{}.{}", snapshot_stem(repo, ref_label), format.extension())
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
    let (walking, writing) = stage_labels(props.format);
    match &props.state {
        SnapshotState::Building { fetched, total } => {
            progress_view(&props.name, walking, *fetched, *total)
        }
        SnapshotState::Writing { written, total } => {
            progress_view(&props.name, writing, *written, *total)
        }
        SnapshotState::Ready {
            archive,
            size,
            count,
        } => html! {
            <ReadySnapshot
                name={props.name.clone()}
                format={props.format}
                archive={archive.clone()}
                size={*size}
                count={*count}
            />
        },
    }
}

/// The download being built: what stage it is at, and a bar that fills as the
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

/// Props for [`ReadySnapshot`]: the finished file.
#[derive(Properties, PartialEq, Clone)]
struct ReadyProps {
    name: String,
    format: SnapshotFormat,
    archive: Blob,
    size: usize,
    count: usize,
}

/// The finished snapshot. Like [`crate::render::blob::BlobView`] it mints the
/// object URL in an effect and passes it in, keeping the markup a plain
/// function of its inputs.
#[function_component(ReadySnapshot)]
fn ready_snapshot(props: &ReadyProps) -> Html {
    let url = use_blob_url(&props.archive);
    use_auto_download(&url, &props.name);
    ready_view(&props.name, props.format, props.size, props.count, &url)
}

/// The markup for a built download. `url` is an object URL over it, or empty if
/// one couldn't be made (under SSR, or if the browser refused), in which case
/// the link is omitted rather than emitted pointing at the page.
fn ready_view(name: &str, format: SnapshotFormat, size: usize, count: usize, url: &str) -> Html {
    let noun = match format {
        SnapshotFormat::TarGz => "file",
        SnapshotFormat::Bundle => "object",
    };
    let summary = format!(
        "{} {noun}{}, {}",
        count,
        if count == 1 { "" } else { "s" },
        format_bytes(size as u64)
    );

    html! {
        <div class="snapshot">
            <p class="snapshot-info">
                { name }{ " \u{2014} " }{ summary }
            </p>
            if url.is_empty() {
                <p class="msg error">
                    { "This browser wouldn't hand over the file to download." }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gib_archive::EntryKind;

    #[test]
    fn test_snapshot_stem() {
        assert_eq!(snapshot_stem("webgit", "v1.0.0"), "webgit-v1.0.0");
        // A ref name with a slash in it is still one file name.
        assert_eq!(snapshot_stem("webgit", "release/2.0"), "webgit-release-2.0");
    }

    /// The link text in the ref tables and on the tag page, and what the
    /// browser saves. The stem is the format's only shared part.
    #[test]
    fn test_snapshot_file_name() {
        assert_eq!(
            snapshot_file_name("webgit", "v1.0.0", SnapshotFormat::TarGz),
            "webgit-v1.0.0.tar.gz"
        );
        assert_eq!(
            snapshot_file_name("webgit", "v1.0.0", SnapshotFormat::Bundle),
            "webgit-v1.0.0.bundle"
        );
    }

    /// Render a finished snapshot's markup to a static HTML string via SSR. See
    /// the equivalent helper in `render::tag` for why we go through SSR.
    fn render(name: &str, bytes: usize, files: usize, url: &str) -> String {
        render_ready(name, SnapshotFormat::TarGz, bytes, files, url)
    }

    fn render_ready(
        name: &str,
        format: SnapshotFormat,
        bytes: usize,
        count: usize,
        url: &str,
    ) -> String {
        let (name, url) = (name.to_string(), url.to_string());
        let html = futures::executor::block_on(
            yew::ServerRenderer::<SvHost>::with_props(move || SvHostProps {
                name,
                format,
                size: bytes,
                count,
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
        format: SnapshotFormat,
        size: usize,
        count: usize,
        url: String,
    }

    #[function_component(SvHost)]
    fn sv_host(p: &SvHostProps) -> Html {
        ready_view(&p.name, p.format, p.size, p.count, &p.url)
    }

    /// The building state goes through the real component: it has no object
    /// URL to stand in for, so nothing needs hosting.
    /// The state is built by a closure rather than passed in, because
    /// `SnapshotState::Ready` holds a `Blob` and so the enum isn't `Send` — the
    /// same reason `render` above builds its props inside the closure. The
    /// in-progress states capture nothing but numbers.
    fn render_state(
        name: &str,
        format: SnapshotFormat,
        state: impl FnOnce() -> SnapshotState + Send + 'static,
    ) -> String {
        let name = name.to_string();
        let html = futures::executor::block_on(
            yew::ServerRenderer::<SnapshotView>::with_props(move || SnapshotProps {
                name,
                format,
                state: state(),
            })
            .hydratable(false)
            .render(),
        );
        html.replace("><", ">\n<")
    }

    fn render_building(fetched: usize, total: usize) -> String {
        render_state("webgit-main.tar.gz", SnapshotFormat::TarGz, move || {
            SnapshotState::Building { fetched, total }
        })
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
        insta::assert_snapshot!(render_state(
            "webgit-main.tar.gz",
            SnapshotFormat::TarGz,
            || SnapshotState::Writing {
                written: 4096,
                total: 13658,
            }
        ));
    }

    /// A finished bundle: the same view, counted in the objects that went into
    /// the pack rather than in files, since what comes out of one is a
    /// repository.
    #[test]
    fn test_bundle_html() {
        insta::assert_snapshot!(render_ready(
            "webgit-v1.0.0.bundle",
            SnapshotFormat::Bundle,
            918_244,
            13658,
            "blob:fake"
        ));
    }

    /// One object, and the noun in the summary line is singular.
    #[test]
    fn test_bundle_html_single_object() {
        insta::assert_snapshot!(render_ready(
            "webgit-v1.0.0.bundle",
            SnapshotFormat::Bundle,
            212,
            1,
            "blob:fake"
        ));
    }

    /// A bundle's phases are its own work, not the archive's: it enumerates a
    /// history where the archive fetches one tree.
    #[test]
    fn test_bundle_html_enumerating() {
        insta::assert_snapshot!(render_state(
            "webgit-v1.0.0.bundle",
            SnapshotFormat::Bundle,
            || SnapshotState::Building {
                fetched: 37,
                total: 120,
            }
        ));
    }

    /// And then writes every object it found into the pack.
    #[test]
    fn test_bundle_html_writing() {
        insta::assert_snapshot!(render_state(
            "webgit-v1.0.0.bundle",
            SnapshotFormat::Bundle,
            || SnapshotState::Writing {
                written: 4096,
                total: 13658,
            }
        ));
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
