//! The blob view's markup, and the browser effect around it: minting an object
//! URL over the bytes. The line selection lives in [`crate::render::lines`].

use super::{BlobContent, BlobProps, MAX_BLOB_BYTES, MAX_BLOB_LINES};
use crate::render::markdown::MarkdownFrame;
use crate::render::{anchored, line_click_handler, use_object_url, use_selection_scroll};
use crate::route::LineRange;
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
                    <a class="blame-link" href={anchored(blame, *selected)}>{ "blame" }</a>
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
