//! The render path's handful of direct browser calls: yielding to the event
//! loop between streamed batches, and turning bytes in memory into something
//! the DOM can point at.
//!
//! Kept out of the views themselves so the SSR-based tests, which run without a
//! DOM, never reach them.

use std::rc::Rc;

/// Hand control back to the browser event loop so it can paint pending DOM
/// updates before the next chunk of work. A resolved `Promise` (microtask)
/// would not give the renderer a turn — a 0 ms `setTimeout` is a real macrotask
/// boundary, which is where the browser gets to repaint. Used between streamed
/// render batches whose data resolves too fast (cached) to yield on its own.
pub(crate) async fn yield_to_browser() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// An object URL over `data`, for whatever needs to hand bytes to the browser:
/// a blob's `<img>` and download link, a snapshot's download link.
///
/// An object URL rather than a `data:` one because the bytes are already in
/// memory: base64 would add a third again in size and park the whole encoded
/// file in a DOM attribute, where a `blob:` URL is a short string the browser
/// resolves back to a buffer. One URL per set of bytes, since constructing the
/// `Blob` copies them and a second one would hold the file twice.
///
/// The URL is created in an effect, not during render, for two reasons: it is a
/// side effect with a matching teardown (an object URL pins its buffer until
/// revoked, so navigating between blobs would otherwise leak one per visit),
/// and it keeps `web_sys` off the render path, where the SSR-based tests run
/// without a DOM. Under SSR the effect never fires and the empty string is what
/// the caller sees.
#[yew::hook]
pub(crate) fn use_object_url(mime: &'static str, data: &Rc<Vec<u8>>) -> String {
    let url = yew::use_state(String::new);
    {
        let url = url.clone();
        yew::use_effect_with(
            (mime, data.clone()),
            move |(mime, data): &(&'static str, Rc<Vec<u8>>)| {
                let created = object_url(mime, data).unwrap_or_default();
                url.set(created.clone());
                move || {
                    if !created.is_empty() {
                        let _ = web_sys::Url::revoke_object_url(&created);
                    }
                }
            },
        );
    }
    (*url).clone()
}

/// As [`use_object_url`], for a `Blob` the caller already has.
///
/// The snapshot view's archive arrives this way — the browser assembled it from
/// the gzip stream, so its bytes were never in our memory and there is nothing
/// to wrap. Blob equality is JS identity, so the effect re-runs when the archive
/// is genuinely a different one and not merely re-rendered.
#[yew::hook]
pub(crate) fn use_blob_url(blob: &web_sys::Blob) -> String {
    let url = yew::use_state(String::new);
    {
        let url = url.clone();
        yew::use_effect_with(blob.clone(), move |blob: &web_sys::Blob| {
            let created = web_sys::Url::create_object_url_with_blob(blob).unwrap_or_default();
            url.set(created.clone());
            move || {
                if !created.is_empty() {
                    let _ = web_sys::Url::revoke_object_url(&created);
                }
            }
        });
    }
    (*url).clone()
}

/// Wrap `data` in a `Blob` of type `mime` and mint an object URL for it. `None`
/// if the browser refuses either step, which leaves the view showing neither an
/// image nor a download link rather than broken ones.
fn object_url(mime: &str, data: &[u8]) -> Option<String> {
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(data));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

/// Click a detached `<a download>`, the one way to start a download that isn't
/// a navigation. Nothing is done on failure: the caller either has a visible
/// link as its fallback, or nothing useful to say.
pub(crate) fn click_download(url: &str, name: &str) {
    use wasm_bindgen::JsCast;
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

/// Save `data` as `name`, for bytes that exist only for the moment a link is
/// clicked: mint an object URL, click it, and revoke it again.
pub(crate) fn download_bytes(name: &str, mime: &str, data: &[u8]) {
    use wasm_bindgen::JsCast;
    let Some(url) = object_url(mime, data) else {
        return;
    };
    click_download(&url, name);
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    if let Some(win) = web_sys::window() {
        let _ =
            win.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 0);
    }
}
