use web_sys::HtmlIFrameElement;
use yew::prelude::*;

/// Where a rendered document's relative links resolve from.
///
/// A markdown file is read relative to itself: `[x](y.md)` in `docs/a.md` means
/// `docs/y.md`, and `#section` means a place in the document the reader is
/// already looking at. Neither is knowable from the markdown, so the caller —
/// the readme route, or the blob view — says where the document lives.
#[derive(Clone, PartialEq)]
pub(crate) struct LinkBase {
    /// The document's containing directory, repository-relative; empty at the
    /// root.
    pub dir: String,
    /// Where a bare `#fragment` points — the document's own URL.
    pub self_url: String,
}

/// Render markdown to HTML with the GFM extensions a document is likely to use.
///
/// `render.unsafe` is left off, so raw HTML in the document is dropped rather
/// than emitted, and comrak filters script-bearing URLs (`javascript:`,
/// `data:`, `vbscript:`) out of links and images for us. Documents do use raw
/// HTML — `<details>` blocks, centred headers — and this loses it; the trade is
/// that the frame's sandbox stays a second line of defence rather than the only
/// thing standing between a repository's markup and the app. See
/// [`markdown_frame`] for what that sandbox does and doesn't allow.
///
/// Link destinations are rewritten on the way out so repository-relative paths
/// address the app — see [`rewrite_url`]. Where each link *opens* is settled by
/// the frame document's `<base target="_top">`, since comrak has no per-link
/// target.
pub(crate) fn markdown_to_html(markdown: &str, base: &LinkBase) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    // GFM's autolink extension: bare URLs and emails in the text become links,
    // trailing sentence punctuation excluded. CommonMark only links the
    // `<https://…>` form.
    options.extension.autolink = true;
    // The rewriter is a closure rather than a plain function because it has to
    // carry the document's position with it; comrak's blanket `URLRewriter`
    // impl covers any `Fn(&str) -> String` that is `Send + Sync`.
    let base = base.clone();
    options.extension.link_url_rewriter = Some(std::sync::Arc::new(move |url: &str| {
        rewrite_url(url, &base)
    }));
    comrak::markdown_to_html(markdown, &options)
}

/// Rewrite a link's destination for display inside the frame.
///
/// Absolute URLs are left alone. A repository-relative path becomes the in-app
/// tree URL for that file, which works because the frame's document resolves
/// relative URLs against this page. A bare fragment is pointed back at the
/// document itself — where the reader already is — so that following it does
/// nothing instead of navigating the app somewhere unrelated.
///
/// A fragment can't do better than that: the frame is exactly as tall as its
/// content, so it has nothing to scroll, and scrolling *this* page to a position
/// inside the frame would take a script the sandbox forbids.
///
/// Script-bearing schemes need no handling here — comrak checks those against
/// the *original* URL before this runs, and drops the href itself.
fn rewrite_url(url: &str, base: &LinkBase) -> String {
    if url.starts_with('#') {
        base.self_url.clone()
    } else if is_absolute(url) {
        url.to_string()
    } else {
        link_url(&base.dir, url)
    }
}

