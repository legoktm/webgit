use crate::render::is_binary;
use git_async::object::ObjectId;
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
#[derive(PartialEq, Clone)]
pub(crate) enum BlobContent {
    /// The blob's lines; a line's 1-based number is its index here.
    Text(Vec<String>),
    /// A blob to render as an image, with the bytes to build the object URL
    /// from and the file name to use as its alt text. `Rc` so that the props
    /// clone on every re-render stays a refcount bump rather than a copy of
    /// the whole image.
    Image {
        format: ImageFormat,
        data: Rc<Vec<u8>>,
        name: String,
    },
    /// A blob git's heuristic calls binary, with its size in bytes.
    Binary { bytes: usize },
    /// A blob over [`MAX_BLOB_BYTES`], with its size in bytes.
    TooManyBytes { bytes: usize },
    /// A blob over [`MAX_BLOB_LINES`], with its line count.
    TooManyLines { lines: usize },
}

/// The view inputs for a blob: its id and its content. Doubles as the
/// component's props and the test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct BlobProps {
    pub blob_id: String,
    pub content: BlobContent,
}

/// Build the blob view's props. `path` is the blob's full path within the tree;
/// only its last component is used, to recognise an image by extension.
pub(crate) fn build_blob_props(blob_id: ObjectId, path: &str, data: &[u8]) -> BlobProps {
    let filename = path.rsplit('/').next().unwrap_or(path);
    BlobProps {
        blob_id: blob_id.to_string(),
        content: blob_content(filename, data),
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
fn blob_content(filename: &str, data: &[u8]) -> BlobContent {
    if let Some(format) = image_format(filename, data) {
        return BlobContent::Image {
            // The one copy on this path. `walk_to_blob` owns a `Vec` that could
            // be moved in instead, but threading ownership through for a single
            // memcpy of an already-in-memory image isn't worth the churn.
            data: Rc::new(data.to_vec()),
            format,
            name: filename.to_string(),
        };
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

/// The Yew component used to mount the blob view into the DOM. The markup lives
/// in the plain `blob_view` function below so it can be unit-tested without a
/// renderer.
#[function_component(BlobView)]
pub(crate) fn blob_view_component(props: &BlobProps) -> Html {
    blob_view(props)
}

pub(crate) fn blob_view(props: &BlobProps) -> Html {
    let BlobProps { blob_id, content } = props;

    html! {
        <>
            <div class="blob-info">
                { "blob: " }{ blob_id }
            </div>
            { match content {
                BlobContent::Text(lines) => html! {
                    <table class="blob-table">
                        <tbody>
                            { for lines.iter().enumerate().map(|(i, line)| blob_row(i + 1, line)) }
                        </tbody>
                    </table>
                },
                BlobContent::Image { format, data, name } => html! {
                    <BlobImage format={*format} data={data.clone()} name={name.clone()} />
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

#[derive(Properties, PartialEq, Clone)]
pub(crate) struct BlobImageProps {
    pub format: ImageFormat,
    pub data: Rc<Vec<u8>>,
    pub name: String,
}

/// An image blob, shown from an object URL over its bytes.
///
/// An object URL rather than a `data:` one because the bytes are already in
/// memory: base64 would add a third again in size and park the whole encoded
/// image in a DOM attribute, where a `blob:` URL is a short string the browser
/// resolves back to the buffer we already hold.
///
/// The URL is created in an effect, not during render, for two reasons: it is a
/// side effect with a matching teardown (an object URL pins its buffer until
/// revoked, so navigating between images would otherwise leak one per visit),
/// and it keeps `web_sys` off the render path, where the SSR-based tests run
/// without a DOM. Under SSR the effect never fires and the `<img>` is simply
/// not emitted — better than emitting one with an empty `src`, which browsers
/// resolve to the current page and re-fetch.
#[function_component(BlobImage)]
fn blob_image(props: &BlobImageProps) -> Html {
    let url = use_state(String::new);
    {
        let url = url.clone();
        use_effect_with(
            (props.format, props.data.clone()),
            move |(format, data): &(ImageFormat, Rc<Vec<u8>>)| {
                let created = object_url(*format, data).unwrap_or_default();
                url.set(created.clone());
                move || {
                    if !created.is_empty() {
                        let _ = web_sys::Url::revoke_object_url(&created);
                    }
                }
            },
        );
    }

    if url.is_empty() {
        return html! {};
    }
    html! {
        <img class="blob-image" src={(*url).clone()} alt={props.name.clone()} />
    }
}

/// Wrap `data` in a `Blob` of `format`'s MIME type and mint an object URL for
/// it. `None` if the browser refuses either step, which leaves the view
/// showing nothing rather than a broken image.
fn object_url(format: ImageFormat, data: &[u8]) -> Option<String> {
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(data));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(format.mime());
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
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

    /// The props are built *inside* the closure rather than passed into it:
    /// `ServerRenderer` requires the closure be `Send`, and `BlobProps` holds an
    /// `Rc` for image blobs. Moving the inputs in instead (both `Send`) keeps
    /// the refcount non-atomic, which is what a single-threaded WASM app wants.
    fn render_path(path: &str, data: &[u8]) -> String {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let path = path.to_string();
        let data = data.to_vec();
        let html = futures::executor::block_on(
            yew::ServerRenderer::<BlobView>::with_props(move || build_blob_props(id, &path, &data))
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
    /// would show up as a diff.
    #[test]
    fn test_blob_html_image_ssr_omits_img() {
        insta::assert_snapshot!(render_path("logo.png", PNG));
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
            blob_content("file.txt", &data),
            BlobContent::Binary { .. }
        ));
    }

    #[test]
    fn test_blob_caps_are_inclusive() {
        // Exactly at either limit still renders; only past it is refused.
        assert!(matches!(
            blob_content("file.txt", &vec![b'x'; MAX_BLOB_BYTES]),
            BlobContent::Text(_)
        ));
        assert!(matches!(
            blob_content("file.txt", &b"x\n".repeat(MAX_BLOB_LINES)),
            BlobContent::Text(_)
        ));
    }

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0";
    const GIF: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0";

    fn format_of(path: &str, data: &[u8]) -> Option<ImageFormat> {
        match blob_content(path, data) {
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

    #[test]
    fn test_image_carries_bytes_and_alt_text() {
        let BlobContent::Image { data, name, .. } = blob_content("logo.png", PNG) else {
            panic!("expected an image");
        };
        assert_eq!(data.as_slice(), PNG);
        assert_eq!(name, "logo.png");
    }

    /// The name used for alt text is the last path component, not the whole
    /// path — `build_blob_props` is handed the full path from the route.
    #[test]
    fn test_build_blob_props_uses_filename_from_path() {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let props = build_blob_props(id, "docs/img/logo.png", PNG);
        let BlobContent::Image { name, .. } = props.content else {
            panic!("expected an image");
        };
        assert_eq!(name, "logo.png");
    }

    /// The extension alone doesn't promote a blob out of the text/binary path.
    #[test]
    fn test_image_extension_without_magic_is_not_an_image() {
        // Plain text under an image name stays text, rather than becoming a
        // broken <img>.
        assert!(matches!(
            blob_content("fake.png", b"not actually a png\n"),
            BlobContent::Text(_)
        ));
        // Binary-but-not-PNG under an image name is reported as binary.
        assert!(matches!(
            blob_content("fake.png", b"PK\x03\x04\0\0"),
            BlobContent::Binary { .. }
        ));
        // A signature that doesn't match the extension it claims counts for
        // nothing: neither half of the pair is trusted on its own.
        assert!(matches!(
            blob_content("mislabelled.gif", PNG),
            BlobContent::Binary { .. }
        ));
    }

    /// The mirror image: the signature alone doesn't promote it either.
    #[test]
    fn test_image_magic_without_extension_is_not_an_image() {
        assert!(matches!(
            blob_content("payload.bin", PNG),
            BlobContent::Binary { .. }
        ));
        assert!(matches!(
            blob_content("noext", PNG),
            BlobContent::Binary { .. }
        ));
    }

    /// SVG is text and stays text. See [`ImageFormat`] for why it is excluded.
    #[test]
    fn test_svg_renders_as_text() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>\n";
        assert!(matches!(
            blob_content("icon.svg", svg),
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
            blob_content("huge.png", &big),
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
            let BlobContent::Text(lines) = blob_content("file.txt", case) else {
                panic!("expected text for {case:?}");
            };
            assert_eq!(count_lines(case), lines.len(), "{case:?}");
        }
    }
}
