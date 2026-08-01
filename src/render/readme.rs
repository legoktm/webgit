use crate::cache::CachingRepo;
use git_async::object::{Tree, TreeEntryType};
use web_sys::HtmlIFrameElement;
use yew::prelude::*;

/// The README file names looked for in the root of the HEAD tree, in the order
/// they're preferred; first one wins. Only `.md` is treated as markdown — the
/// extension-less and `.txt` spellings are shown verbatim.
const README_NAMES: [&str; 4] = ["README.md", "README", "README.txt", "README.rst"];

/// A README's content, in the form it should be displayed.
#[derive(PartialEq, Clone)]
pub(crate) enum ReadmeContent {
    /// Markdown already rendered to HTML, to be shown in the sandboxed frame.
    Markdown(String),
    /// A non-markdown README, shown as-is.
    Plain(String),
}

/// The view inputs for the readme page: the README's content, or `None` when
/// the repository's HEAD tree has none of [`README_NAMES`] at its root.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct ReadmeProps {
    pub content: Option<ReadmeContent>,
}

/// Find the first of [`README_NAMES`] at the root of `root_tree` and read it as
/// text. Entries that aren't a regular file (a directory named `README`, a
/// symlink, a submodule) are skipped rather than treated as a match.
pub(crate) async fn build_readme(root_tree: &Tree, repo: &CachingRepo) -> ReadmeProps {
    for name in README_NAMES {
        let Some(entry) = root_tree.entries().find(|e| e.name() == name.as_bytes()) else {
            continue;
        };
        if !matches!(
            entry.entry_type(),
            TreeEntryType::File | TreeEntryType::Executable
        ) {
            continue;
        }
        let Some(blob) = repo
            .lookup_object(entry.id())
            .await
            .ok()
            .and_then(|o| o.blob().ok())
        else {
            continue;
        };
        let text = String::from_utf8_lossy(blob.data()).into_owned();
        return ReadmeProps {
            content: Some(if name.ends_with(".md") {
                ReadmeContent::Markdown(markdown_to_html(&text))
            } else {
                ReadmeContent::Plain(text)
            }),
        };
    }
    ReadmeProps { content: None }
}

/// Render markdown to HTML with the GFM extensions a README is likely to use.
///
/// `render.unsafe` is left off, so raw HTML in the README is dropped rather than
/// emitted, and comrak filters script-bearing URLs (`javascript:`, `data:`,
/// `vbscript:`) out of links and images for us. READMEs do use raw HTML —
/// `<details>` blocks, centred headers — and this loses it; the trade is that
/// the frame's sandbox stays a second line of defence rather than the only
/// thing standing between a repository's markup and the app. See
/// [`markdown_frame`] for what that sandbox does and doesn't allow.
///
/// Link destinations are rewritten on the way out so repository-relative paths
/// address the app — see [`rewrite_url`]. Where each link *opens* is settled by
/// the frame document's `<base target="_top">`, since comrak has no per-link
/// target.
fn markdown_to_html(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    // GFM's autolink extension: bare URLs and emails in the text become links,
    // trailing sentence punctuation excluded. CommonMark only links the
    // `<https://…>` form.
    options.extension.autolink = true;
    options.extension.link_url_rewriter = Some(std::sync::Arc::new(rewrite_url));
    comrak::markdown_to_html(markdown, &options)
}

/// Rewrite a link's destination for display inside the frame.
///
/// Absolute URLs are left alone. A repository-relative path becomes the in-app
/// tree URL for that file, which works because the frame's document resolves
/// relative URLs against this page. A bare fragment is pointed back at the
/// readme route — where the reader already is — so that following it does
/// nothing instead of navigating the app somewhere unrelated.
///
/// A fragment can't do better than that: the frame is exactly as tall as its
/// content, so it has nothing to scroll, and scrolling *this* page to a position
/// inside the frame would take a script the sandbox forbids.
///
/// Script-bearing schemes need no handling here — comrak checks those against
/// the *original* URL before this runs, and drops the href itself.
fn rewrite_url(url: &str) -> String {
    if url.starts_with('#') {
        "#!/readme".to_string()
    } else if is_absolute(url) {
        url.to_string()
    } else {
        tree_url(url)
    }
}

