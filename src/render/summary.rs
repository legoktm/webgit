use crate::{
    cache::CachingRepo,
    render::{
        CommitRow, RefRow, collect_refs, decoration_map, fetch_ref_rows, head_branch_name,
        render_template, walk_commits,
    },
};
use git_async::object::Commit;
use serde::Serialize;
use tera::Tera;

async fn build_summary(
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
) -> SummaryTemplate {
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
        walk_commits(head_commit, repo, 0, 10, &decorations),
    );

    SummaryTemplate {
        branches,
        more_branches,
        tags,
        more_tags,
        commits,
        clone_url: clone_url.to_string(),
    }
}

#[derive(Serialize)]
struct SummaryTemplate {
    branches: Vec<RefRow>,
    more_branches: bool,
    tags: Vec<RefRow>,
    more_tags: bool,
    commits: Vec<CommitRow>,
    clone_url: String,
}

pub(crate) async fn render_summary(
    tera: &Tera,
    head_commit: &Commit,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_summary(head_commit, repo, clone_url).await;
    render_template(tera, "summary.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{fixtures, init_tera, render_to_string};

    #[test]
    fn test_summary_html() {
        let template = SummaryTemplate {
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
        };
        insta::assert_snapshot!(render_to_string(&init_tera(), "summary.html", &template).unwrap());
    }
}
