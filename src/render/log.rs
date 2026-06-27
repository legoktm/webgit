use crate::{
    cache::CachingRepo,
    render::{CommitRow, commits_table, decoration_map, walk_commits},
    route::log_url,
};
use git_async::object::Commit;
use yew::prelude::*;

const PAGE_SIZE: usize = 50;

async fn build_log(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: &str,
    offset: usize,
    head: Option<&str>,
) -> LogProps {
    let decorations = decoration_map(repo).await;
    let path_filter = (!path.is_empty()).then_some(path);
    let (commits, has_next) = walk_commits(
        head_commit,
        repo,
        path_filter,
        offset,
        PAGE_SIZE,
        &decorations,
    )
    .await;
    LogProps {
        commits,
        prev_url: (offset > 0).then(|| log_url(path, offset.saturating_sub(PAGE_SIZE), head)),
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

pub(crate) async fn render_log(
    head_commit: &Commit,
    repo: &CachingRepo,
    path: &str,
    offset: usize,
    head: Option<&str>,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let props = build_log(head_commit, repo, path, offset, head).await;
    // Incremental migration: mount a self-contained Yew app at #output. The
    // handle is leaked because the next navigation clears #output directly.
    let handle = yew::Renderer::<LogView>::with_root_and_props(output.clone(), props).render();
    std::mem::forget(handle);
    Ok(())
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
