use git_async::file_system::FileSystemError;
use std::cell::{Cell, RefCell};
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
/// that no longer exists, and a stale loose `refs/heads/<branch>` resolves HEAD
/// to a superseded commit. They are tiny, so revalidating them on every load (a
/// conditional request answered with 304 when unchanged) is cheap.
fn is_volatile_metadata(url: &str) -> bool {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    url.ends_with("/HEAD")
        || url.ends_with("/packed-refs")
        || url.ends_with("/info/refs")
        || url.ends_with("/objects/info/packs")
        // Loose ref files (refs/heads/*, refs/tags/*, …) move on every push.
        || url.contains("/refs/")
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
    WARNED_RANGE_IGNORED.set(false);
    ON_PROGRESS.with(|cb| *cb.borrow_mut() = Some(f));
}

/// Return the current `(request_count, total_bytes, cached_bytes)`.
pub(crate) fn fetch_stats() -> (u32, u64, u64) {
    STATS.with(|s| *s.borrow())
}

/// Drop the progress callback (e.g. when the stats component unmounts) so a
/// stale `use_state` setter isn't held or invoked.
pub(crate) fn clear_watch() {
    ON_PROGRESS.with(|cb| *cb.borrow_mut() = None);
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

// ---------------------------------------------------------------------------
// Response status handling
// ---------------------------------------------------------------------------

/// What [`fetch_bytes`] should do with a response, decided from its status code
/// and whether we sent a `Range` header.
///
/// Split out as a pure decision so it can be unit-tested natively: `Response`
/// only exists on `wasm32`, so anything that touches one is untestable here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseAction {
    /// Success (200 for a whole-file read, 206 for a range): the body *is* the
    /// bytes we asked for.
    Body,
    /// We asked for a range and got 200: the server ignored `Range` and sent
    /// the whole entity, so the requested window has to be cut out locally.
    SliceRange,
    /// Report a zero-length read so the caller sees EOF.
    Eof,
    /// The file does not exist.
    NotFound,
    /// Anything else. The body is an error page, not repo data — returning it
    /// would feed HTML into the zlib/pack parsers (and, worse, into the page
    /// cache) and surface as a corruption error that says nothing about the
    /// actual HTTP failure.
    HttpError,
}

fn classify(status: u16, range_requested: bool) -> ResponseAction {
    match status {
        404 => ResponseAction::NotFound,
        // 416 Range Not Satisfiable: the requested range starts at or past EOF.
        // Treat it as a zero-length read so callers see EOF instead of an error
        // (or an error page's body being copied into the destination buffer).
        416 => ResponseAction::Eof,
        // A range request answered with the full entity. Servers and proxies
        // are allowed to ignore `Range`, and some (or an intervening CDN) do.
        200 if range_requested => ResponseAction::SliceRange,
        200..=299 => ResponseAction::Body,
        // Redirects included: `fetch` follows them transparently by default, so
        // a 3xx reaching us means redirect-following was exhausted or refused,
        // and the body is not the file.
        _ => ResponseAction::HttpError,
    }
}

/// Parse a single byte range spec (`bytes=START-END`, `END` inclusive and
/// optional) into `(start, Some(end_inclusive))`.
///
/// Only the forms [`HttpFile::read_segment`](crate::fs) can generate are
/// accepted; multi-range and suffix (`bytes=-500`) specs return `None` rather
/// than being guessed at, because honouring them wrongly is exactly the silent
/// corruption this parsing exists to prevent.
fn parse_byte_range(header: &str) -> Option<(u64, Option<u64>)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        end => Some(end.parse::<u64>().ok()?),
    };
    if end.is_some_and(|end| end < start) {
        return None;
    }
    Some((start, end))
}

