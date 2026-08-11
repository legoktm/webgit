use crate::render::markdown::{LinkBase, MarkdownFrame, markdown_to_html};
use crate::render::{is_binary, use_object_url};
use crate::route::tree_url;
use gib::object::ObjectId;
use std::rc::Rc;
use yew::prelude::*;

/// The largest blob rendered as text, in bytes.
///
/// This is a rendering budget, not a download one: by the time the view sees a
/// blob the object layer has already decompressed the whole zlib stream into
/// memory, so nothing is saved by refusing it *earlier*. What the cap avoids is
/// everything downstream — the `from_utf8_lossy` copy, a `String` per line, and
/// a `<tr>` per line in the DOM, none of which the browser recovers from
/// quickly on a vendored binary or a minified bundle.
///
/// 1 MiB because that is comfortably past the point where a file stops being
/// something a person reads in a browser: the largest hand-written source in
/// most repositories is a few hundred KiB, and what exceeds 1 MiB is generated,
/// vendored or minified. Under the cap the copy costs a millisecond or so,
/// which is not worth reasoning about further.
const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// The largest blob rendered as text, in lines.
///
/// A second axis, because size in bytes and size in rows fail differently: a
/// 5 MiB single-line minified bundle is one enormous row that the byte cap
/// catches, while a 900 KiB log file slips under the byte cap and still asks
/// the browser to lay out hundreds of thousands of nodes. Each line costs four
/// (the row, two cells and the line-number anchor), so 20 000 lines is ~80 000
/// nodes — heavy but survivable, and an order of magnitude short of the point
/// where a click freezes the tab.
const MAX_BLOB_LINES: usize = 20_000;

/// An image format rendered inline rather than reported as binary.
///
/// Deliberately only the three raster formats git repositories actually carry
/// in bulk. SVG is absent and should stay absent: it is text, so it already
/// renders as source, and showing it instead means handing repository-controlled
/// XML to the browser's parser. An `<img>` is a passive context — no scripts,
/// no external fetches — so that would not be *unsafe*, but it is a bigger
/// decision than "PNGs should look like PNGs" and shouldn't ride along with it.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
}

impl ImageFormat {
    /// The MIME type given to the `Blob`. This, not the file name, is what the
    /// browser decodes by: the extension never leaves this module.
    fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
        }
    }

    fn from_extension(filename: &str) -> Option<Self> {
        let (_, ext) = filename.rsplit_once('.')?;
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "gif" => Some(ImageFormat::Gif),
            _ => None,
        }
    }

    /// Whether `data` opens with this format's signature. Only the leading
    /// bytes are examined: enough to tell the format apart from a file that
    /// merely claims the extension, which is all this is for.
    fn matches_magic(self, data: &[u8]) -> bool {
        match self {
            ImageFormat::Png => data.starts_with(b"\x89PNG\r\n\x1a\n"),
            // SOI followed by the first marker's 0xFF. Every real JPEG variant
            // (JFIF, Exif, raw) shares this prefix and differs after it.
            ImageFormat::Jpeg => data.starts_with(b"\xff\xd8\xff"),
            ImageFormat::Gif => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
        }
    }
}

/// The format to render `filename` as, or `None` to fall through to the normal
/// text/binary handling.
///
/// Both the extension and the signature have to agree. Either alone would be
/// enough to pick a MIME type, but requiring both keeps the two failure modes
/// from turning into misrenderings: a `.png` holding something else is reported
/// as binary rather than handed to the image decoder, and a file that happens
/// to start with a PNG signature is not promoted out of the text view on the
/// strength of its first eight bytes.
fn image_format(filename: &str, data: &[u8]) -> Option<ImageFormat> {
    let format = ImageFormat::from_extension(filename)?;
    format.matches_magic(data).then_some(format)
}

