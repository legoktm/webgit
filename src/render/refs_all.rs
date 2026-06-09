use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_ref_names, fetch_branch_rows, fetch_tag_rows, render_template},
};
use serde::Serialize;
use tera::Tera;

async fn build_refs_all(repo: &CachingRepo) -> RefsAllTemplate {
    let (branch_names, tag_names) = collect_ref_names(repo).await;

    let (mut branches, mut tags) = futures::join!(
        fetch_branch_rows(&branch_names, repo),
        fetch_tag_rows(&tag_names, repo),
    );
    branches.sort_by_key(|b| b.age);
    tags.sort_by_key(|t| t.age);

    RefsAllTemplate { branches, tags }
}

#[derive(Serialize)]
struct RefsAllTemplate {
    branches: Vec<RefRow>,
    tags: Vec<RefRow>,
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
                3600,
            )],
            tags: vec![fixtures::ref_row(
                "v1.0.0",
                "Release 1.0.0",
                "Kunal Mehta",
                86400 * 400,
            )],
        };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "refs_all.html", &template).unwrap()
        );
    }
}
