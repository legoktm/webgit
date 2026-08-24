//! Diffing a commit's files as their blobs arrive, so the view paints from
//! the top rather than after the last round-trip lands.

use super::FileRow;
use crate::cache::CachingRepo;
use crate::render::yield_to_browser;
use futures::stream::{FuturesOrdered, StreamExt};
use gib::diff::{DiffEntry, TreeDiff};
use gib::object::{Object, ObjectId};
use gib_patch::{DiffOptions, Side};

/// The bytes of one side of a change, or nothing when the file did not exist
/// on that side.
async fn load_side(repo: &CachingRepo, side: Option<Side>) -> Vec<u8> {
    let Some(side) = side else {
        return Vec::new();
    };
    match repo.lookup_object(side.id).await {
        Ok(Object::Blob(b)) => b.data_owned(),
        Ok(_) => format!("{}", side.id).into_bytes(),
        Err(_) => Vec::new(),
    }
}

/// The two sides of a diff entry, absent where the file did not exist.
fn sides(entry: &DiffEntry<(ObjectId, ObjectId)>) -> (Option<Side>, Option<Side>) {
    match entry {
        DiffEntry::LeftOnly {
            entry_type,
            content: (old, _),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *entry_type,
            }),
            None,
        ),
        DiffEntry::RightOnly {
            entry_type,
            content: (_, new),
            ..
        } => (
            None,
            Some(Side {
                id: *new,
                entry_type: *entry_type,
            }),
        ),
        DiffEntry::Both {
            left_type,
            right_type,
            content: (old, new),
            ..
        } => (
            Some(Side {
                id: *old,
                entry_type: *left_type,
            }),
            Some(Side {
                id: *new,
                entry_type: *right_type,
            }),
        ),
    }
}

/// Rescale the diffstat bars to the current widest file (0–40 columns). Called
/// before each progress emit, so bars re-normalise as larger files arrive; the
/// final return leaves them at their finished widths.
pub(super) fn recompute_bars(files: &mut [FileRow]) {
    let max_changes = files
        .iter()
        .map(|f| f.additions() + f.deletions())
        .max()
        .unwrap_or(1)
        .max(1);

    for f in files {
        let total = f.additions() + f.deletions();
        let bar_total = total * 40 / max_changes;
        f.bar_add = f
            .additions()
            .checked_mul(bar_total)
            .and_then(|n| n.checked_div(total))
            .unwrap_or(0);
        f.bar_del = bar_total - f.bar_add;
    }
}

/// How often, in milliseconds of wall time, to emit a partial diff while
/// streaming. Cached blobs resolve in a back-to-back microtask burst that would
/// otherwise never yield the renderer a turn; emitting (and yielding) on a time
/// budget paints progressively without re-rendering the whole diff once per
/// file. A small/fast diff never trips it and just renders once at the end.
const DIFF_EMIT_INTERVAL_MS: f64 = 50.0;

/// Diff every changed file, calling `on_progress` as it goes. The diffstat's
/// file list is emitted immediately from the tree diff (paths known up front,
/// stats `pending`); then each file's blobs are loaded — kicked off all at once
/// so the round-trips overlap, but consumed in tree order via [`FuturesOrdered`]
/// so the diff body fills in top-to-bottom — and its counts/lines folded in,
/// re-emitting roughly every [`DIFF_EMIT_INTERVAL_MS`]. Diffing itself is
/// CPU-bound and stays sequential.
pub(super) async fn stream_diff(
    repo: &CachingRepo,
    td: &TreeDiff,
    options: DiffOptions,
    on_progress: impl Fn(&[FileRow]),
) -> Vec<FileRow> {
    // The changed-file list is known from the tree diff before any blob loads,
    // so show it right away with the stats column blank.
    let mut files: Vec<FileRow> = td
        .entries()
        .iter()
        .map(|entry| FileRow {
            path: String::from_utf8_lossy(entry.path().as_slice()).into_owned(),
            diff: None,
            patch_diff: None,
            bar_add: 0,
            bar_del: 0,
        })
        .collect();
    on_progress(&files);
    yield_to_browser().await;

    let mut loads: FuturesOrdered<_> = td
        .entries()
        .iter()
        .map(|entry| async move {
            let (old, new) = sides(entry);
            let (old_data, new_data) = futures::join!(load_side(repo, old), load_side(repo, new));
            (old, new, old_data, new_data)
        })
        .collect();

    // `FuturesOrdered` yields in tree order, matching `files`.
    let mut idx = 0;
    let mut last_emit = js_sys::Date::now();
    while let Some((old, new, old_data, new_data)) = loads.next().await {
        files[idx].diff = Some(gib_patch::diff_file(
            &files[idx].path,
            old,
            new,
            &old_data,
            &new_data,
            options,
        ));
        // Only the reader who has changed the controls pays for the second
        // diff, and only they need it: at the defaults the view's own diff is
        // already the patch.
        if options != DiffOptions::default() {
            files[idx].patch_diff = Some(gib_patch::diff_file(
                &files[idx].path,
                old,
                new,
                &old_data,
                &new_data,
                DiffOptions::default(),
            ));
        }
        idx += 1;
        // Paint progressively, but only every ~50ms so a large diff isn't
        // re-rendered (and the growing line list re-cloned) once per file. Yield
        // to the event loop after emitting so the browser actually repaints —
        // cached blobs drain as microtasks that wouldn't otherwise let it.
        if js_sys::Date::now() - last_emit >= DIFF_EMIT_INTERVAL_MS {
            recompute_bars(&mut files);
            on_progress(&files);
            yield_to_browser().await;
            last_emit = js_sys::Date::now();
        }
    }

    recompute_bars(&mut files);
    files
}
