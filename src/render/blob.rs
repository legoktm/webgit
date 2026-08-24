//! Showing one file: as source, as a picture, as rendered markdown, or as a
//! refusal when it is too large to be any of those.
//!
//! [`image`] decides whether a blob is a picture, and [`view`] holds the
//! markup; what is left here is the classification the two of them feed.

mod image;
mod view;

#[cfg(test)]
mod tests;

pub(crate) use view::BlobView;

use image::{ImageFormat, image_format};

use crate::render::is_binary;
use crate::render::markdown::{LinkBase, markdown_to_html};
use crate::route::{LineRange, blame_url, tree_url};
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
pub(crate) const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// The largest blob rendered as text, in lines.
///
/// A second axis, because size in bytes and size in rows fail differently: a
/// 5 MiB single-line minified bundle is one enormous row that the byte cap
/// catches, while a 900 KiB log file slips under the byte cap and still asks
/// the browser to lay out hundreds of thousands of nodes. Each line costs four
/// (the row, two cells and the line-number anchor), so 20 000 lines is ~80 000
/// nodes — heavy but survivable, and an order of magnitude short of the point
/// where a click freezes the tab.
pub(crate) const MAX_BLOB_LINES: usize = 20_000;

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
    /// A blob to draw as an image, in the format its object URL should claim.
    /// A raster format arrives here by being one; [`ImageFormat::Svg`] only by
    /// having been asked for with `?render=1`.
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

/// The blob's other view — the rendered form from the source, or the source
/// from the rendered form — as the link that reaches it.
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
    /// The link to the blob's other view, for a markdown or SVG blob that has
    /// one.
    pub alt_view: Option<AltView>,
    /// The blame view of the same file, linked beside the download. `None`
    /// when there are no lines to attribute — an image, or a file the text
    /// view turned down — since blame would only refuse it again.
    pub blame_url: Option<String>,
    /// This blob's own source-view URL, without a line anchor: what every line
    /// number's link is built on top of.
    ///
    /// A line anchor has to carry the whole route with it
    pub source_url: String,
    /// The lines the URL's `#n…` anchor selected, highlighted in the gutter and
    /// scrolled to on arrival. Set by the router rather than by
    /// [`build_blob_props`], because changing the selection must not re-resolve
    /// the route: the blob is already on screen and only its highlight moves.
    pub lines: Option<LineRange>,
}

/// Whether a file name is one this view will render as a picture on request.
fn is_svg(filename: &str) -> bool {
    filename
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("svg"))
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
/// `render` is the route's `?render=1`: show a blob rendered rather than as
/// source — markdown as a document, SVG as a picture. It is a request, not a
/// promise — a file that is neither, or a markdown file too large for the text
/// view, is classified as it would have been anyway.
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
    let source_url = tree_url(path, head, false);
    let content = blob_content(filename, &data, render.then_some(&base));
    let alt_view = match &content {
        // Back to the source, from either of the two rendered forms. A raster
        // image has no source view to go back to and doesn't match here.
        BlobContent::Markdown(_)
        | BlobContent::Image {
            format: ImageFormat::Svg,
        } => Some(AltView {
            url: source_url.clone(),
            label: "source",
        }),
        // Only from the source view, and only when the rendered view would
        // actually render: otherwise the link leads back to the page the reader
        // is already on.
        BlobContent::Text(_) if is_markdown(filename) && !render => Some(AltView {
            url: base.self_url,
            label: "rendered",
        }),
        // An SVG's rendered view is an `<img>`, which doesn't depend on the
        // source view having fit — so unlike markdown it is still worth
        // offering from a blob the text view turned down as binary or oversized.
        _ if is_svg(filename) && !render => Some(AltView {
            url: base.self_url,
            label: "rendered",
        }),
        _ => None,
    };
    BlobProps {
        blob_id: blob_id.to_string(),
        name: filename.to_string(),
        blame_url: matches!(content, BlobContent::Text(_)).then(|| blame_url(path, head)),
        content,
        alt_view,
        source_url,
        lines: None,
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
/// `render` is `Some` when the route asked for a blob's rendered form, and
/// carries where a markdown document's links resolve from. A requested SVG
/// joins the images rather than the markdown, and for the reason they are
/// exempt: it becomes one `<img>` too, so the caps have nothing to say about it
/// and an SVG too large to read as source still draws. Markdown rendering sits
/// *after* the caps instead: comrak's output is a DOM the browser has to lay
/// out like any other, so an over-cap markdown file is refused for the same
/// reason an over-cap source file is.
fn blob_content(filename: &str, data: &[u8], render: Option<&LinkBase>) -> BlobContent {
    if let Some(format) = image_format(filename, data) {
        return BlobContent::Image { format };
    }
    if render.is_some() && is_svg(filename) {
        return BlobContent::Image {
            format: ImageFormat::Svg,
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
    if let Some(base) = render
        && is_markdown(filename)
    {
        return BlobContent::Markdown(markdown_to_html(&text, base));
    }
    BlobContent::Text(split_lines(&text))
}

/// Split a file into the lines a view renders, one per row.
///
/// A trailing newline yields a spurious empty final element; dropping it makes
/// a file ending in '\n' render the same as one that doesn't, and leaves the
/// count matching [`count_lines`] — which is what the caps above are checked
/// against, and how git counts a file's lines.
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.into_iter().map(String::from).collect()
}

/// How many rows `data` would render as, counted without decoding it: one per
/// '\n', plus one more for a final line that isn't newline-terminated. Kept in
/// step with the split in [`blob_content`], which drops that same trailing
/// empty element.
pub(crate) fn count_lines(data: &[u8]) -> usize {
    let newlines = data.iter().filter(|&&b| b == b'\n').count();
    if data.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}