/// Whether a link points outside this repository: it carries a scheme
/// (`https:`) or is protocol-relative (`//host/x`).
///
/// `web_sys::Url` would be the browser's own parser, but it panics off-wasm, so
/// every test that renders markdown — including the frame snapshot — would have
/// to move to a browser runner. A `:` that follows a `/`, `?` or `#` belongs to
/// the path rather than a scheme (`docs/a:b.md`), which is the only subtlety
/// here.
fn is_absolute(url: &str) -> bool {
    if url.starts_with("//") {
        return true;
    }
    let Some(end) = url.find([':', '/', '?', '#']) else {
        return false;
    };
    if url.as_bytes()[end] != b':' || end == 0 {
        return false;
    }
    let mut scheme = url[..end].chars();
    scheme.next().is_some_and(|c| c.is_ascii_alphabetic())
        && scheme.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The in-app tree URL for a link destination written inside a document in
/// `dir`, with `.`/`..` segments resolved and any query or fragment dropped
/// (the tree view takes neither). A `..` that would climb past the root is
/// discarded.
///
/// A leading `/` means the repository root rather than the document's
/// directory, which is how a README written for a forge reads it.
///
/// The ref the document is being viewed at is deliberately not carried into the
/// link: a rewritten destination addresses a path, and the tree route resolves
/// a path without a `?h=` against HEAD.
fn link_url(dir: &str, dest: &str) -> String {
    let path = dest.split(['?', '#']).next().unwrap_or("");
    let joined = if dir.is_empty() || path.starts_with('/') {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    // `tree_url` encodes each segment: a document can link to a file whose name
    // contains '?' or '#', which would otherwise cut the route short.
    crate::route::tree_url(&segments.join("/"), None, false)
}

/// The height (CSS px) the frame is given before its content has been measured.
const INITIAL_FRAME_HEIGHT: i32 = 150;

#[derive(Properties, PartialEq, Clone)]
pub(crate) struct MarkdownFrameProps {
    pub html: String,
    /// The frame's accessible name: "README", or the file's name.
    pub title: AttrValue,
}

/// Rendered markdown, shown in a sandboxed `srcdoc` frame.
///
/// The sandbox omits `allow-scripts`, so nothing in the document can execute.
/// Nothing should reach it that would want to — [`markdown_to_html`] drops raw
/// HTML — but the frame is where that stops being a matter of trusting the
/// renderer. The rest is the minimum the content needs: `allow-same-origin` so
/// this document can read the frame's height back out of it, and
/// `allow-top-navigation-by-user-activation` so a link can take the page with it
/// — only on a real click, never on its own.
///
/// Do not add `allow-scripts`. Paired with the `allow-same-origin` already here,
/// it would give anything that did slip through the renderer script execution in
/// this app's own origin. If the frame ever needs to size itself, find another
/// way.
///
/// The frame's document inherits this page's CSP, so it can neither carry an
/// inline `<style>` nor load anything off-origin; it links `markdown.css`
/// instead and is styled by the `.markdown-body` rules there.
#[function_component(MarkdownFrame)]
pub(crate) fn markdown_frame(props: &MarkdownFrameProps) -> Html {
    let frame_ref = use_node_ref();
    // Grown to fit the content once the frame reports its own height; reset
    // whenever the document changes so a shorter document can shrink again.
    let height = use_state(|| INITIAL_FRAME_HEIGHT);
    {
        let height = height.clone();
        use_effect_with(props.html.clone(), move |_| {
            height.set(INITIAL_FRAME_HEIGHT);
            || ()
        });
    }

    // A sandboxed frame can't size itself (that would need a script inside), so
    // measure its content here once it has loaded and set the frame's `height`
    // attribute. The attribute, not a `style`, because the CSP forbids inline
    // styles — and going through Yew keeps it declarative.
    let onload = {
        let frame_ref = frame_ref.clone();
        let height = height.clone();
        Callback::from(move |_: Event| {
            let Some(frame) = frame_ref.cast::<HtmlIFrameElement>() else {
                return;
            };
            // The body, not `documentElement`: it's the element `markdown.css`
            // sizes to the content, so its scroll height is the content's.
            if let Some(body) = frame.content_document().and_then(|d| d.body()) {
                height.set(body.scroll_height().max(INITIAL_FRAME_HEIGHT));
            }
        })
    };

    html! {
        <iframe
            ref={frame_ref}
            class="markdown-frame"
            title={props.title.clone()}
            height={height.to_string()}
            sandbox="allow-same-origin allow-top-navigation-by-user-activation"
            srcdoc={frame_document(&props.html, &crate::assets::markdown_css())}
            {onload}
        />
    }
}

/// The complete document put in the frame's `srcdoc`. Relative URLs in it
/// resolve against this page, which is what lets a rewritten `#!/tree/...` link
/// address the app.
///
/// `<base target="_top">` makes every link navigate the page rather than the
/// frame — a frame that can only ever be as tall as its content is no place to
/// land. It applies to all links because comrak has no per-link target, so an
/// external link leaves the app the way any ordinary link would. (`base-uri`
/// doesn't apply here: no base *URL* is being set.)
fn frame_document(body_html: &str, stylesheet_href: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <base target=\"_top\">\
         <link rel=\"stylesheet\" href=\"{stylesheet_href}\">\
         </head><body class=\"markdown-body\">{body_html}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document at the repository root, as the readme route renders one.
    fn root() -> LinkBase {
        LinkBase {
            dir: String::new(),
            self_url: "#!/readme".to_string(),
        }
    }

    /// A document one directory down, as the blob view renders one.
    fn in_docs() -> LinkBase {
        LinkBase {
            dir: "docs".to_string(),
            self_url: "#!/tree/docs/setup.md?render=1".to_string(),
        }
    }

    #[test]
    fn markdown_renders_gfm_extensions() {
        // Tables, strikethrough and task lists are all opted into explicitly.
        let html = markdown_to_html(
            "| a |\n|---|\n| 1 |\n\n~~gone~~\n\n- [x] done\n- [ ] todo\n",
            &root(),
        );
        assert!(html.contains("<table>"), "tables: {html}");
        assert!(html.contains("<del>"), "strikethrough: {html}");
        assert!(html.contains("type=\"checkbox\""), "task lists: {html}");
    }

    #[test]
    fn markdown_drops_raw_html() {
        // `render.unsafe` is off, so a document's own markup never reaches the
        // frame. The sandbox is a backstop for this, not the reason it's safe.
        let html = markdown_to_html("<p align=\"center\"><b>hi</b></p>\n", &root());
        assert!(!html.contains("<p align"), "{html}");
        let script = markdown_to_html("<script>alert(1)</script>\n", &root());
        assert!(!script.contains("<script"), "{script}");
    }

    #[test]
    fn relative_links_route_to_the_tree_view() {
        // A repo-relative link becomes an in-app tree URL. Where it opens is
        // the frame document's business (`<base target="_top">`), not the URL's.
        let html = markdown_to_html("[setup](docs/setup.md)\n", &root());
        assert!(html.contains(r##"href="#!/tree/docs/setup.md""##), "{html}");
    }

    /// A link in a document below the root is read relative to that document,
    /// the way it would be on disk.
    #[test]
    fn relative_links_resolve_against_the_documents_directory() {
        let html = markdown_to_html("[next](install.md)\n", &in_docs());
        assert!(
            html.contains(r##"href="#!/tree/docs/install.md""##),
            "{html}"
        );
        let up = markdown_to_html("[src](../src/lib.rs)\n", &in_docs());
        assert!(up.contains(r##"href="#!/tree/src/lib.rs""##), "{up}");
        // A leading slash means the repository root, not the document's dir.
        let abs = markdown_to_html("[root](/README.md)\n", &in_docs());
        assert!(abs.contains(r##"href="#!/tree/README.md""##), "{abs}");
    }

    #[test]
    fn absolute_links_are_left_alone() {
        let html = markdown_to_html("[home](https://example.org/x?a=1&b=2)\n", &root());
        assert!(
            html.contains(r#"href="https://example.org/x?a=1&amp;b=2""#),
            "{html}"
        );
    }

    #[test]
    fn fragment_links_go_nowhere() {
        // A fragment can't scroll a content-height frame, so it points at the
        // document the reader is already on rather than navigating the app away.
        let html = markdown_to_html("[top](#intro)\n", &root());
        assert!(html.contains(r##"href="#!/readme""##), "{html}");
        let blob = markdown_to_html("[top](#intro)\n", &in_docs());
        assert!(
            blob.contains(r##"href="#!/tree/docs/setup.md?render=1""##),
            "{blob}"
        );
    }

    #[test]
    fn script_bearing_links_are_dropped_by_comrak() {
        // Checked against the original URL, before `rewrite_url` sees it — so
        // this holds for images too, which have no rewriter of their own.
        let link = markdown_to_html("[x](javascript:alert(1))\n", &root());
        assert!(!link.contains("javascript:"), "{link}");
        let image = markdown_to_html("![x](javascript:alert(1))\n", &root());
        assert!(!image.contains("javascript:"), "{image}");
    }

    #[test]
    fn bare_urls_are_linkified() {
        // GFM's autolink extension, which plain CommonMark does not do. The
        // trailing full stop is punctuation, not part of the URL.
        let html = markdown_to_html("See https://example.org/docs.\n", &root());
        assert!(
            html.contains(r#"<a href="https://example.org/docs">https://example.org/docs</a>."#),
            "{html}"
        );
    }

    #[test]
    fn bare_www_urls_are_linkified() {
        let html = markdown_to_html("www.example.org\n", &root());
        assert!(html.contains(">www.example.org</a>"), "{html}");
    }

    #[test]
    fn autolinking_skips_code_and_existing_links() {
        // Inline code, a fenced block, and the text of an explicit link are all
        // left alone — the last would otherwise nest one <a> inside another.
        let html = markdown_to_html(
            "`https://example.org`\n\n```\nhttps://example.org\n```\n\n[https://example.org](https://other.example)\n",
            &root(),
        );
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(html.contains(r#"href="https://other.example""#), "{html}");
    }

    #[test]
    fn test_link_url_normalizes_paths() {
        assert_eq!(link_url("", "docs/setup.md"), "#!/tree/docs/setup.md");
        assert_eq!(link_url("", "./docs/setup.md"), "#!/tree/docs/setup.md");
        // A leading slash still means the repository root, not the web root.
        assert_eq!(link_url("", "/docs/setup.md"), "#!/tree/docs/setup.md");
        assert_eq!(link_url("", "docs/../src/lib.rs"), "#!/tree/src/lib.rs");
        // Climbing past the root is clamped rather than escaping the repo.
        assert_eq!(link_url("", "../../etc/passwd"), "#!/tree/etc/passwd");
        assert_eq!(
            link_url("docs", "../../../etc/passwd"),
            "#!/tree/etc/passwd"
        );
        // The tree view takes neither a query nor a fragment.
        assert_eq!(
            link_url("", "docs/setup.md#install"),
            "#!/tree/docs/setup.md"
        );
        assert_eq!(link_url("", ""), "#!/tree");
        // A link to the directory the document is in.
        assert_eq!(link_url("docs/guide", "."), "#!/tree/docs/guide");
        // Characters that would otherwise read as route syntax are escaped,
        // not left to cut the URL short. (A literal '?' or '#' can't get this
        // far: they end the destination's path, above.)
        assert_eq!(link_url("docs", "a b.md"), "#!/tree/docs/a%20b.md");
        assert_eq!(link_url("", "50%off.md"), "#!/tree/50%25off.md");
    }

    #[test]
    fn test_is_absolute() {
        assert!(is_absolute("https://example.org"));
        assert!(is_absolute("MAILTO:a@b.c"));
        // A protocol-relative URL is absolute, not a repository path.
        assert!(is_absolute("//cdn.example.org/x"));
        // A colon inside a path is not a scheme delimiter.
        assert!(!is_absolute("docs/a:b.md"));
        assert!(!is_absolute("./x.md"));
        assert!(!is_absolute(""));
    }

    #[test]
    fn test_rewrite_url() {
        assert_eq!(rewrite_url("mailto:a@b.c", &root()), "mailto:a@b.c");
        assert_eq!(
            rewrite_url("docs/setup.md", &root()),
            "#!/tree/docs/setup.md"
        );
        assert_eq!(rewrite_url("#intro", &root()), "#!/readme");
        assert_eq!(
            rewrite_url("#intro", &in_docs()),
            "#!/tree/docs/setup.md?render=1"
        );
    }

    #[test]
    fn frame_document_embeds_body_and_stylesheet() {
        let doc = frame_document("<h1>hi</h1>", "/assets/styles-abc123.css");
        assert!(doc.contains("<link rel=\"stylesheet\" href=\"/assets/styles-abc123.css\">"));
        assert!(doc.contains("<body class=\"markdown-body\"><h1>hi</h1></body>"));
    }
}
