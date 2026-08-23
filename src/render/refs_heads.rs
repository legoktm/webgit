use crate::{
    cache::CachingRepo,
    render::{RefRow, branches_section, collect_refs, fetch_ref_rows},
};
use gib_mailmap::Mailmap;
use yew::prelude::*;

pub(crate) async fn build_refs_heads(repo: &CachingRepo, mailmap: &Mailmap) -> RefsHeadsProps {
    let (branches, _) = collect_refs(repo).await;
    let branches = fetch_ref_rows(&branches, repo, mailmap).await;
    RefsHeadsProps {
        branches,
        // This page lists every branch, so there is never a "more" link.
        more_branches: false,
    }
}

/// The view inputs for the branch list. Doubles as the component's props and
/// the unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct RefsHeadsProps {
    branches: Vec<RefRow>,
    more_branches: bool,
}

/// The Yew component used to mount the branch list into the DOM. The markup
/// lives in the plain `refs_heads_view` function below so it can be exercised
/// without a renderer.
#[function_component(RefsHeadsView)]
pub(crate) fn refs_heads_view_component(props: &RefsHeadsProps) -> Html {
    refs_heads_view(props)
}

pub(crate) fn refs_heads_view(props: &RefsHeadsProps) -> Html {
    let RefsHeadsProps {
        branches,
        more_branches,
    } = props;
    branches_section(branches, *more_branches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fixtures;

    /// Render `RefsHeadsView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: RefsHeadsProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<RefsHeadsView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_refs_heads_html() {
        insta::assert_snapshot!(render(RefsHeadsProps {
            branches: vec![
                fixtures::ref_row(
                    "main",
                    "Fix non-annotated tags",
                    "Kunal Mehta",
                    fixtures::relative_age(3600),
                ),
                fixtures::ref_row(
                    "develop",
                    "WIP: new parser",
                    "Someone Else",
                    fixtures::date_age("2001-01-05"),
                ),
            ],
            more_branches: false,
        }));
    }
}
