use git_async::file_system::FileSystemError;
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestCache, RequestInit, RequestMode, Response};

// ---------------------------------------------------------------------------
// Per-load stats + progress hook
// ---------------------------------------------------------------------------

type ProgressFn = Box<dyn Fn(u32, u64, u64)>;

// ---------------------------------------------------------------------------
// Session-scoped URL cache (avoids re-fetching stable files like packed-refs)
// ---------------------------------------------------------------------------

thread_local! {
    static SESSION_CACHE: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
}

fn is_session_cacheable(url: &str) -> bool {
    url.ends_with("/packed-refs")
}

/// Mutable dumb-HTTP metadata: small files that are rewritten as the repo
/// changes (refs move, `update-server-info` regenerates the pack/ref manifests).
/// Unlike content-addressed objects and packs, these must not be served stale
/// from the browser's HTTP cache — a stale `objects/info/packs` names a pack
/// that no longer exists, which fails the whole repo open. They are tiny, so
/// revalidating them on every load (a conditional request answered with 304
/// when unchanged) is cheap.
fn is_volatile_metadata(url: &str) -> bool {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    url.ends_with("/HEAD")
        || url.ends_with("/packed-refs")
        || url.ends_with("/info/refs")
        || url.ends_with("/objects/info/packs")
}

thread_local! {
    /// (request_count, total_bytes, cached_bytes)
    static STATS: RefCell<(u32, u64, u64)> = const { RefCell::new((0, 0, 0)) };
    static ON_PROGRESS: RefCell<Option<ProgressFn>> = const { RefCell::new(None) };
}

/// Reset the counters to zero and register a callback that is invoked after
/// every fetch or cache hit with `(request_count, total_bytes, cached_bytes)`.
pub(crate) fn reset_and_watch(f: Box<dyn Fn(u32, u64, u64)>) {
    SESSION_CACHE.with(|c| c.borrow_mut().clear());
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

async fn send(url: &str, headers: &Headers) -> Result<Response, FileSystemError> {
    let window = web_sys::window()
        .ok_or_else(|| FileSystemError::Other(Box::new("no window".to_string())))?;
    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);
    opts.set_headers_headers(headers);
    // Force revalidation of the mutable manifests so a repacked/renamed repo
    // isn't read through a heuristically-cached (no Cache-Control) stale copy.
    if is_volatile_metadata(url) {
        opts.set_cache(RequestCache::NoCache);
    }
    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    resp_value
        .dyn_into::<Response>()
        .map_err(|_| FileSystemError::Other(Box::new("not a Response".to_string())))
}

pub(crate) async fn fetch_bytes(
    url: &str,
    range: Option<String>,
) -> Result<Vec<u8>, FileSystemError> {
    if is_session_cacheable(url) && range.is_none() {
        let hit = SESSION_CACHE.with(|c| c.borrow().get(url).cloned());
        if let Some(bytes) = hit {
            record_cache_hit(bytes.len() as u64);
            return Ok(bytes);
        }
    }

    let headers = Headers::new().unwrap();
    if let Some(range) = range {
        headers.set("Range", &range).unwrap();
    }

    let resp = send(url, &headers).await?;
    if resp.status() == 404 {
        fire_progress(1, 0);
        return Err(FileSystemError::NotFound(Box::new(url.to_string())));
    }
    // 416 Range Not Satisfiable: the requested range starts at or past EOF.
    // Treat it as a zero-length read so callers see EOF instead of an error
    // (or an error page's body being copied into the destination buffer).
    if resp.status() == 416 {
        fire_progress(1, 0);
        return Ok(Vec::new());
    }
    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?,
    )
    .await
    .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
    fire_progress(1, bytes.len() as u64);
    if is_session_cacheable(url) {
        SESSION_CACHE.with(|c| c.borrow_mut().insert(url.to_string(), bytes.clone()));
    }
    Ok(bytes)
}

pub(crate) async fn fetch_text(url: &str) -> Result<String, FileSystemError> {
    let bytes = fetch_bytes(url, None).await?;
    String::from_utf8(bytes).map_err(|e| FileSystemError::Other(Box::new(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::is_volatile_metadata;

    #[test]
    fn volatile_metadata_matches_mutable_manifests() {
        let base = "https://host/repo.git";
        for path in [
            "/HEAD",
            "/packed-refs",
            "/info/refs",
            "/objects/info/packs",
        ] {
            assert!(is_volatile_metadata(&format!("{base}{path}")), "{path}");
        }
        // Query/fragment suffixes (e.g. smart-HTTP service params) still match.
        assert!(is_volatile_metadata(&format!("{base}/info/refs?service=git-upload-pack")));
    }

    #[test]
    fn volatile_metadata_excludes_immutable_objects() {
        let base = "https://host/repo.git";
        for path in [
            "/objects/pack/pack-52dea9ac.pack",
            "/objects/pack/pack-52dea9ac.idx",
            "/objects/ab/cdef0123456789",
            // A ref literally named HEAD-ish but not the HEAD file.
            "/refs/heads/HEADER",
        ] {
            assert!(!is_volatile_metadata(&format!("{base}{path}")), "{path}");
        }
    }
}
