//! The blob view's tests: how a file is classified, what the caps refuse, and
//! what the two alternate views offer.

use super::view::blob_view;
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
    render_selection(path, data, head, render, None)
}

/// As [`render_route`], but with `lines` selected — the field the router
/// sets after [`build_blob_props`] has run.
fn render_selection(
    path: &str,
    data: &[u8],
    head: Option<&str>,
    render: bool,
    lines: Option<LineRange>,
) -> String {
    let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    let path = path.to_string();
    let data = data.to_vec();
    let head = head.map(String::from);
    let html = futures::executor::block_on(
        yew::ServerRenderer::<BlobView>::with_props(move || {
            let mut props = build_blob_props(id, &path, data, head.as_deref(), render);
            props.lines = lines;
            props
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
    render_route_with_url(path, data, false, url)
}

/// As [`render_with_url`], but for the `?render=1` view — which an SVG
/// needs the object URL for, the same way a raster image does.
fn render_route_with_url(path: &str, data: &[u8], render: bool, url: &str) -> String {
    let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    let path = path.to_string();
    let data = data.to_vec();
    let url = url.to_string();
    let html = futures::executor::block_on(
        yew::ServerRenderer::<BlobViewWithUrl>::with_props(move || WithUrlProps {
            blob: build_blob_props(id, &path, data, None, render),
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

/// An SVG's source view: an ordinary text blob, plus the link to the
/// picture.
#[test]
fn test_blob_html_svg_source() {
    insta::assert_snapshot!(render_path("img/logo.svg", SVG));
}

/// The picture: an `<img>` over the same object URL the download link uses,
/// and a link back to the source.
#[test]
fn test_blob_html_svg_rendered() {
    insta::assert_snapshot!(render_route_with_url(
        "img/logo.svg",
        SVG,
        true,
        "blob:fake-url"
    ));
}

/// A range selects every line between its ends, inclusive, and leaves the
/// rest of the table alone.
#[test]
fn test_blob_html_line_range_selected() {
    insta::assert_snapshot!(render_selection(
        "src/main.rs",
        b"one\ntwo\nthree\nfour\nfive\n",
        None,
        false,
        Some(LineRange { start: 2, end: 4 }),
    ));
}

/// A single-line anchor is a range whose ends are equal, so it highlights
/// exactly the one row.
#[test]
fn test_blob_html_single_line_selected() {
    insta::assert_snapshot!(render_selection(
        "src/main.rs",
        b"one\ntwo\nthree\n",
        None,
        false,
        Some(LineRange::single(2)),
    ));
}

/// Every line's link carries the blob's whole route, including the `?h=`
/// it was reached through. A bare `#n2` would name no route at all and land
/// the reader on the summary page.
#[test]
fn test_blob_line_links_carry_the_route() {
    let html = render_route("src/main.rs", b"one\ntwo\n", Some("v1.0"), false);
    assert!(
        html.contains(r##"href="#!/tree/src/main.rs?h=v1.0#n2""##),
        "line link dropped the route: {html}"
    );
}

/// A selection outside the file selects nothing rather than clamping to the
/// last line: `#n900` on a 3-line file is a stale link, and highlighting an
/// arbitrary row would misrepresent it as the one that was asked for.
#[test]
fn test_blob_selection_past_end_of_file() {
    let html = render_selection(
        "src/main.rs",
        b"one\ntwo\nthree\n",
        None,
        false,
        Some(LineRange {
            start: 900,
            end: 902,
        }),
    );
    assert!(
        !html.contains(r#"class="hl""#),
        "selected a row that doesn't exist: {html}"
    );
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

/// An SVG toggles both ways too, and on the same ref.
#[test]
fn test_blob_svg_alt_view_toggles_both_ways() {
    let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    let source = build_blob_props(id, "img/a.svg", SVG.to_vec(), Some("v1.0"), false);
    assert_eq!(
        source.alt_view.map(|a| (a.label, a.url)),
        Some(("rendered", "#!/tree/img/a.svg?h=v1.0&render=1".to_string()))
    );
    let rendered = build_blob_props(id, "img/a.svg", SVG.to_vec(), Some("v1.0"), true);
    assert_eq!(
        rendered.alt_view.map(|a| (a.label, a.url)),
        Some(("source", "#!/tree/img/a.svg?h=v1.0".to_string()))
    );
}

/// The link markdown wouldn't get: an SVG the text view refuses still has a
/// working rendered view, so it is still offered one.
#[test]
fn test_blob_svg_offers_rendered_even_when_source_is_refused() {
    let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
    for (case, data) in [
        ("oversized", vec![b'x'; MAX_BLOB_BYTES + 1]),
        ("binary", b"<svg\0/>".to_vec()),
    ] {
        let props = build_blob_props(id, "big.svg", data, None, false);
        assert_eq!(props.alt_view.map(|a| a.label), Some("rendered"), "{case}");
    }
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
    assert_eq!(
        BlobContent::Image {
            format: ImageFormat::Svg
        }
        .mime(),
        "image/svg+xml"
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

const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>\n";

/// SVG is text, so the source view is what it gets by default — the same
/// treatment markdown gets, and for the same reason.
#[test]
fn test_svg_is_source_until_asked() {
    assert!(matches!(
        blob_content_source("icon.svg", SVG),
        BlobContent::Text(_)
    ));
    assert!(matches!(
        blob_content("icon.svg", SVG, Some(&root_base())),
        BlobContent::Image {
            format: ImageFormat::Svg
        }
    ));
}

/// The request is what promotes it, not the bytes: no signature is checked,
/// so a `.svg` holding anything at all draws (or fails to draw) as one.
#[test]
fn test_svg_render_does_not_sniff() {
    assert!(matches!(
        blob_content("lies.svg", b"not xml at all\n", Some(&root_base())),
        BlobContent::Image {
            format: ImageFormat::Svg
        }
    ));
    // And the extension is required: SVG bytes under another name stay text.
    assert!(matches!(
        blob_content("icon.txt", SVG, Some(&root_base())),
        BlobContent::Text(_)
    ));
}

/// Unlike markdown, the rendered view sits ahead of the binary check and
/// the caps — it is one `<img>`, whatever the file's size or bytes — so an
/// SVG the text view would refuse still draws.
#[test]
fn test_svg_render_is_not_capped_or_sniffed() {
    for data in [
        vec![b'x'; MAX_BLOB_BYTES + 1],
        b"x\n".repeat(MAX_BLOB_LINES + 1),
        b"<svg\0/>".to_vec(),
    ] {
        assert!(matches!(
            blob_content("big.svg", &data, Some(&root_base())),
            BlobContent::Image {
                format: ImageFormat::Svg
            }
        ));
    }
}

#[test]
fn test_is_svg() {
    assert!(is_svg("icon.svg"));
    assert!(is_svg("ICON.SVG"));
    assert!(is_svg("a/b/c.Svg"));
    for name in ["icon", "svg", "icon.svgz", "icon.svg.gz", "icon.png"] {
        assert!(!is_svg(name), "{name}");
    }
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
    assert_eq!(ImageFormat::Svg.mime(), "image/svg+xml");
}

/// `.svg` included: it is deliberately not part of the sniffing path, since
/// there is no signature to hold the extension to.
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
