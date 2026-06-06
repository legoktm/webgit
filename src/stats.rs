use crate::fetch;
use web_sys::Document;

fn format_bytes(b: u64) -> String {
    if b < 1_024 {
        format!("{} B", b)
    } else if b < 1_024 * 1_024 {
        format!("{} KB", b / 1_024)
    } else {
        format!("{:.1} MB", b as f64 / (1_024.0 * 1_024.0))
    }
}

pub(crate) fn format_stats(label: &str, reqs: u32, bytes: u64, cached_bytes: u64) -> String {
    let base = format!("{}: {} requests, {}", label, reqs, format_bytes(bytes));
    if cached_bytes > 0 {
        format!("{} (cached: {})", base, format_bytes(cached_bytes))
    } else {
        base
    }
}

pub(crate) fn set_stats_loaded(doc: &Document) {
    let (reqs, bytes, cached_bytes) = fetch::fetch_stats();
    doc.get_element_by_id("fetch-stats")
        .unwrap()
        .set_text_content(Some(&format_stats("Loaded", reqs, bytes, cached_bytes)));
}
