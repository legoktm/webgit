use crate::{
    cache::CachingRepo,
    render::{
        CommitRow, RefRow, apply_decorations, branches_section, collect_refs, commits_table,
        decoration_map, fetch_ref_rows_each, head_branch_name, loading_dots, recent_commits,
        tags_section,
    },
};
use git_async::object::Commit;
use std::cell::RefCell;
use std::rc::Rc;
use yew::prelude::*;

/// Build the summary, calling `on_partial` as sections fill in: first a skeleton
/// with the (name-sorted) branch/tag rows shown but their commit metadata blank,
/// then each branch/tag row backfilled as its commit resolves and the
/// recent-commit rows appended newest-first as their objects load — all
/// concurrently. The recent-commit walk goes through [`recent_commits`], which
/// reads commit objects directly rather than bulk-loading the commit-graph, so
/// this bounded preview stays as cheap as the ref rows. The returned value is
/// the complete page.
///
/// Every section fills strictly top-down even though the fetches behind it
/// complete out of order: [`fetch_ref_rows_each`] releases ref rows only in list
/// order, and [`recent_commits`] appends one row per pop of its
/// newest-first frontier. Fetch concurrency is unaffected — only the reveal is
/// sequenced, so no request waits on another.
pub(crate) async fn build_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
    on_partial: impl Fn(SummaryProps),
) -> SummaryProps {
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

    // Branch/tag rows are name-sorted, so the names (and order) are known before
    // any commit fetch: render the rows immediately and backfill message/author/
    // age into them from the top down as commits resolve. The recent-commit walk
    // has no such up-front list, so its section shows a spinner until the first
    // row arrives, then grows downward. All three run concurrently via a shared
    // accumulator.
    let acc = Rc::new(RefCell::new(SummaryProps {
        branches: branches
            .iter()
            .map(|(n, _)| RefRow::pending(n.clone()))
            .collect(),
        more_branches,
        tags: tags
            .iter()
            .map(|(n, _)| RefRow::pending(n.clone()))
            .collect(),
        more_tags,
        commits: None,
        clone_url: clone_url.to_string(),
    }));
    on_partial(acc.borrow().clone());

    let on_partial = &on_partial;
    let branches_fut = {
        let acc = acc.clone();
        async move {
            fetch_ref_rows_each(&branches, repo, |i, row| {
                acc.borrow_mut().branches[i] = row;
                on_partial(acc.borrow().clone());
            })
            .await;
        }
    };
    let tags_fut = {
        let acc = acc.clone();
        async move {
            fetch_ref_rows_each(&tags, repo, |i, row| {
                acc.borrow_mut().tags[i] = row;
                on_partial(acc.borrow().clone());
            })
            .await;
        }
    };
    let commits_fut = {
        let acc = acc.clone();
        async move {
            // Walk and decoration scan run concurrently: the (sometimes
            // fetch-bound) ref scan must not delay the first commit rows, so the
            // walk streams label-less rows and the branch/tag chips are folded in
            // once decorations resolve.
            let walk = recent_commits(head_commit, repo, 10, |rows| {
                acc.borrow_mut().commits = Some(rows.to_vec());
                on_partial(acc.borrow().clone());
            });
            let (decorations, _) = futures::join!(decoration_map(repo), walk);
            {
                let mut summary = acc.borrow_mut();
                if let Some(rows) = summary.commits.as_mut() {
                    apply_decorations(rows, &decorations);
                }
            }
            on_partial(acc.borrow().clone());
        }
    };
    futures::join!(branches_fut, tags_fut, commits_fut);

    acc.borrow().clone()
}

/// The view inputs for the summary page: clone URL, the (capped) branch and tag
/// lists with their "more" flags, and the most recent commits. Doubles as the
/// component's props and the unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct SummaryProps {
    /// Branch/tag rows are name-sorted, so the names are known up front: the rows
    /// render immediately and their commit metadata (`RefRow.meta`) is backfilled
    /// top-down as commits resolve.
    branches: Vec<RefRow>,
    more_branches: bool,
    tags: Vec<RefRow>,
    more_tags: bool,
    /// Recent commits have no up-front list, so the section shows the loading
    /// ellipsis (`None`) until the first row arrives, then streams in newest-first.
    commits: Option<Vec<CommitRow>>,
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
            { tags_section(tags, *more_tags, clone_url) }
            <h3 class="summary-heading">{ "Recent commits" }</h3>
            { match commits {
                Some(c) => commits_table(c),
                None => html! { <p class="msg">{ loading_dots() }</p> },
            } }
        </>
    }
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
            commits: Some(vec![
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
            ]),
            clone_url: "https://example.org/repo.git".to_string(),
        }));
    }

    #[test]
    fn test_summary_html_loading() {
        // The initial skeleton: branch/tag names shown immediately with blank
        // metadata columns (rows are `RefRow::pending`), and recent commits still
        // spinning, before any commit resolves.
        insta::assert_snapshot!(render(SummaryProps {
            branches: vec![
                RefRow::pending("main".to_string()),
                RefRow::pending("develop".to_string()),
            ],
            more_branches: false,
            tags: vec![RefRow::pending("v1.0.0".to_string())],
            more_tags: false,
            commits: None,
            clone_url: "https://example.org/repo.git".to_string(),
        }));
    }
}
