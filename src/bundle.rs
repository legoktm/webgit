//! Writing a git bundle, in the browser.
//!
//! The bundle itself — the walk that enumerates every object a ref reaches, and
//! the packfile written from them — is [`gib_bundle`]'s, which knows nothing
//! about browsers. What is left here is the half that can only happen in one:
//! reading objects through the caching repo, and handing the pieces to the
//! browser to hold rather than keeping a repository's worth of pack in wasm
//! memory.

use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::yield_to_browser;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use futures::stream::StreamExt;
use gib::object::{Object, ObjectId};
use gib_bundle::{BundleRef, BundleWriter};
use wasm_bindgen::JsValue;

impl gib_bundle::ObjectSource for CachingRepo {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move { self.lookup_object(id).await.context("read object") }.boxed_local()
    }
}

pub(crate) const BUNDLE_MIME: &str = "application/x-git-bundle";

/// How much bundle to accumulate before handing a piece to the browser.

const FLUSH_BYTES: usize = 1024 * 1024;

/// How many object fetches to keep in flight while writing.
const FETCH_AHEAD: usize = 48;

/// How long, in milliseconds of wall time, to go between letting the page
/// repaint while a bundle is being written.
const PAINT_INTERVAL_MS: f64 = 50.0;

/// Write `ids` — the objects [`gib_bundle::collect_objects`] settled on — into
/// a bundle carrying `refs`, and hand it back as a [`Blob`].
/// [`Blob`]: web_sys::Blob
pub(crate) async fn stream_bundle(
    repo: &CachingRepo,
    refs: &[BundleRef],
    ids: Vec<ObjectId>,
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<web_sys::Blob> {
    let total = ids.len();
    let mut writer = BundleWriter::new(refs, total)?;
    let parts = js_sys::Array::new();

    let mut objects = futures::stream::iter(
        ids.into_iter()
            .map(|id| async move { repo.lookup_object(id).await.context("read object") }),
    )
    .buffered(FETCH_AHEAD);

    let mut last_paint = js_sys::Date::now();
    let mut written = 0;
    while let Some(object) = objects.next().await {
        let object = object?;
        writer.append_object(&object)?;
        // Explicitly, rather than at the end of the iteration: the bytes are in
        // the writer's buffer as well now, and holding both copies across the
        // yield below is the peak this loop is shaped to avoid.
        drop(object);
        written += 1;
        if writer.pending() >= FLUSH_BYTES {
            push(&parts, writer.take());
        }
        // Report and repaint on a wall-clock budget rather than per object: a
        // small repository's objects land in bursts far faster than anything
        // can be seen, and `yield_to_browser` is a real timer whose cost would
        // otherwise scale with the object count.
        let now = js_sys::Date::now();
        if now - last_paint >= PAINT_INTERVAL_MS {
            last_paint = now;
            on_progress(written, total);
            yield_to_browser().await;
        }
    }

    push(&parts, writer.finish()?);
    on_progress(total, total);

    let options = web_sys::BlobPropertyBag::new();
    options.set_type(BUNDLE_MIME);
    web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .map_err(|e| js_error("collect the bundle", e))
}

/// Hand one piece of the bundle to the browser, copying it out of wasm memory
/// so the Rust side can drop it.
fn push(parts: &js_sys::Array, chunk: Vec<u8>) {
    if chunk.is_empty() {
        return;
    }
    parts.push(&js_sys::Uint8Array::from(chunk.as_slice()));
}

/// Describe a rejected promise or a failed constructor. `JsValue` is only
/// sometimes a string, so fall back to its debug form.
fn js_error(what: &str, e: JsValue) -> anyhow::Error {
    anyhow::anyhow!(
        "Failed to {what}: {}",
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    )
}