/// Cut the window a `Range` header asked for out of a full-entity body.
///
/// Returns `None` only for a range spec we can't honour (see
/// [`parse_byte_range`]); the caller turns that into an error instead of
/// returning bytes from the wrong offset. Overruns are clamped to the end of
/// the body, so an over-long range yields a short read and one entirely past
/// EOF yields nothing — the same outcomes a compliant server's 206/416 would
/// have produced, which is what `read_segment` already copes with.
fn slice_range<'a>(body: &'a [u8], range_header: &str) -> Option<&'a [u8]> {
    let (start, end) = parse_byte_range(range_header)?;
    let start = usize::try_from(start).ok()?;
    if start >= body.len() {
        return Some(&[]);
    }
    // `end` is inclusive, and absent means "to EOF".
    let stop = end
        .and_then(|end| usize::try_from(end).ok())
        .map_or(body.len(), |end| end.saturating_add(1).min(body.len()));
    Some(&body[start..stop])
}

thread_local! {
    /// Whether we've already warned that the server ignores `Range`. Every pack
    /// read takes the fallback path once it starts, so warning per request would
    /// bury the console (and the warning itself) under thousands of lines.
    /// Cleared per load by `reset_and_watch`, since the next load may well be a
    /// different host that does support ranges.
    static WARNED_RANGE_IGNORED: Cell<bool> = const { Cell::new(false) };
}

fn warn_range_ignored_once(url: &str) {
    if WARNED_RANGE_IGNORED.replace(true) {
        return;
    }
    web_sys::console::warn_1(
        &format!(
            "webgit: server ignored the Range header for {url} and returned the whole file; \
         reading the requested bytes locally instead. Everything still works, but every \
         read downloads the entire pack, so expect this to be slow and bandwidth-hungry."
        )
        .into(),
    );
}

