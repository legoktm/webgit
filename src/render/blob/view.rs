//! The blob view's markup, and the two browser effects around it: minting an
//! object URL over the bytes, and scrolling a line selection into view.

use super::{BlobContent, BlobProps, MAX_BLOB_BYTES, MAX_BLOB_LINES};
use crate::render::markdown::MarkdownFrame;
use crate::render::use_object_url;
use crate::route::LineRange;
use wasm_bindgen::JsCast;
use yew::prelude::*;

/// The Yew component used to mount the blob view into the DOM.
///
/// The one thing it does beyond calling `blob_view` is mint the object URL over
/// the blob's bytes, which is a side effect and so can't live in the markup.
/// Passing the URL in keeps `blob_view` a plain function of its inputs, which
/// is what lets the tests render it without a DOM.
#[function_component(BlobView)]
pub(crate) fn blob_view_component(props: &BlobProps) -> Html {
    let url = use_object_url(props.content.mime(), &props.data);
    use_selection_scroll(props.lines.map(|lines| lines.start));
    blob_view(props, &url)
}

/// Bring the selected lines into view once they are on screen.
///
/// The browser would do this itself for a real fragment, but a line anchor is a
/// suffix inside the routing fragment rather than a fragment of its own, so no
/// element's id ever matches `location.hash` and native navigation has nothing
/// to act on. The rows also arrive after the route resolves, which is late
/// enough that even a matching id would have been missed.
///
/// Only the range's first line is scrolled to, and only when *it* changes —
/// which is also why the effect keys on the start rather than on the whole
/// range. The start is the line the reader asked for, and a range taller than
/// the viewport should be positioned by its top rather than centred on nothing
/// in particular; keying on it means extending a selection downwards leaves the
/// page where it is, since a shift-click grows the range without moving the end
/// the reader anchored it to.
#[hook]
fn use_selection_scroll(start: Option<usize>) {
    use_effect_with(start, |start| {
        if let Some(start) = *start
            && let Some(document) = web_sys::window().and_then(|window| window.document())
            && let Some(target) = document.get_element_by_id(&format!("n{start}"))
        {
            target.scroll_into_view();
        }
        || ()
    });
}

/// The click handler shared by every line number in the gutter.
///
/// One callback for the whole table rather than one per row: a blob may run to
/// [`MAX_BLOB_LINES`] rows, and a closure per row would allocate 20 000 of them
/// to serve the one click that ever fires. The line number comes back off the
/// clicked element's `data-n` instead of being captured.
///
/// A plain click is left to the browser — the `href` already names the right
/// URL. Only a shift-click is intercepted, to extend the current selection into
/// a range the way every other code viewer does; with nothing selected yet it
/// falls through to selecting the clicked line alone.
fn line_click_handler(source_url: &str, lines: Option<LineRange>) -> Callback<MouseEvent> {
    let source_url = source_url.to_string();
    Callback::from(move |event: MouseEvent| {
        if !event.shift_key() {
            return;
        }
        let Some(n) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.get_attribute("data-n"))
            .and_then(|n| n.parse::<usize>().ok())
        else {
            return;
        };
        // Extend from the anchor the reader last set, not from whichever end of
        // the range is nearer: shift-clicking twice should be able to shrink a
        // range as well as grow it.
        let range = match lines {
            Some(lines) => LineRange::spanning(lines.start, n),
            None => LineRange::single(n),
        };
        event.prevent_default();
        if let Some(window) = web_sys::window() {
            let _ = window
                .location()
                .set_hash(&format!("{source_url}{}", range.anchor()));
        }
    })
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
        blame_url,
        source_url,
        lines: selected,
        data: _,
    } = props;
    let on_line_click = line_click_handler(source_url, *selected);

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
                if let Some(blame) = blame_url {
                    { " · " }
                    <a class="blame-link" href={blame.clone()}>{ "blame" }</a>
                }
            </div>
            { match content {
                BlobContent::Text(lines) => html! {
                    <table class="blob-table">
                        <tbody>
                            { for lines.iter().enumerate().map(|(i, line)| {
                                let n = i + 1;
                                blob_row(n, line, BlobRowLink {
                                    selected: selected.is_some_and(|s| s.contains(n)),
                                    source_url,
                                    on_click: &on_line_click,
                                })
                            }) }
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

/// What a row needs to link its own line number, beyond the number itself.
/// Grouped so [`blob_row`] keeps one parameter per idea rather than a row of
/// positional arguments a caller can transpose.
struct BlobRowLink<'a> {
    /// Whether this line falls inside the selected range.
    selected: bool,
    /// The blob's source-view URL, which the line anchor is appended to.
    source_url: &'a str,
    /// The gutter's shared shift-click handler.
    on_click: &'a Callback<MouseEvent>,
}

/// One source line: its number in the gutter, linking to itself, and its text.
///
/// The link is the blob's whole URL plus a `#n…` suffix, not a bare `#n5`: the
/// app routes on the fragment, so a bare one would parse as no known route and
/// drop the reader on the summary page. `data-n` is what the shared click
/// handler reads the line number back out of.
fn blob_row(n: usize, line: &str, link: BlobRowLink<'_>) -> Html {
    let BlobRowLink {
        selected,
        source_url,
        on_click,
    } = link;
    let row_id = format!("n{n}");
    let href = format!("{source_url}{}", LineRange::single(n).anchor());
    html! {
        <tr id={row_id} class={classes!(selected.then_some("hl"))}>
            <td class="lno">
                <a href={href} data-n={n.to_string()} onclick={on_click.clone()}>{ n }</a>
            </td>
            <td class="code">{ line }</td>
        </tr>
    }
}
