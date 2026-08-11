use crate::cache::CachingRepo;
use crate::render::markdown::{LinkBase, MarkdownFrame, markdown_to_html};
use gib::object::{Tree, TreeEntryType};
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
                ReadmeContent::Markdown(markdown_to_html(&text, &link_base()))
            } else {
                ReadmeContent::Plain(text)
            }),
        };
    }
    ReadmeProps { content: None }
}

/// Where the README's own links resolve from: the repository root, and back to
/// this route for a bare fragment.
fn link_base() -> LinkBase {
    LinkBase {
        dir: String::new(),
        self_url: "#!/readme".to_string(),
    }
}

/// The Yew component used to mount the readme view into the DOM.
#[function_component(ReadmeView)]
pub(crate) fn readme_view(props: &ReadmeProps) -> Html {
    match &props.content {
        Some(ReadmeContent::Markdown(html)) => {
            html! { <MarkdownFrame html={html.clone()} title="README" /> }
        }
        Some(ReadmeContent::Plain(text)) => html! { <pre class="readme">{ text.clone() }</pre> },
        None => html! { <p class="msg">{ "No README found." }</p> },
    }
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
                 | a | b |\n|---|---|\n| 1 | 2 |\n",
                &link_base(),
            ))),
        }));
    }
}
