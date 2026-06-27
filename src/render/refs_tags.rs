use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_refs, fetch_ref_rows, tags_section},
};
use yew::prelude::*;

async fn build_refs_tags(repo: &CachingRepo) -> RefsTagsProps {
    let (_, tags) = collect_refs(repo).await;
    let mut tags = fetch_ref_rows(&tags, repo).await;
    tags.sort_by_key(|t| t.age.secs());
    RefsTagsProps {
        tags,
        // This page lists every tag, so there is never a "more" link.
        more_tags: false,
    }
}

/// The view inputs for the tag list. Doubles as the component's props and the
/// unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct RefsTagsProps {
    tags: Vec<RefRow>,
    more_tags: bool,
}

/// The Yew component used to mount the tag list into the DOM. The markup lives
/// in the plain `refs_tags_view` function below so it can be exercised without
/// a renderer.
#[function_component(RefsTagsView)]
pub(crate) fn refs_tags_view_component(props: &RefsTagsProps) -> Html {
    refs_tags_view(props)
}

pub(crate) fn refs_tags_view(props: &RefsTagsProps) -> Html {
    let RefsTagsProps { tags, more_tags } = props;
    tags_section(tags, *more_tags)
}

pub(crate) async fn render_refs_tags(
    repo: &CachingRepo,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let props = build_refs_tags(repo).await;
    // Incremental migration: mount a self-contained Yew app at #output. The
    // handle is leaked because the next navigation clears #output directly.
    let handle = yew::Renderer::<RefsTagsView>::with_root_and_props(output.clone(), props).render();
    std::mem::forget(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fixtures;

    /// Render `RefsTagsView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: RefsTagsProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<RefsTagsView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_refs_tags_html() {
        insta::assert_snapshot!(render(RefsTagsProps {
            tags: vec![
                fixtures::ref_row(
                    "v1.1.0",
                    "Release 1.1.0",
                    "Kunal Mehta",
                    fixtures::relative_age(86400),
                ),
                fixtures::ref_row(
                    "v1.0.0",
                    "Release 1.0.0",
                    "Kunal Mehta",
                    fixtures::date_age("2000-03-15"),
                ),
            ],
            more_tags: false,
        }));
    }

    #[test]
    fn test_refs_tags_html_empty() {
        insta::assert_snapshot!(render(RefsTagsProps {
            tags: vec![],
            more_tags: false,
        }));
    }
}