async fn read_body(resp: &Response) -> Result<Vec<u8>, FileSystemError> {
    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?,
    )
    .await
    .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    Ok(js_sys::Uint8Array::new(&array_buffer).to_vec())
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
    if let Some(range) = &range {
        headers.set("Range", range).unwrap();
    }

    let resp = send(url, &headers).await?;
    let status = resp.status();
    // Every branch below counts one request: the round trip happened and cost
    // latency whether or not it yielded usable bytes. Only the byte total is
    // conditional, and it always reflects what actually came over the wire.
    let action = classify(status, range.is_some());
    match action {
        ResponseAction::NotFound => {
            fire_progress(1, 0);
            return Err(FileSystemError::NotFound(Box::new(url.to_string())));
        }
        ResponseAction::Eof => {
            fire_progress(1, 0);
            return Ok(Vec::new());
        }
        ResponseAction::HttpError => {
            fire_progress(1, 0);
            return Err(FileSystemError::Other(Box::new(format!(
                "HTTP {status} for {url}"
            ))));
        }
        ResponseAction::Body | ResponseAction::SliceRange => {}
    }

    let bytes = read_body(&resp).await?;
    fire_progress(1, bytes.len() as u64);

    if action == ResponseAction::SliceRange {
        let range = range.as_deref().unwrap_or_default();
        warn_range_ignored_once(url);
        return slice_range(&bytes, range)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| {
                FileSystemError::Other(Box::new(format!(
                    "server ignored Range '{range}' for {url} and it can't be applied locally"
                )))
            });
    }

    // Ranged responses are partial by definition, so only whole-file reads are
    // worth remembering (and `is_session_cacheable` files are never ranged).
    if is_session_cacheable(url) && range.is_none() {
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
    use super::{ResponseAction, classify, is_volatile_metadata, parse_byte_range, slice_range};

    #[test]
    fn ranged_success_returns_the_body_verbatim() {
        // 206 is the compliant answer to a Range request: the body already is
        // the requested window, so it must not be sliced a second time.
        assert_eq!(classify(206, true), ResponseAction::Body);
        assert_eq!(classify(200, false), ResponseAction::Body);
        // A 206 nobody asked for still describes a body we can use as-is.
        assert_eq!(classify(206, false), ResponseAction::Body);
    }

    #[test]
    fn range_ignored_is_sliced_locally() {
        assert_eq!(classify(200, true), ResponseAction::SliceRange);
    }

    #[test]
    fn missing_and_past_eof_are_not_errors_to_the_caller() {
        assert_eq!(classify(404, false), ResponseAction::NotFound);
        assert_eq!(classify(404, true), ResponseAction::NotFound);
        assert_eq!(classify(416, true), ResponseAction::Eof);
    }

    #[test]
    fn other_statuses_surface_as_http_errors() {
        for status in [301, 302, 400, 401, 403, 500, 502, 503] {
            assert_eq!(
                classify(status, false),
                ResponseAction::HttpError,
                "{status}"
            );
            assert_eq!(
                classify(status, true),
                ResponseAction::HttpError,
                "{status}"
            );
        }
    }

    #[test]
    fn byte_ranges_we_emit_round_trip() {
        assert_eq!(parse_byte_range("bytes=0-0"), Some((0, Some(0))));
        assert_eq!(parse_byte_range("bytes=12-4095"), Some((12, Some(4095))));
        assert_eq!(parse_byte_range("bytes=12-"), Some((12, None)));
    }

    #[test]
    fn unhonourable_range_specs_are_rejected() {
        for header in [
            "bytes=-500",    // suffix range: start is relative to EOF
            "bytes=0-9,20-", // multi-range: the reply would be multipart
            "bytes=9-0",     // end before start
            "items=0-9",     // not a byte range
            "0-9",           // no unit
            "bytes=a-9",
        ] {
            assert_eq!(parse_byte_range(header), None, "{header}");
        }
    }

    #[test]
    fn slicing_a_full_body_matches_what_a_206_would_have_returned() {
        let body: Vec<u8> = (0u8..=9).collect();
        assert_eq!(slice_range(&body, "bytes=0-3"), Some(&body[0..4]));
        assert_eq!(slice_range(&body, "bytes=4-4"), Some(&body[4..5]));
        assert_eq!(slice_range(&body, "bytes=7-9"), Some(&body[7..10]));
        assert_eq!(slice_range(&body, "bytes=3-"), Some(&body[3..]));
        // Whole-file range: identical to the body.
        assert_eq!(slice_range(&body, "bytes=0-9"), Some(&body[..]));
    }

    #[test]
    fn slicing_clamps_reads_that_run_past_eof() {
        let body: Vec<u8> = (0u8..=9).collect();
        // Short read, as a compliant server's short 206 would have given.
        assert_eq!(slice_range(&body, "bytes=8-99"), Some(&body[8..]));
        // Entirely past EOF: empty, as a 416 would have given.
        assert_eq!(slice_range(&body, "bytes=10-19"), Some(&[][..]));
        assert_eq!(slice_range(&body, "bytes=10-"), Some(&[][..]));
        assert_eq!(slice_range(&[], "bytes=0-9"), Some(&[][..]));
        // An end offset too large to index with still just means "to EOF".
        assert_eq!(
            slice_range(&body, "bytes=0-18446744073709551615"),
            Some(&body[..])
        );
    }

    #[test]
    fn slicing_refuses_ranges_it_cannot_honour() {
        let body: Vec<u8> = (0u8..=9).collect();
        assert_eq!(slice_range(&body, "bytes=-5"), None);
    }

    #[test]
    fn volatile_metadata_matches_mutable_manifests() {
        let base = "https://host/repo.git";
        for path in [
            "/HEAD",
            "/packed-refs",
            "/info/refs",
            "/objects/info/packs",
            // Loose refs move on every push, so they must revalidate too.
            "/refs/heads/main",
            "/refs/tags/v1.0",
            "/refs/heads/HEADER",
        ] {
            assert!(is_volatile_metadata(&format!("{base}{path}")), "{path}");
        }
        // Query/fragment suffixes (e.g. smart-HTTP service params) still match.
        assert!(is_volatile_metadata(&format!(
            "{base}/info/refs?service=git-upload-pack"
        )));
    }

    #[test]
    fn volatile_metadata_excludes_immutable_objects() {
        let base = "https://host/repo.git";
        for path in [
            "/objects/pack/pack-52dea9ac.pack",
            "/objects/pack/pack-52dea9ac.idx",
            "/objects/ab/cdef0123456789",
        ] {
            assert!(!is_volatile_metadata(&format!("{base}{path}")), "{path}");
        }
    }
}
