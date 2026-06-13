use crate::{
    cache::CachingRepo,
    render::{CommitRow, decoration_map, render_template, walk_commits},
    route::log_url,
};
use git_async::object::Commit;
use serde::Serialize;
use tera::Tera;

const PAGE_SIZE: usize = 50;

async fn build_log(
    head_commit: &Commit,
    repo: &CachingRepo,
    offset: usize,
    head: Option<&str>,
) -> LogTemplate {
    let decorations = decoration_map(repo).await;
    let (commits, has_next) =
        walk_commits(head_commit, repo, offset, PAGE_SIZE, &decorations).await;
    LogTemplate {
        commits,
        prev_url: (offset > 0).then(|| log_url(offset.saturating_sub(PAGE_SIZE), head)),
        next_url: has_next.then(|| log_url(offset + PAGE_SIZE, head)),
    }
}

#[derive(Serialize)]
struct LogTemplate {
    commits: Vec<CommitRow>,
    prev_url: Option<String>,
    next_url: Option<String>,
}

pub(crate) async fn render_log(
    tera: &Tera,
    head_commit: &Commit,
    repo: &CachingRepo,
    offset: usize,
    head: Option<&str>,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_log(head_commit, repo, offset, head).await;
    render_template(tera, "log.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{fixtures, init_tera, render_to_string};

    #[test]
    fn test_log_html_with_pagination() {
        let template = LogTemplate {
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
            prev_url: Some(log_url(0, Some("main"))),
            next_url: Some(log_url(100, Some("main"))),
        };
        insta::assert_snapshot!(render_to_string(&init_tera(), "log.html", &template).unwrap());
    }

    #[test]
    fn test_log_html_first_page_no_nav() {
        let template = LogTemplate {
            commits: vec![fixtures::commit_row(
                "0123abcd",
                "Initial commit",
                "Kunal Mehta",
                fixtures::relative_age(60),
            )],
            prev_url: None,
            next_url: None,
        };
        insta::assert_snapshot!(render_to_string(&init_tera(), "log.html", &template).unwrap());
    }
}
