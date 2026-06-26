use git_async::object::ObjectId;
use yew::prelude::*;

/// The view inputs for a blob: its id and its lines (1-based line numbers are
/// the row index). Doubles as the component's props and the test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct BlobProps {
    pub blob_id: String,
    pub lines: Vec<String>,
}

fn build_blob_props(blob_id: ObjectId, data: &[u8]) -> BlobProps {
    let text = String::from_utf8_lossy(data);
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline yields a spurious empty final element; drop it so a
    // file ending in '\n' renders the same as one that doesn't.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    BlobProps {
        blob_id: blob_id.to_string(),
        lines: lines.into_iter().map(String::from).collect(),
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
    let BlobProps { blob_id, lines } = props;

    html! {
        <>
            <div class="blob-info">
                { "blob: " }{ blob_id }
            </div>
            <table class="blob-table">
                <tbody>
                    { for lines.iter().enumerate().map(|(i, line)| blob_row(i + 1, line)) }
                </tbody>
            </table>
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

pub(crate) fn render_blob(
    blob_id: ObjectId,
    data: &[u8],
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let props = build_blob_props(blob_id, data);
    // Incremental migration: mount a self-contained Yew app at #output. The
    // handle is leaked because the next navigation clears #output directly.
    let handle = yew::Renderer::<BlobView>::with_root_and_props(output.clone(), props).render();
    std::mem::forget(handle);
    Ok(())
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
}
