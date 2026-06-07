use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_ref_names, fetch_branch_rows, fetch_tag_rows},
};
use serde::Serialize;
use tera::{Context, Tera};

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
    let ctx = Context::from_serialize(&template)?;
    let html = tera.render("refs_all.html", &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}
