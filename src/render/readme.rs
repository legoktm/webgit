use crate::cache::CachingRepo;
use git_async::object::{Tree, TreeEntryType};
use yew::prelude::*;

/// The README file names looked for in the root of the HEAD tree, in the order
/// they're preferred; first one wins.
const README_NAMES: [&str; 3] = ["README.md", "README", "README.txt"];

/// The view inputs for the readme page: the README's text, or `None` when the
/// repository's HEAD tree has none of [`README_NAMES`] at its root.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct ReadmeProps {
    pub text: Option<String>,
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
        return ReadmeProps {
            text: Some(String::from_utf8_lossy(blob.data()).into_owned()),
        };
    }
    ReadmeProps { text: None }
}

/// The Yew component used to mount the readme view into the DOM. The markup
/// lives in the plain `readme_view` function below so it can be unit-tested
/// without a renderer.
#[function_component(ReadmeView)]
pub(crate) fn readme_view_component(props: &ReadmeProps) -> Html {
    readme_view(props)
}

/// The README rendered as plain text — no markup interpretation, and (unlike
/// the blob view) no line numbers or blob id, since this is meant to read as
/// prose rather than as source.
pub(crate) fn readme_view(props: &ReadmeProps) -> Html {
    match &props.text {
        Some(text) => html! { <pre class="readme">{ text.clone() }</pre> },
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
    fn render(props: ReadmeProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<ReadmeView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn readme_html() {
        insta::assert_snapshot!(render(ReadmeProps {
            text: Some("# webgit\n\nA client-side Git viewer.\n".to_string()),
        }));
    }

    #[test]
    fn readme_html_escapes_markup() {
        // The text is rendered verbatim, so any markup in it must be escaped.
        insta::assert_snapshot!(render(ReadmeProps {
            text: Some("<script>alert(1)</script> & <b>bold</b>\n".to_string()),
        }));
    }

    #[test]
    fn readme_html_missing() {
        insta::assert_snapshot!(render(ReadmeProps { text: None }));
    }
}
