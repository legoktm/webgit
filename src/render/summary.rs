use crate::{
    cache::CachingRepo,
    render::{CommitRow, RefRow, collect_ref_names, fetch_branch_rows, fetch_tag_rows, head_branch_name, render_template, walk_commits},
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

    let (all_branch_names, mut tag_names) = collect_ref_names(repo).await;

    // HEAD branch goes first; remaining are alpha-sorted. Total cap: 10.
    let mut primary: Option<String> = None;
    let mut other_branches: Vec<String> = Vec::new();
    for short in all_branch_names {
        if head_branch.as_deref() == Some(&short) {
            primary = Some(short);
        } else {
            other_branches.push(short);
        }
    }

    other_branches.sort();
    let others_limit = if primary.is_some() { 9 } else { 10 };
    let more_branches = other_branches.len() > others_limit;
    other_branches.truncate(others_limit);
    let branch_names: Vec<String> = primary.into_iter().chain(other_branches).collect();

    let more_tags = tag_names.len() > 10;
    // Tags: reverse alpha, cap at 10.
    tag_names.sort_by(|a, b| b.cmp(a));
    tag_names.truncate(10);

    // --- Fetch branch commits, tag commits, and recent HEAD commits concurrently ---
    let (branches, tags, (commits, _)) = futures::join!(
        fetch_branch_rows(&branch_names, repo),
        fetch_tag_rows(&tag_names, repo),
        walk_commits(head_commit, repo, 0, 10),
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
                fixtures::ref_row("main", "Fix non-annotated tags", "Kunal Mehta", 3600),
                fixtures::ref_row("develop", "WIP: new parser", "Someone Else", 86400 * 30),
            ],
            more_branches: true,
            tags: vec![fixtures::ref_row(
                "v1.0.0",
                "Release 1.0.0",
                "Kunal Mehta",
                86400 * 400,
            )],
            more_tags: true,
            commits: vec![
                fixtures::commit_row("0123abcd", "Fix non-annotated tags", "Kunal Mehta", 3600),
                fixtures::commit_row("89abcdef", "Add README", "Kunal Mehta", 86400 * 3),
            ],
            clone_url: "https://example.org/repo.git".to_string(),
        };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "summary.html", &template).unwrap()
        );
    }
}
