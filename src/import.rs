//! Reading a git bundle the user picked, in the browser.
//!
//! The mirror of [`crate::bundle`]: that hands a repository to the browser as a
//! file, this takes one back. A bundle built from a clone on disk — by this
//! viewer's own download, or by `git bundle create` — carries every object of
//! the history it names, so loading one fills the object cache in a single
//! sequential read instead of one ranged request per object.
//!
//! The reading itself is [`gib_bundle`]'s, which knows nothing about browsers.
//! What is here is the half that can only happen in one: pulling the file into
//! memory, writing objects into IndexedDB in batches rather than one
//! transaction each, and letting the page repaint while a repository's worth of
//! them goes in.

use crate::cache::GlobalCache;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use gib::object::{ObjectId, RawObject};
use gib_bundle::{BundleSummary, MAX_BUNDLE_BYTES, ObjectStore, read_bundle};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// How much object content to hold before writing a batch into IndexedDB.
///
/// One transaction per object would spend most of the import opening and
/// committing them; one transaction for everything would keep a repository in
/// memory twice over. This is the middle: big enough that the per-transaction
/// cost is lost in it, small enough that what is waiting to be written is never
/// much.
const BATCH_BYTES: usize = 4 * 1024 * 1024;

/// And a ceiling on the count, for a history of very small objects — a
/// transaction's cost is per request as well as per commit.
const BATCH_OBJECTS: usize = 512;

/// How long, in milliseconds of wall time, to go between letting the page
/// repaint while objects are going in. Same reasoning as the bundle writer's.
const PAINT_INTERVAL_MS: f64 = 50.0;

/// What an import put into the cache.
pub(crate) struct Imported {
    /// How many objects the bundle carried.
    pub objects: usize,
    /// How many refs its header named.
    pub refs: usize,
}

/// Read `file` as a git bundle and put every object in it into the shared
/// object cache.
///
/// `on_progress` is called with how many objects have been stored out of how
/// many the bundle holds; it is the only sign of life during what, for a real
/// repository, is a few seconds of solid work.
pub(crate) async fn import_bundle(
    cache: &GlobalCache,
    file: &web_sys::File,
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<Imported> {
    // Bigger than any bundle this viewer will build, and past the point where
    // holding the whole file in memory to read it is reasonable at all.
    if file.size() > MAX_BUNDLE_BYTES as f64 {
        anyhow::bail!(
            "that file is {}, which is more than the {} this can read at once",
            crate::stats::format_bytes(file.size() as u64),
            crate::stats::format_bytes(MAX_BUNDLE_BYTES as u64),
        );
    }

    let buffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(|e| js_error("read the file", e))?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    // The browser's copy is no longer needed once the bytes are ours, and the
    // two together are the import's peak.
    drop(buffer);

    let store = CacheStore::new(cache);
    let summary: BundleSummary = read_bundle(&bytes, &store, on_progress).await?;
    store.flush().await?;

    Ok(Imported {
        objects: summary.objects,
        refs: summary.header.refs.len(),
    })
}

/// The object cache, as somewhere a bundle's objects can be written and read
/// back from.
struct CacheStore<'a> {
    cache: &'a GlobalCache,
    /// Objects read but not yet written, and what they weigh.
    pending: RefCell<Vec<(ObjectId, RawObject)>>,
    pending_bytes: Cell<usize>,
    /// When the page last got a turn to paint.
    last_paint: Cell<f64>,
}

impl<'a> CacheStore<'a> {
    fn new(cache: &'a GlobalCache) -> Self {
        Self {
            cache,
            pending: RefCell::new(Vec::new()),
            pending_bytes: Cell::new(0),
            last_paint: Cell::new(js_sys::Date::now()),
        }
    }

    /// Write everything held back, and wait for it to land.
    async fn flush(&self) -> anyhow::Result<()> {
        let batch = std::mem::take(&mut *self.pending.borrow_mut());
        self.pending_bytes.set(0);
        if batch.is_empty() {
            return Ok(());
        }
        self.cache
            .put_objects(&batch)
            .await
            .map_err(|e| js_error("write the objects to the cache", e))
    }
}

impl ObjectStore for CacheStore<'_> {
    fn put(&self, id: ObjectId, object: Rc<RawObject>) -> LocalBoxFuture<'_, anyhow::Result<()>> {
        // Copied out of the reader's `Rc` rather than kept as one: what is held
        // back here is held until the next batch commits, and the reader's own
        // window is holding the same objects for as long as a delta might be
        // written against them.
        let full = {
            let mut pending = self.pending.borrow_mut();
            pending.push((
                id,
                RawObject {
                    object_type: object.object_type,
                    body: object.body.clone(),
                },
            ));
            self.pending_bytes
                .set(self.pending_bytes.get() + object.body.len());
            pending.len() >= BATCH_OBJECTS || self.pending_bytes.get() >= BATCH_BYTES
        };
        drop(object);

        async move {
            if full {
                self.flush().await?;
                // Only when a batch has just gone in: the point of this is the
                // progress bar, and between two batches there is nothing new
                // to draw.
                let now = js_sys::Date::now();
                if now - self.last_paint.get() >= PAINT_INTERVAL_MS {
                    self.last_paint.set(now);
                    crate::render::yield_to_browser().await;
                }
            }
            Ok(())
        }
        .boxed_local()
    }

    fn get(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Option<RawObject>>> {
        // A base can be in either place: still waiting to be written, or
        // already in the cache — including from long before this import, which
        // is how a differential bundle's deltas against objects it doesn't
        // carry are rebuilt at all.
        let held = self
            .pending
            .borrow()
            .iter()
            .find(|(pending, _)| *pending == id)
            .map(|(_, raw)| RawObject {
                object_type: raw.object_type,
                body: raw.body.clone(),
            });
        async move {
            if let Some(raw) = held {
                return Ok(Some(raw));
            }
            Ok(self.cache.object(id).await)
        }
        .boxed_local()
    }
}

/// Describe a rejected promise or a failed call. `JsValue` is only sometimes a
/// string, so fall back to its debug form.
fn js_error(what: &str, e: JsValue) -> anyhow::Error {
    anyhow::anyhow!(
        "Failed to {what}: {}",
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    )
}
