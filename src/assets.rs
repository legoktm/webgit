//! Build-time asset URLs, resolved once at startup.
//!
//! The build gives each asset a content hash, so its URL isn't known to this
//! code; it's read out of the document instead. Doing that once in [`init`],
//! rather than when a view needs it, keeps the DOM lookup off the render path —
//! views can then treat the URL as a plain value, and the SSR-based tests (which
//! have no DOM, and would panic on `web_sys::window`) simply see the default.

use std::cell::RefCell;

thread_local! {
    /// The `markdown.css` URL from the `<meta name="markdown-css">` the
    /// post-build hook writes. Empty until [`init`] runs, and if the tag is
    /// missing — in which case the readme frame renders unstyled rather than
    /// failing.
    static MARKDOWN_CSS: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Read the asset URLs out of the document. Call once, before rendering.
pub(crate) fn init() {
    let href = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector("meta[name=markdown-css]").ok().flatten())
        .and_then(|meta| meta.get_attribute("content"))
        .unwrap_or_default();
    if href.is_empty() {
        crate::console_log("webgit: no markdown-css meta; readme frame will be unstyled");
    }
    MARKDOWN_CSS.with(|css| *css.borrow_mut() = href);
}

/// The URL of the stylesheet the readme frame links.
pub(crate) fn markdown_css() -> String {
    MARKDOWN_CSS.with(|css| css.borrow().clone())
}