/// Whether a link points outside this repository: it carries a scheme
/// (`https:`) or is protocol-relative (`//host/x`).
///
/// `web_sys::Url` would be the browser's own parser, but it panics off-wasm, so
/// every test that renders a README — including the frame snapshot — would have
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

/// The in-app tree URL for a repository-relative path, with `.`/`..` segments
/// resolved and any query or fragment dropped (the tree view takes neither).
/// A `..` that would climb past the root is discarded.
fn tree_url(dest: &str) -> String {
    let path = dest.split(['?', '#']).next().unwrap_or("");
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    if segments.is_empty() {
        "#!/tree".to_string()
    } else {
        // Encoded per segment: a README can link to a file whose name contains
        // '?' or '#', which would otherwise cut the route short.
        let encoded: Vec<String> = segments
            .iter()
            .map(|s| crate::route::encode_component(s))
            .collect();
        format!("#!/tree/{}", encoded.join("/"))
    }
}

/// The Yew component used to mount the readme view into the DOM.
#[function_component(ReadmeView)]
pub(crate) fn readme_view(props: &ReadmeProps) -> Html {
    match &props.content {
        Some(ReadmeContent::Markdown(html)) => {
            html! { <MarkdownFrame html={html.clone()} /> }
        }
        Some(ReadmeContent::Plain(text)) => html! { <pre class="readme">{ text.clone() }</pre> },
        None => html! { <p class="msg">{ "No README found." }</p> },
    }
}

/// The height (CSS px) the frame is given before its content has been measured.
const INITIAL_FRAME_HEIGHT: i32 = 150;

#[derive(Properties, PartialEq, Clone)]
pub(crate) struct MarkdownFrameProps {
    pub html: String,
}

