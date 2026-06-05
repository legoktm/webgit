use git_async::file_system::FileSystemError;
use std::cell::RefCell;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};

// ---------------------------------------------------------------------------
// Per-load stats + progress hook
// ---------------------------------------------------------------------------

type ProgressFn = Box<dyn Fn(u32, u64, u64)>;

thread_local! {
    /// (request_count, total_bytes, cached_bytes)
    static STATS: RefCell<(u32, u64, u64)> = const { RefCell::new((0, 0, 0)) };
    static ON_PROGRESS: RefCell<Option<ProgressFn>> = const { RefCell::new(None) };
}

/// Reset the counters to zero and register a callback that is invoked after
/// every fetch or cache hit with `(request_count, total_bytes, cached_bytes)`.
pub(crate) fn reset_and_watch(f: Box<dyn Fn(u32, u64, u64)>) {
    STATS.with(|s| *s.borrow_mut() = (0, 0, 0));
    ON_PROGRESS.with(|cb| *cb.borrow_mut() = Some(f));
}

/// Return the current `(request_count, total_bytes, cached_bytes)`.
pub(crate) fn fetch_stats() -> (u32, u64, u64) {
    STATS.with(|s| *s.borrow())
}

/// Record a cache hit and fire the progress callback.
pub(crate) fn record_cache_hit(bytes: u64) {
    fire(0, 0, bytes);
}

fn fire_progress(req_delta: u32, byte_delta: u64) {
    fire(req_delta, byte_delta, 0);
}

fn fire(req_delta: u32, byte_delta: u64, cache_delta: u64) {
    let snapshot = STATS.with(|s| {
        let mut st = s.borrow_mut();
        st.0 += req_delta;
        st.1 += byte_delta;
        st.2 += cache_delta;
        *st
    });
    ON_PROGRESS.with(|cb| {
        if let Some(f) = cb.borrow().as_ref() {
            f(snapshot.0, snapshot.1, snapshot.2);
        }
    });
}

async fn send(url: &str) -> Result<Response, FileSystemError> {
    let window = web_sys::window()
        .ok_or_else(|| FileSystemError::Other(Box::new("no window".to_string())))?;
    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);
    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    resp_value
        .dyn_into::<Response>()
        .map_err(|_| FileSystemError::Other(Box::new("not a Response".to_string())))
}

pub(crate) async fn fetch_bytes(url: &str) -> Result<Vec<u8>, FileSystemError> {
    let resp = send(url).await?;
    if resp.status() == 404 {
        fire_progress(1, 0);
        return Err(FileSystemError::NotFound(Box::new(url.to_string())));
    }
    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?,
    )
    .await
    .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
    fire_progress(1, bytes.len() as u64);
    Ok(bytes)
}

pub(crate) async fn fetch_text(url: &str) -> Result<String, FileSystemError> {
    let bytes = fetch_bytes(url).await?;
    String::from_utf8(bytes).map_err(|e| FileSystemError::Other(Box::new(e.to_string())))
}
