use crate::{
    cache::CachingRepo,
    render::{RefRow, branches_section, collect_refs, fetch_ref_rows, tags_section},
};
use yew::prelude::*;

pub(crate) async fn build_refs_all(repo: &CachingRepo) -> RefsAllProps {
    let (branch_refs, tag_refs) = collect_refs(repo).await;

    let (mut branches, mut tags) = futures::join!(
        fetch_ref_rows(&branch_refs, repo),
        fetch_ref_rows(&tag_refs, repo),
    );
    branches.sort_by_key(|b| b.age.secs());
    tags.sort_by_key(|t| t.age.secs());

    RefsAllProps {
        branches,
        tags,
        // This page lists every ref, so there are never "more" links.
        more_branches: false,
        more_tags: false,
    }
}

/// The view inputs for the "all refs" page: the branch and tag rows plus
/// whether each list is truncated. Doubles as the component's props and the
/// unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct RefsAllProps {
    branches: Vec<RefRow>,
    tags: Vec<RefRow>,
    more_branches: bool,
    more_tags: bool,
}

/// The Yew component used to mount the all-refs view into the DOM. The markup
/// lives in the plain `refs_all_view` function below so it can be exercised
/// without a renderer.
#[function_component(RefsAllView)]
pub(crate) fn refs_all_view_component(props: &RefsAllProps) -> Html {
    refs_all_view(props)
}

pub(crate) fn refs_all_view(props: &RefsAllProps) -> Html {
    let RefsAllProps {
        branches,
        tags,
        more_branches,
        more_tags,
    } = props;

    html! {
        <>
            { branches_section(branches, *more_branches) }
            { tags_section(tags, *more_tags) }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fixtures;

    /// Render `RefsAllView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: RefsAllProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<RefsAllView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_refs_all_html() {
        insta::assert_snapshot!(render(RefsAllProps {
            branches: vec![fixtures::ref_row(
                "main",
                "Fix non-annotated tags",
                "Kunal Mehta",
                fixtures::relative_age(3600),
            )],
            tags: vec![fixtures::ref_row(
                "v1.0.0",
                "Release 1.0.0",
                "Kunal Mehta",
                fixtures::date_age("2000-03-15"),
            )],
            more_branches: false,
            more_tags: false,
        }));
    }
}