/// A blob's content, in the form it should be displayed. Every variant but
/// [`BlobContent::Text`] and [`BlobContent::Image`] carries the measurement
/// that rejected it, since that is the only thing the view still has to say
/// about a file it won't show.
///
/// The bytes themselves are not in here: they live on [`BlobProps`], because
/// the download link needs them whichever way the blob is classified.
#[derive(PartialEq, Clone)]
pub(crate) enum BlobContent {
    /// The blob's lines; a line's 1-based number is its index here.
    Text(Vec<String>),
    /// A markdown blob asked for with `?render=1`, already rendered to HTML and
    /// bound for the sandboxed frame.
    Markdown(String),
    /// A blob to render as an image, in the format its object URL should claim.
    Image { format: ImageFormat },
    /// A blob git's heuristic calls binary, with its size in bytes.
    Binary { bytes: usize },
    /// A blob over [`MAX_BLOB_BYTES`], with its size in bytes.
    TooManyBytes { bytes: usize },
    /// A blob over [`MAX_BLOB_LINES`], with its line count.
    TooManyLines { lines: usize },
}

impl BlobContent {
    /// The MIME type to give the object URL the view builds over the blob.
    ///
    /// `application/octet-stream` for everything that isn't a recognised image:
    /// the URL's only other consumer is the download link, and the point there
    /// is to hand the bytes over untouched rather than to invite the browser to
    /// display them.
    fn mime(&self) -> &'static str {
        match self {
            BlobContent::Image { format } => format.mime(),
            _ => "application/octet-stream",
        }
    }
}

/// The blob's other view — rendered markdown from the source, or the source
/// from the rendered markdown — as the link that reaches it.
#[derive(PartialEq, Clone)]
pub(crate) struct AltView {
    pub url: String,
    pub label: &'static str,
}

/// The view inputs for a blob: its id, its bytes and how to display them.
/// Doubles as the component's props and the test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct BlobProps {
    pub blob_id: String,
    /// The blob's file name — the last component of its path. Used as the
    /// downloaded file's name, as an image's alt text, and to name the frame a
    /// rendered markdown blob is shown in.
    pub name: String,
    /// The blob's bytes, as read from the object. `Rc` so that the props clone
    /// on every re-render stays a refcount bump rather than a copy of the whole
    /// file.
    pub data: Rc<Vec<u8>>,
    pub content: BlobContent,
    /// The link to the blob's other view, for a markdown blob that has one.
    pub alt_view: Option<AltView>,
}

/// Whether a file name is one this view will render as markdown.
fn is_markdown(filename: &str) -> bool {
    let Some((_, ext)) = filename.rsplit_once('.') else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown")
}

/// Build the blob view's props. `path` is the blob's full path within the tree
/// and `head` the ref it was reached through, which together address the blob's
/// other view; the path's last component recognises an image by extension and
/// names the download.
///
/// `render` is the route's `?render=1`: show a markdown blob rendered rather
/// than as source. It is a request, not a promise — a file that isn't markdown,
/// or one too large for the text view, is classified as it would have been
/// anyway.
pub(crate) fn build_blob_props(
    blob_id: ObjectId,
    path: &str,
    data: Vec<u8>,
    head: Option<&str>,
    render: bool,
) -> BlobProps {
    let filename = path.rsplit('/').next().unwrap_or(path);
    // Where the rendered document's own links resolve from: the directory it
    // sits in, and — for a bare fragment — the rendered view's own URL.
    let base = LinkBase {
        dir: path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string(),
        self_url: tree_url(path, head, true),
    };
    let content = blob_content(filename, &data, render.then_some(&base));
    let alt_view = match &content {
        BlobContent::Markdown(_) => Some(AltView {
            url: tree_url(path, head, false),
            label: "source",
        }),
        // Only from the source view, and only when the rendered view would
        // actually render: otherwise the link leads back to the page the reader
        // is already on.
        BlobContent::Text(_) if is_markdown(filename) && !render => Some(AltView {
            url: base.self_url,
            label: "rendered",
        }),
        _ => None,
    };
    BlobProps {
        blob_id: blob_id.to_string(),
        name: filename.to_string(),
        content,
        alt_view,
        data: Rc::new(data),
    }
}

