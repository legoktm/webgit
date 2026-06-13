use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_refs, fetch_ref_rows, render_template},
};
use serde::Serialize;
use tera::Tera;

async fn build_refs_all(repo: &CachingRepo) -> RefsAllTemplate {
    let (branch_refs, tag_refs) = collect_refs(repo).await;

    let (mut branches, mut tags) = futures::join!(
        fetch_ref_rows(&branch_refs, repo),
        fetch_ref_rows(&tag_refs, repo),
    );
    branches.sort_by_key(|b| b.age.secs());
    tags.sort_by_key(|t| t.age.secs());

    RefsAllTemplate {
        branches,
        tags,
        // This page lists every ref, so there are never "more" links.
        more_branches: false,
        more_tags: false,
    }
}

#[derive(Serialize)]
struct RefsAllTemplate {
    branches: Vec<RefRow>,
    tags: Vec<RefRow>,
    more_branches: bool,
    more_tags: bool,
}

pub(crate) async fn render_refs_all(
    tera: &Tera,
    repo: &CachingRepo,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_refs_all(repo).await;
    render_template(tera, "refs_all.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{fixtures, init_tera, render_to_string};

    #[test]
    fn test_refs_all_html() {
        let template = RefsAllTemplate {
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
        };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "refs_all.html", &template).unwrap()
        );
    }
}
