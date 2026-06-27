use crate::{
    cache::CachingRepo,
    render::{
        CommitRow, RefRow, branches_section, collect_refs, commits_table, decoration_map,
        fetch_ref_rows, head_branch_name, tags_section, walk_commits,
    },
};
use git_async::object::Commit;
use yew::prelude::*;

async fn build_summary(head_commit: &Commit, repo: &CachingRepo, clone_url: &str) -> SummaryProps {
    let head_branch: Option<String> = head_branch_name(repo).await;

    let (all_branches, mut tags) = collect_refs(repo).await;

    // HEAD branch goes first; remaining are alpha-sorted. Total cap: 10.
    let mut primary = None;
    let mut other_branches = Vec::new();
    for branch in all_branches {
        if head_branch.as_deref() == Some(&branch.0) {
            primary = Some(branch);
        } else {
            other_branches.push(branch);
        }
    }

    other_branches.sort_by(|a, b| a.0.cmp(&b.0));
    let others_limit = if primary.is_some() { 9 } else { 10 };
    let more_branches = other_branches.len() > others_limit;
    other_branches.truncate(others_limit);
    let branches: Vec<_> = primary.into_iter().chain(other_branches).collect();

    let more_tags = tags.len() > 10;
    // Tags: reverse alpha, cap at 10.
    tags.sort_by(|a, b| b.0.cmp(&a.0));
    tags.truncate(10);

    let decorations = decoration_map(repo).await;

    // --- Fetch branch commits, tag commits, and recent HEAD commits concurrently ---
    let (branches, tags, (commits, _)) = futures::join!(
        fetch_ref_rows(&branches, repo),
        fetch_ref_rows(&tags, repo),
        walk_commits(head_commit, repo, None, 0, 10, &decorations),
    );

    SummaryProps {
        branches,
        more_branches,
        tags,
        more_tags,
        commits,
        clone_url: clone_url.to_string(),
    }
}

/// The view inputs for the summary page: clone URL, the (capped) branch and tag
/// lists with their "more" flags, and the most recent commits. Doubles as the
/// component's props and the unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct SummaryProps {
    branches: Vec<RefRow>,
    more_branches: bool,
    tags: Vec<RefRow>,
    more_tags: bool,
    commits: Vec<CommitRow>,
    clone_url: String,
}

/// The Yew component used to mount the summary view into the DOM. The markup
/// lives in the plain `summary_view` function below so it can be exercised
/// without a renderer.
#[function_component(SummaryView)]
pub(crate) fn summary_view_component(props: &SummaryProps) -> Html {
    summary_view(props)
}

pub(crate) fn summary_view(props: &SummaryProps) -> Html {
    let SummaryProps {
        branches,
        more_branches,
        tags,
        more_tags,
        commits,
        clone_url,
    } = props;

    html! {
        <>
            <h3 class="summary-heading">{ "Clone" }</h3>
            <div class="clone-url">{ format!("git clone {clone_url}") }</div>
            { branches_section(branches, *more_branches) }
            { tags_section(tags, *more_tags) }
            <h3 class="summary-heading">{ "Recent commits" }</h3>
            { commits_table(commits) }
        </>
    }
}

pub(crate) async fn render_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let props = build_summary(head_commit, repo, clone_url).await;
    // Incremental migration: mount a self-contained Yew app at #output. The
    // handle is leaked because the next navigation clears #output directly.
    let handle = yew::Renderer::<SummaryView>::with_root_and_props(output.clone(), props).render();
    std::mem::forget(handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fixtures;

    /// Render `SummaryView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: SummaryProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<SummaryView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_summary_html() {
        insta::assert_snapshot!(render(SummaryProps {
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
            more_branches: true,
            tags: vec![fixtures::ref_row(
                "v1.0.0",
                "Release 1.0.0",
                "Kunal Mehta",
                fixtures::date_age("2000-03-15"),
            )],
            more_tags: true,
            commits: vec![
                fixtures::commit_row(
                    "0123abcd",
                    "Fix non-annotated tags",
                    "Kunal Mehta",
                    fixtures::relative_age(3600),
                ),
                fixtures::commit_row(
                    "89abcdef",
                    "Add README",
                    "Kunal Mehta",
                    fixtures::relative_age(86400 * 3),
                ),
            ],
            clone_url: "https://example.org/repo.git".to_string(),
        }));
    }
}