/// Classify a blob, splitting it into lines only if it is going to be shown.
///
/// Every rejection is decided on the raw bytes, ahead of `from_utf8_lossy`:
/// that copy is the first thing the caps exist to avoid, so nothing that could
/// rule the blob out may run after it. Binary is checked before the size caps
/// because it is the more informative answer — a 50 MiB tarball should read as
/// binary rather than as an oversized text file.
///
/// Images come first of all, since a recognised image is the most specific
/// thing that can be said about a blob and the two checks are a few byte
/// comparisons. They are deliberately not size-capped: [`MAX_BLOB_BYTES`] is a
/// budget for building a DOM node per line, and an image is one node no matter
/// how large. The browser decodes it lazily and can drop it again, which is
/// more than can be said for the `String`s the text path would allocate.
///
/// `render` is `Some` when the route asked for markdown to be rendered, and
/// carries where the document's links resolve from. Rendering sits *after* the
/// caps rather than beside the image check: comrak's output is a DOM the browser
/// has to lay out like any other, so an over-cap markdown file is refused for
/// the same reason an over-cap source file is.
fn blob_content(filename: &str, data: &[u8], render: Option<&LinkBase>) -> BlobContent {
    if let Some(format) = image_format(filename, data) {
        return BlobContent::Image { format };
    }
    if is_binary(data) {
        return BlobContent::Binary { bytes: data.len() };
    }
    if data.len() > MAX_BLOB_BYTES {
        return BlobContent::TooManyBytes { bytes: data.len() };
    }
    let lines = count_lines(data);
    if lines > MAX_BLOB_LINES {
        return BlobContent::TooManyLines { lines };
    }

    let text = String::from_utf8_lossy(data);
    if let Some(base) = render
        && is_markdown(filename)
    {
        return BlobContent::Markdown(markdown_to_html(&text, base));
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline yields a spurious empty final element; drop it so a
    // file ending in '\n' renders the same as one that doesn't.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    BlobContent::Text(lines.into_iter().map(String::from).collect())
}

/// How many rows `data` would render as, counted without decoding it: one per
/// '\n', plus one more for a final line that isn't newline-terminated. Kept in
/// step with the split in [`blob_content`], which drops that same trailing
/// empty element.
fn count_lines(data: &[u8]) -> usize {
    let newlines = data.iter().filter(|&&b| b == b'\n').count();
    if data.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}

/// The Yew component used to mount the blob view into the DOM.
///
/// The one thing it does beyond calling `blob_view` is mint the object URL over
/// the blob's bytes, which is a side effect and so can't live in the markup.
/// Passing the URL in keeps `blob_view` a plain function of its inputs, which
/// is what lets the tests render it without a DOM.
#[function_component(BlobView)]
pub(crate) fn blob_view_component(props: &BlobProps) -> Html {
    let url = use_object_url(props.content.mime(), &props.data);
    blob_view(props, &url)
}

/// The blob view's markup. `url` is an object URL over `props.data`, or empty
/// if one couldn't be made — under SSR, or if the browser refused. Everything
/// that needs it is omitted rather than emitted with an empty `src`/`href`,
/// which browsers resolve to the current page and re-fetch.
pub(crate) fn blob_view(props: &BlobProps, url: &str) -> Html {
    let BlobProps {
        blob_id,
        name,
        content,
        alt_view,
        data: _,
    } = props;

    html! {
        <>
            <div class="blob-info">
                { "blob: " }{ blob_id }
                if !url.is_empty() {
                    { " · " }
                    <a class="blob-download" href={url.to_string()} download={name.clone()}>
                        { "download" }
                    </a>
                }
                if let Some(alt) = alt_view {
                    { " · " }
                    <a class="blob-alt-view" href={alt.url.clone()}>{ alt.label }</a>
                }
            </div>
            { match content {
                BlobContent::Text(lines) => html! {
                    <table class="blob-table">
                        <tbody>
                            { for lines.iter().enumerate().map(|(i, line)| blob_row(i + 1, line)) }
                        </tbody>
                    </table>
                },
                BlobContent::Markdown(html) => html! {
                    <MarkdownFrame html={html.clone()} title={name.clone()} />
                },
                BlobContent::Image { .. } if url.is_empty() => html! {},
                BlobContent::Image { .. } => html! {
                    <img class="blob-image" src={url.to_string()} alt={name.clone()} />
                },
                BlobContent::Binary { bytes } => html! {
                    <p class="msg">{ format!("Binary file ({bytes} bytes).") }</p>
                },
                BlobContent::TooManyBytes { bytes } => html! {
                    <p class="msg">{
                        format!("File too large to display ({bytes} bytes, limit {MAX_BLOB_BYTES}).")
                    }</p>
                },
                BlobContent::TooManyLines { lines } => html! {
                    <p class="msg">{
                        format!("File too large to display ({lines} lines, limit {MAX_BLOB_LINES}).")
                    }</p>
                },
            } }
        </>
    }
}

fn blob_row(n: usize, line: &str) -> Html {
    let row_id = format!("n{n}");
    let href = format!("#n{n}");
    html! {
        <tr id={row_id}>
            <td class="lno"><a href={href}>{ n }</a></td>
            <td class="code">{ line }</td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG's 8-byte signature, followed by the start of its IHDR. Enough for
    /// [`ImageFormat::matches_magic`], which is all the classifier looks at.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR";

    /// Render `BlobView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR
    /// and why indentation is omitted.
    ///
    /// Named `.txt` so the image classifier stays out of the way; the image
    /// path has its own helper below.
    fn render(data: &[u8]) -> String {
        render_path("file.txt", data)
    }

    /// The classification of `data` under `filename`, with markdown left as
    /// source — what almost every case here wants.
    fn blob_content_source(filename: &str, data: &[u8]) -> BlobContent {
        blob_content(filename, data, None)
    }

    /// A [`LinkBase`] for a document at the repository root, for the cases that
    /// do render markdown.
    fn root_base() -> LinkBase {
        LinkBase {
            dir: String::new(),
            self_url: "#!/tree/x.md?render=1".to_string(),
        }
    }

    /// The props are built *inside* the closure rather than passed into it:
    /// `ServerRenderer` requires the closure be `Send`, and `BlobProps` holds an
    /// `Rc`. Moving the inputs in instead (both `Send`) keeps the refcount
    /// non-atomic, which is what a single-threaded WASM app wants.
    fn render_path(path: &str, data: &[u8]) -> String {
        render_route(path, data, None, false)
    }

    /// As [`render_path`], but for a blob reached through a `?h=` ref and/or
    /// with `?render=1` — the inputs that decide the alt-view link.
    fn render_route(path: &str, data: &[u8], head: Option<&str>, render: bool) -> String {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let path = path.to_string();
        let data = data.to_vec();
        let head = head.map(String::from);
        let html = futures::executor::block_on(
            yew::ServerRenderer::<BlobView>::with_props(move || {
                build_blob_props(id, &path, data, head.as_deref(), render)
            })
            .hydratable(false)
            .render(),
        );
        html.replace("><", ">\n<")
    }

    /// A test-only component that renders the markup with an object URL already
    /// in hand, standing in for the effect that mints one in the browser. The
    /// URL is the only thing SSR can't produce, so this covers everything that
    /// depends on it — the download link, and the image.
    #[derive(Properties, PartialEq, Clone)]
    struct WithUrlProps {
        blob: BlobProps,
        url: String,
    }

    #[function_component(BlobViewWithUrl)]
    fn blob_view_with_url(props: &WithUrlProps) -> Html {
        blob_view(&props.blob, &props.url)
    }

    /// As [`render_path`], but with `url` standing in for the object URL.
    fn render_with_url(path: &str, data: &[u8], url: &str) -> String {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let path = path.to_string();
        let data = data.to_vec();
        let url = url.to_string();
        let html = futures::executor::block_on(
            yew::ServerRenderer::<BlobViewWithUrl>::with_props(move || WithUrlProps {
                blob: build_blob_props(id, &path, data, None, false),
                url,
            })
            .hydratable(false)
            .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_blob_html() {
        insta::assert_snapshot!(render(b"fn main() {\n    println!(\"hello\");\n}\n"));
    }

    #[test]
    fn test_blob_html_escapes_markup() {
        insta::assert_snapshot!(render(b"<script>alert(1)</script> & <b>bold</b>\n"));
    }

    #[test]
    fn test_blob_html_empty() {
        insta::assert_snapshot!(render(b""));
    }

    #[test]
    fn test_blob_no_trailing_newline_keeps_last_line() {
        let with = render(b"one\ntwo\n");
        let without = render(b"one\ntwo");
        assert_eq!(with, without);
    }

    /// A PNG header under a name that doesn't claim to be one: the NUL in it is
    /// inside git's 8000-byte window, so this is binary by the same rule the
    /// diff view uses.
    #[test]
    fn test_blob_html_binary() {
        insta::assert_snapshot!(render(PNG));
    }

    /// The image view under SSR: the object URL is minted in an effect, which
    /// doesn't run without a DOM, so the `<img>` is deliberately absent and only
    /// the info line remains. Locked in so that emitting an empty `src` instead
    /// would show up as a diff. The download link is absent for the same
    /// reason, and is covered with a URL in hand below.
    #[test]
    fn test_blob_html_image_ssr_omits_img() {
        insta::assert_snapshot!(render_path("logo.png", PNG));
    }

    /// What the browser actually shows once the URL exists: the download link
    /// in the info line, named after the file rather than the path it came
    /// from, and the `<img>` pointing at the same URL.
    #[test]
    fn test_blob_html_image_with_url() {
        insta::assert_snapshot!(render_with_url("docs/logo.png", PNG, "blob:fake-url"));
    }

    /// Every classification gets the same download link, including the ones
    /// whose content isn't shown at all — where it is the only thing the view
    /// still offers.
    #[test]
    fn test_blob_html_text_download_link() {
        insta::assert_snapshot!(render_with_url(
            "src/main.rs",
            b"fn main() {}\n",
            "blob:fake"
        ));
    }

    #[test]
    fn test_blob_html_binary_download_link() {
        insta::assert_snapshot!(render_with_url("payload.bin", PNG, "blob:fake"));
    }

    /// A file name with markup in it is escaped in the `download` attribute
    /// like anywhere else.
    #[test]
    fn test_blob_html_download_name_is_escaped() {
        let html = render_with_url("a\"><script>.txt", b"x\n", "blob:fake");
        assert!(!html.contains("<script>"), "{html}");
    }

    /// A markdown file's source view, which offers the rendered one. The link
    /// is the only difference from any other text blob.
    #[test]
    fn test_blob_html_markdown_source() {
        insta::assert_snapshot!(render_path("docs/setup.md", b"# Setup\n\nRun it.\n"));
    }

    /// The rendered view: the frame in place of the source table, and a link
    /// back to the source. Under SSR the frame carries an empty stylesheet href
    /// and its initial height, as in the readme frame snapshot.
    #[test]
    fn test_blob_html_markdown_rendered() {
        insta::assert_snapshot!(render_route(
            "docs/setup.md",
            b"# Setup\n\nSee [install](install.md).\n",
            None,
            true
        ));
    }

    /// Both directions of the toggle stay on the ref the blob was reached
    /// through — unlike the links *inside* the document, which address paths.
    #[test]
    fn test_blob_markdown_alt_view_keeps_the_ref() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let source = build_blob_props(id, "docs/a.md", b"# x\n".to_vec(), Some("v1.0"), false);
        assert_eq!(
            source.alt_view.map(|a| (a.label, a.url)),
            Some(("rendered", "#!/tree/docs/a.md?h=v1.0&render=1".to_string()))
        );
        let rendered = build_blob_props(id, "docs/a.md", b"# x\n".to_vec(), Some("v1.0"), true);
        assert_eq!(
            rendered.alt_view.map(|a| (a.label, a.url)),
            Some(("source", "#!/tree/docs/a.md?h=v1.0".to_string()))
        );
    }

    /// A document below the root has its relative links resolved against its
    /// own directory, which is what the `LinkBase` handed to the renderer is
    /// for.
    #[test]
    fn test_blob_markdown_links_resolve_against_the_documents_directory() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let props = build_blob_props(id, "docs/guide/a.md", b"[b](b.md)\n".to_vec(), None, true);
        let BlobContent::Markdown(html) = props.content else {
            panic!("expected rendered markdown");
        };
        assert!(
            html.contains(r##"href="#!/tree/docs/guide/b.md""##),
            "{html}"
        );
    }

    /// Only markdown gets the link, and only when the other view would show
    /// something: a file the text view refuses is refused rendered too, and
    /// must not offer a link back to the page the reader is on.
    #[test]
    fn test_blob_alt_view_absent_where_there_is_nothing_to_toggle() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let cases: [(&str, Vec<u8>, bool); 4] = [
            // Not markdown.
            ("src/main.rs", b"fn main() {}\n".to_vec(), false),
            ("logo.png", PNG.to_vec(), false),
            // Markdown, but past the caps — with and without the flag.
            ("big.md", vec![b'x'; MAX_BLOB_BYTES + 1], false),
            ("big.md", vec![b'x'; MAX_BLOB_BYTES + 1], true),
        ];
        for (path, data, render) in cases {
            let props = build_blob_props(id, path, data, None, render);
            assert!(props.alt_view.is_none(), "{path} render={render}");
        }
    }

    /// The caps and the binary check come first, so asking for markdown can't
    /// route a file around them.
    #[test]
    fn test_blob_markdown_is_still_capped_and_sniffed() {
        assert!(matches!(
            blob_content(
                "big.md",
                &vec![b'x'; MAX_BLOB_BYTES + 1],
                Some(&root_base())
            ),
            BlobContent::TooManyBytes { .. }
        ));
        assert!(matches!(
            blob_content(
                "many.md",
                &b"x\n".repeat(MAX_BLOB_LINES + 1),
                Some(&root_base())
            ),
            BlobContent::TooManyLines { .. }
        ));
        assert!(matches!(
            blob_content("weird.md", b"# title\0\n", Some(&root_base())),
            BlobContent::Binary { .. }
        ));
    }

    /// Without the flag a `.md` is source, whatever else is true of it.
    #[test]
    fn test_blob_markdown_only_renders_when_asked() {
        assert!(matches!(
            blob_content_source("a.md", b"# x\n"),
            BlobContent::Text(_)
        ));
    }

    #[test]
    fn test_is_markdown() {
        assert!(is_markdown("README.md"));
        assert!(is_markdown("a.MD"));
        assert!(is_markdown("notes.markdown"));
        for name in ["README", "a.mdx", "a.txt", "md", "a.md.gz"] {
            assert!(!is_markdown(name), "{name}");
        }
    }

    #[test]
    fn test_blob_html_too_many_bytes() {
        // One very long line: over the byte cap without approaching the line
        // cap, i.e. the minified-bundle shape.
        insta::assert_snapshot!(render(&vec![b'x'; MAX_BLOB_BYTES + 1]));
    }

    #[test]
    fn test_blob_html_too_many_lines() {
        // The mirror image: comfortably inside the byte cap, far past the line
        // cap, i.e. the giant-log shape.
        insta::assert_snapshot!(render(&b"x\n".repeat(MAX_BLOB_LINES + 1)));
    }

    #[test]
    fn test_blob_binary_wins_over_size() {
        // A blob that trips both checks is reported as binary, which says more
        // about it than its size does.
        let mut data = vec![0u8; MAX_BLOB_BYTES + 1];
        data[0] = b'x';
        assert!(matches!(
            blob_content_source("file.txt", &data),
            BlobContent::Binary { .. }
        ));
    }

    #[test]
    fn test_blob_caps_are_inclusive() {
        // Exactly at either limit still renders; only past it is refused.
        assert!(matches!(
            blob_content_source("file.txt", &vec![b'x'; MAX_BLOB_BYTES]),
            BlobContent::Text(_)
        ));
        assert!(matches!(
            blob_content_source("file.txt", &b"x\n".repeat(MAX_BLOB_LINES)),
            BlobContent::Text(_)
        ));
    }

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0";
    const GIF: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0";

    fn format_of(path: &str, data: &[u8]) -> Option<ImageFormat> {
        match blob_content_source(path, data) {
            BlobContent::Image { format, .. } => Some(format),
            _ => None,
        }
    }

    #[test]
    fn test_image_recognised_formats() {
        assert_eq!(format_of("logo.png", PNG), Some(ImageFormat::Png));
        assert_eq!(format_of("photo.jpg", JPEG), Some(ImageFormat::Jpeg));
        assert_eq!(format_of("photo.jpeg", JPEG), Some(ImageFormat::Jpeg));
        assert_eq!(format_of("anim.gif", GIF), Some(ImageFormat::Gif));
        // GIF87a is the older signature and equally valid.
        assert_eq!(
            format_of("old.gif", b"GIF87a\x01\0"),
            Some(ImageFormat::Gif)
        );
    }

    #[test]
    fn test_image_extension_match_is_case_insensitive() {
        assert_eq!(format_of("LOGO.PNG", PNG), Some(ImageFormat::Png));
        assert_eq!(format_of("Photo.JPeG", JPEG), Some(ImageFormat::Jpeg));
    }

    /// The bytes reach the props whatever the classification says, since the
    /// download link needs them either way.
    #[test]
    fn test_props_carry_bytes_for_every_content() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        for (path, data) in [
            ("logo.png", PNG),
            ("payload.bin", PNG),
            ("file.txt", b"hello\n"),
        ] {
            let props = build_blob_props(id, path, data.to_vec(), None, false);
            assert_eq!(props.data.as_slice(), data, "{path}");
        }
    }

    /// The name used for the download and for alt text is the last path
    /// component, not the whole path — `build_blob_props` is handed the full
    /// path from the route.
    #[test]
    fn test_build_blob_props_uses_filename_from_path() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let props = build_blob_props(id, "docs/img/logo.png", PNG.to_vec(), None, false);
        assert_eq!(props.name, "logo.png");
        assert!(matches!(props.content, BlobContent::Image { .. }));
    }

    /// An image's object URL claims its own type, so the `<img>` decodes;
    /// everything else is handed over as opaque bytes.
    #[test]
    fn test_content_mime() {
        assert_eq!(
            BlobContent::Image {
                format: ImageFormat::Png
            }
            .mime(),
            "image/png"
        );
        assert_eq!(BlobContent::Text(vec![]).mime(), "application/octet-stream");
        assert_eq!(
            BlobContent::Binary { bytes: 1 }.mime(),
            "application/octet-stream"
        );
    }

    /// The extension alone doesn't promote a blob out of the text/binary path.
    #[test]
    fn test_image_extension_without_magic_is_not_an_image() {
        // Plain text under an image name stays text, rather than becoming a
        // broken <img>.
        assert!(matches!(
            blob_content_source("fake.png", b"not actually a png\n"),
            BlobContent::Text(_)
        ));
        // Binary-but-not-PNG under an image name is reported as binary.
        assert!(matches!(
            blob_content_source("fake.png", b"PK\x03\x04\0\0"),
            BlobContent::Binary { .. }
        ));
        // A signature that doesn't match the extension it claims counts for
        // nothing: neither half of the pair is trusted on its own.
        assert!(matches!(
            blob_content_source("mislabelled.gif", PNG),
            BlobContent::Binary { .. }
        ));
    }

    /// The mirror image: the signature alone doesn't promote it either.
    #[test]
    fn test_image_magic_without_extension_is_not_an_image() {
        assert!(matches!(
            blob_content_source("payload.bin", PNG),
            BlobContent::Binary { .. }
        ));
        assert!(matches!(
            blob_content_source("noext", PNG),
            BlobContent::Binary { .. }
        ));
    }

    /// SVG is text and stays text. See [`ImageFormat`] for why it is excluded.
    #[test]
    fn test_svg_renders_as_text() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>\n";
        assert!(matches!(
            blob_content_source("icon.svg", svg),
            BlobContent::Text(_)
        ));
    }

    /// Images are exempt from the caps that govern the text view: they cost one
    /// DOM node regardless of size.
    #[test]
    fn test_image_is_not_size_capped() {
        let mut big = PNG.to_vec();
        big.resize(MAX_BLOB_BYTES + 1, 0);
        assert!(matches!(
            blob_content_source("huge.png", &big),
            BlobContent::Image { .. }
        ));
    }

    #[test]
    fn test_image_mime_types() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageFormat::Gif.mime(), "image/gif");
    }

    #[test]
    fn test_image_format_from_extension_rejects_others() {
        for name in ["a.svg", "a.txt", "a.webp", "a.png.txt", "a", "a."] {
            assert_eq!(ImageFormat::from_extension(name), None, "{name}");
        }
    }

    #[test]
    fn test_count_lines() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
        assert_eq!(count_lines(b"\n"), 1);
    }

    /// The line cap is decided on the raw bytes, so the count it uses has to
    /// agree with the number of rows the split below it would have produced.
    #[test]
    fn test_count_lines_matches_rendered_rows() {
        for case in [&b""[..], b"a", b"a\n", b"a\nb", b"a\nb\n", b"\n", b"\n\n"] {
            let BlobContent::Text(lines) = blob_content_source("file.txt", case) else {
                panic!("expected text for {case:?}");
            };
            assert_eq!(count_lines(case), lines.len(), "{case:?}");
        }
    }
}
