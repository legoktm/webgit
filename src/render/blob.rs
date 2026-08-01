use crate::render::is_binary;
use git_async::object::ObjectId;
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

/// A blob's content, in the form it should be displayed. Every variant but
/// [`BlobContent::Text`] carries the measurement that rejected it, since that
/// is the only thing the view still has to say about a file it won't show.
#[derive(PartialEq, Clone)]
pub(crate) enum BlobContent {
    /// The blob's lines; a line's 1-based number is its index here.
    Text(Vec<String>),
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

pub(crate) fn build_blob_props(blob_id: ObjectId, data: &[u8]) -> BlobProps {
    BlobProps {
        blob_id: blob_id.to_string(),
        content: blob_content(data),
    }
}

/// Classify a blob, splitting it into lines only if it is going to be shown.
///
/// Every rejection is decided on the raw bytes, ahead of `from_utf8_lossy`:
/// that copy is the first thing the caps exist to avoid, so nothing that could
/// rule the blob out may run after it. Binary is checked first because it is
/// the more informative answer — a 50 MiB PNG should read as a PNG rather than
/// as an oversized text file.
fn blob_content(data: &[u8]) -> BlobContent {
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

    /// Render `BlobView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR
    /// and why indentation is omitted.
    fn render(data: &[u8]) -> String {
        let id = ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let props = build_blob_props(id, data);
        let html = futures::executor::block_on(
            yew::ServerRenderer::<BlobView>::with_props(move || props)
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

    /// A PNG header: the NUL in it is inside git's 8000-byte window, so this is
    /// binary by the same rule the diff view uses.
    #[test]
    fn test_blob_html_binary() {
        insta::assert_snapshot!(render(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR"));
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
        assert!(matches!(blob_content(&data), BlobContent::Binary { .. }));
    }

    #[test]
    fn test_blob_caps_are_inclusive() {
        // Exactly at either limit still renders; only past it is refused.
        assert!(matches!(
            blob_content(&vec![b'x'; MAX_BLOB_BYTES]),
            BlobContent::Text(_)
        ));
        assert!(matches!(
            blob_content(&b"x\n".repeat(MAX_BLOB_LINES)),
            BlobContent::Text(_)
        ));
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
            let BlobContent::Text(lines) = blob_content(case) else {
                panic!("expected text for {case:?}");
            };
            assert_eq!(count_lines(case), lines.len(), "{case:?}");
        }
    }
}
