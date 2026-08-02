use crate::{
    cache::CachingRepo,
    render::{CommitRow, apply_decorations, commits_table, decoration_map, walk_commits_streamed},
    route::log_url,
};
use git_async::object::Commit;
use std::cell::RefCell;
use std::collections::BTreeMap;
use yew::prelude::*;

const PAGE_SIZE: usize = 50;

/// Build the log page, calling `on_partial` with progressively longer prefixes
/// of the page as commits stream in. The returned value is the complete page;
/// it's the only one carrying the pagination links (see below).
pub(crate) async fn build_log(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: &str,
    offset: usize,
    head: Option<&str>,
    on_partial: impl Fn(LogProps),
) -> LogProps {
    let path_filter = (!path.is_empty()).then_some(path);
    let prev_url = (offset > 0).then(|| log_url(path, offset.saturating_sub(PAGE_SIZE), head));

    // Walk and decoration scan run concurrently, as in `build_summary`: peeling
    // every tag is fetch-bound on a cold cache and awaiting it first would stall
    // every pagination click before a single row could render. The walk streams
    // label-less rows into this cell; each partial folds in whatever labels have
    // landed, so the chips appear as soon as the scan resolves (and the returned
    // page always carries them).
    let decorations = RefCell::new(BTreeMap::new());
    let decorations = &decorations;
    let scan = async move {
        let map = decoration_map(repo).await;
        *decorations.borrow_mut() = map;
    };
    let walk = walk_commits_streamed(head_commit, repo, path_filter, offset, PAGE_SIZE, |rows| {
        let mut commits = rows.to_vec();
        apply_decorations(&mut commits, &decorations.borrow());
        // Hold the nav off the partials: `next` isn't known until the walk
        // finishes, and showing "newer/older" mid-load would be misleading.
        on_partial(LogProps {
            commits,
            prev_url: prev_url.clone(),
            next_url: None,
        });
    });
    let (_, (mut commits, has_next)) = futures::join!(scan, walk);

    apply_decorations(&mut commits, &decorations.borrow());
    LogProps {
        commits,
        prev_url,
        next_url: has_next.then(|| log_url(path, offset + PAGE_SIZE, head)),
    }
}

/// The view inputs for a page of the log: the commit rows plus the optional
/// newer/older navigation targets. Doubles as the component's props and the
/// unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct LogProps {
    commits: Vec<CommitRow>,
    prev_url: Option<String>,
    next_url: Option<String>,
}

/// The Yew component used to mount the log view into the DOM. The markup lives
/// in the plain `log_view` function below so it can be exercised without a
/// renderer.
#[function_component(LogView)]
pub(crate) fn log_view_component(props: &LogProps) -> Html {
    log_view(props)
}

pub(crate) fn log_view(props: &LogProps) -> Html {
    let LogProps {
        commits,
        prev_url,
        next_url,
    } = props;

    html! {
        <>
            { commits_table(commits) }
            if prev_url.is_some() || next_url.is_some() {
                <div class="log-nav">
                    if let Some(prev) = prev_url {
                        <a href={prev.clone()}>{ "\u{2190} newer" }</a>
                    }
                    if let Some(next) = next_url {
                        <a href={next.clone()}>{ "older \u{2192}" }</a>
                    }
                </div>
            }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fixtures;

    /// Render `LogView` to a static HTML string via SSR, breaking adjacent tags
    /// onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: LogProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<LogView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_log_html_with_pagination() {
        insta::assert_snapshot!(render(LogProps {
            commits: vec![
                fixtures::decorated_commit_row(
                    "0123abcd",
                    "Fix non-annotated tags",
                    "Kunal Mehta",
                    fixtures::relative_age(3600),
                    &["main"],
                    &["v1.0.0"],
                ),
                fixtures::commit_row(
                    "89abcdef",
                    "Add README",
                    "Kunal Mehta",
                    fixtures::relative_age(86400 * 3),
                ),
            ],
            prev_url: Some(log_url("", 0, Some("main"))),
            next_url: Some(log_url("", 100, Some("main"))),
        }));
    }

    #[test]
    fn test_log_html_first_page_no_nav() {
        insta::assert_snapshot!(render(LogProps {
            commits: vec![fixtures::commit_row(
                "0123abcd",
                "Initial commit",
                "Kunal Mehta",
                fixtures::relative_age(60),
            )],
            prev_url: None,
            next_url: None,
        }));
    }
}