/// A README's rendered markdown, shown in a sandboxed `srcdoc` frame.
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
    // whenever the document changes so a shorter README can shrink again.
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
            class="readme-frame"
            title="README"
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

    /// Render `ReadmeView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR
    /// and why indentation is omitted (which is also what keeps the `<pre>`
    /// body byte-exact here).
    ///
    /// SSR has no DOM, so the frame renders with an empty stylesheet href and
    /// its initial (unmeasured) height.
    fn render(props: ReadmeProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<ReadmeView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn readme_html_plain() {
        insta::assert_snapshot!(render(ReadmeProps {
            content: Some(ReadmeContent::Plain(
                "webgit\n\nA client-side Git viewer.\n".to_string()
            )),
        }));
    }

    #[test]
    fn readme_html_plain_escapes_markup() {
        // Plain text is rendered verbatim, so markup in it must be escaped.
        insta::assert_snapshot!(render(ReadmeProps {
            content: Some(ReadmeContent::Plain(
                "<script>alert(1)</script> & <b>bold</b>\n".to_string()
            )),
        }));
    }

    #[test]
    fn readme_html_missing() {
        insta::assert_snapshot!(render(ReadmeProps { content: None }));
    }

    /// The whole frame element, including the `srcdoc` document: the sandbox
    /// flags are what make passing raw HTML through safe, so they belong in a
    /// snapshot where a change to them is visible in review.
    #[test]
    fn readme_html_markdown_frame() {
        insta::assert_snapshot!(render(ReadmeProps {
            content: Some(ReadmeContent::Markdown(markdown_to_html(
                "# webgit\n\nA *client-side* Git viewer.\n\n\
                 | a | b |\n|---|---|\n| 1 | 2 |\n"
            ))),
        }));
    }

    #[test]
    fn markdown_renders_gfm_extensions() {
        // Tables, strikethrough and task lists are all opted into explicitly.
        let html = markdown_to_html("| a |\n|---|\n| 1 |\n\n~~gone~~\n\n- [x] done\n- [ ] todo\n");
        assert!(html.contains("<table>"), "tables: {html}");
        assert!(html.contains("<del>"), "strikethrough: {html}");
        assert!(html.contains("type=\"checkbox\""), "task lists: {html}");
    }

    #[test]
    fn markdown_drops_raw_html() {
        // `render.unsafe` is off, so a README's own markup never reaches the
        // frame. The sandbox is a backstop for this, not the reason it's safe.
        let html = markdown_to_html("<p align=\"center\"><b>hi</b></p>\n");
        assert!(!html.contains("<p align"), "{html}");
        let script = markdown_to_html("<script>alert(1)</script>\n");
        assert!(!script.contains("<script"), "{script}");
    }

    #[test]
    fn relative_links_route_to_the_tree_view() {
        // A repo-relative link becomes an in-app tree URL. Where it opens is
        // the frame document's business (`<base target="_top">`), not the URL's.
        let html = markdown_to_html("[setup](docs/setup.md)\n");
        assert!(html.contains(r##"href="#!/tree/docs/setup.md""##), "{html}");
    }

    #[test]
    fn absolute_links_are_left_alone() {
        let html = markdown_to_html("[home](https://example.org/x?a=1&b=2)\n");
        assert!(
            html.contains(r#"href="https://example.org/x?a=1&amp;b=2""#),
            "{html}"
        );
    }

    #[test]
    fn fragment_links_go_nowhere() {
        // A fragment can't scroll a content-height frame, so it points at the
        // route the reader is already on rather than navigating the app away.
        let html = markdown_to_html("[top](#intro)\n");
        assert!(html.contains(r##"href="#!/readme""##), "{html}");
    }

    #[test]
    fn script_bearing_links_are_dropped_by_comrak() {
        // Checked against the original URL, before `rewrite_url` sees it — so
        // this holds for images too, which have no rewriter of their own.
        let link = markdown_to_html("[x](javascript:alert(1))\n");
        assert!(!link.contains("javascript:"), "{link}");
        let image = markdown_to_html("![x](javascript:alert(1))\n");
        assert!(!image.contains("javascript:"), "{image}");
    }

    #[test]
    fn bare_urls_are_linkified() {
        // GFM's autolink extension, which plain CommonMark does not do. The
        // trailing full stop is punctuation, not part of the URL.
        let html = markdown_to_html("See https://example.org/docs.\n");
        assert!(
            html.contains(r#"<a href="https://example.org/docs">https://example.org/docs</a>."#),
            "{html}"
        );
    }

    #[test]
    fn bare_www_urls_are_linkified() {
        let html = markdown_to_html("www.example.org\n");
        assert!(html.contains(">www.example.org</a>"), "{html}");
    }

    #[test]
    fn autolinking_skips_code_and_existing_links() {
        // Inline code, a fenced block, and the text of an explicit link are all
        // left alone — the last would otherwise nest one <a> inside another.
        let html = markdown_to_html(
            "`https://example.org`\n\n```\nhttps://example.org\n```\n\n[https://example.org](https://other.example)\n",
        );
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(html.contains(r#"href="https://other.example""#), "{html}");
    }

    #[test]
    fn test_tree_url_normalizes_paths() {
        assert_eq!(tree_url("docs/setup.md"), "#!/tree/docs/setup.md");
        assert_eq!(tree_url("./docs/setup.md"), "#!/tree/docs/setup.md");
        // A leading slash still means the repository root, not the web root.
        assert_eq!(tree_url("/docs/setup.md"), "#!/tree/docs/setup.md");
        assert_eq!(tree_url("docs/../src/lib.rs"), "#!/tree/src/lib.rs");
        // Climbing past the root is clamped rather than escaping the repo.
        assert_eq!(tree_url("../../etc/passwd"), "#!/tree/etc/passwd");
        // The tree view takes neither a query nor a fragment.
        assert_eq!(tree_url("docs/setup.md#install"), "#!/tree/docs/setup.md");
        assert_eq!(tree_url(""), "#!/tree");
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
        assert_eq!(rewrite_url("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(rewrite_url("docs/setup.md"), "#!/tree/docs/setup.md");
        assert_eq!(rewrite_url("#intro"), "#!/readme");
    }

    #[test]
    fn frame_document_embeds_body_and_stylesheet() {
        let doc = frame_document("<h1>hi</h1>", "/assets/styles-abc123.css");
        assert!(doc.contains("<link rel=\"stylesheet\" href=\"/assets/styles-abc123.css\">"));
        assert!(doc.contains("<body class=\"markdown-body\"><h1>hi</h1></body>"));
    }
}
