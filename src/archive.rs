//! Gzipping an archive, in the browser.
//!
//! The archive itself — the walk that fetches every object under a tree, and
//! the tar written from what it finds — is [`gib_archive`]'s, which knows
//! nothing about browsers. What is left here is the half that can only happen
//! in one: reading objects through the caching repo, and compressing the tar
//! with the browser's own encoder rather than one built into the bundle.

use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::yield_to_browser;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use gib::object::{Object, ObjectId};
use gib_archive::{ArchiveEntry, ObjectSource, TarWriter};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

// The browser's own gzip encoder. Bound here rather than taken from web-sys,
// which still has it behind `--cfg=web_sys_unstable_apis` — a flag that would
// have to be set for every cargo and trunk invocation, and that would switch on
// every other unstable binding along with this one. The class is three members
// wide, and the streams either side of it are ordinary web-sys types.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = CompressionStream)]
    type CompressionStream;

    #[wasm_bindgen(constructor, catch)]
    fn new(format: &str) -> Result<CompressionStream, JsValue>;

    #[wasm_bindgen(method, getter)]
    fn readable(this: &CompressionStream) -> web_sys::ReadableStream;

    #[wasm_bindgen(method, getter)]
    fn writable(this: &CompressionStream) -> web_sys::WritableStream;
}

impl ObjectSource for CachingRepo {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move { self.lookup_object(id).await.context("read object") }.boxed_local()
    }
}

/// How much tar to accumulate before handing it to the encoder.
///
/// Small enough that no single slice is perceptible, large enough that a big
/// archive isn't thousands of promises.
const FLUSH_BYTES: usize = 1024 * 1024;

/// The archive's content type, on the blob the browser hands back.
pub(crate) const GZIP_MIME: &str = "application/gzip";

/// How long, in milliseconds of wall time, to go between letting the page
/// repaint while an archive is being written.
///
/// Awaiting the encoder is *not* enough on its own: a resolved promise is a
/// microtask, so the DOM updates but the browser never gets to paint it — see
/// [`yield_to_browser`], which is a real timer and therefore a real macrotask
/// boundary. Without one of those the progress bar is updated and never seen.
/// Same interval as the commit view's streamed diff.
const PAINT_INTERVAL_MS: f64 = 50.0;

/// Write `entries` as a tar, gzip it, and hand back the archive as a [`Blob`].
///
/// `prefix` is the directory every entry is placed under (git's `--prefix`),
/// `commit` the id recorded in the archive's global header, and `mtime` the
/// timestamp stamped on every entry — the commit's own time, so that archiving
/// the same commit twice yields the same bytes. `on_progress` is called with
/// `(entries written, total)`.
///
/// Nothing here holds the archive. The tar is fed to the browser's own gzip
/// encoder a piece at a time rather than assembled whole and compressed in one
/// call, `entries` is consumed as it is written so each blob is dropped once it
/// has gone in, and the compressed side is drained by the browser into a `Blob`
/// — which it owns, and can back with disk — instead of being reassembled into
/// a `Vec` on our side and then copied again to hand over.
///
/// That is also what keeps the page alive: compressing a large repository is
/// seconds of work, and a single synchronous call to a compressor in wasm
/// freezes the tab for all of it. The encoder being the browser's puts the
/// compression off our thread; [`PAINT_INTERVAL_MS`] does the rest.
///
/// [`Blob`]: web_sys::Blob
pub(crate) async fn stream_tar_gz(
    entries: Vec<ArchiveEntry>,
    prefix: &str,
    commit: &str,
    mtime: u64,
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<web_sys::Blob> {
    let gzip = CompressionStream::new("gzip").map_err(|e| js_error("start the gzip encoder", e))?;
    let writer = gzip
        .writable()
        .get_writer()
        .map_err(|e| js_error("open the gzip encoder", e))?;
    // A `Response` over the encoder's output side, purely to get at `.blob()`:
    // it is the one API that will drain a stream into a `Blob` for us. The
    // stream must be left alone here — taking a reader would lock it and the
    // `Response` would have nothing to read.
    let headers = web_sys::Headers::new().map_err(|e| js_error("build a response", e))?;
    headers
        .set("Content-Type", GZIP_MIME)
        .map_err(|e| js_error("build a response", e))?;
    let init = web_sys::ResponseInit::new();
    init.set_headers(&headers);
    let response =
        web_sys::Response::new_with_opt_readable_stream_and_init(Some(&gzip.readable()), &init)
            .map_err(|e| js_error("build a response", e))?;

    let total = entries.len();
    let write = async {
        let mut tar = TarWriter::new(prefix, commit, mtime)?;

        let mut last_paint = js_sys::Date::now();

        for (written, entry) in entries.into_iter().enumerate() {
            tar.append(&entry)?;
            // Explicitly, rather than at the end of the iteration: the bytes
            // are now in the tar buffer as well, and holding both copies across
            // the await below is the peak this loop is shaped to avoid.
            drop(entry);
            if tar.pending() >= FLUSH_BYTES {
                push(&writer, tar.take()).await?;
                // Report and repaint on a wall-clock budget rather than per
                // flush: a flush is only a megabyte, and `yield_to_browser` is a
                // real timer whose cost would otherwise scale with the archive.
                let now = js_sys::Date::now();
                if now - last_paint >= PAINT_INTERVAL_MS {
                    last_paint = now;
                    on_progress(written + 1, total);
                    yield_to_browser().await;
                }
            }
        }

        push(&writer, tar.take()).await?;
        push(&writer, tar.finish()?).await?;
        on_progress(total, total);
        JsFuture::from(writer.close())
            .await
            .map_err(|e| js_error("finish the gzip stream", e))?;
        Ok::<(), anyhow::Error>(())
    };

    // Collected concurrently with the writing above, not after it: the encoder's
    // output has to be drained as it is produced or its backpressure stalls the
    // very writes we are waiting on.
    let collect = async {
        let blob = JsFuture::from(
            response
                .blob()
                .map_err(|e| js_error("collect the archive", e))?,
        )
        .await
        .map_err(|e| js_error("collect the archive", e))?;
        blob.dyn_into::<web_sys::Blob>()
            .map_err(|_| anyhow::anyhow!("the archive came back as something other than a blob"))
    };

    let ((), archive) = futures::try_join!(write, collect)?;
    Ok(archive)
}

/// Hand one chunk of tar to the encoder. Awaiting the write is both the
/// backpressure and the yield that keeps the page responsive.
async fn push(writer: &web_sys::WritableStreamDefaultWriter, chunk: Vec<u8>) -> anyhow::Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    let bytes = js_sys::Uint8Array::from(chunk.as_slice());
    JsFuture::from(writer.write_with_chunk(&bytes))
        .await
        .map_err(|e| js_error("write to the gzip encoder", e))?;
    Ok(())
}

/// Describe a rejected promise or a failed constructor. `JsValue` is only
/// sometimes a string, so fall back to its debug form.
fn js_error(what: &str, e: JsValue) -> anyhow::Error {
    anyhow::anyhow!(
        "Failed to {what}: {}",
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    )
}
