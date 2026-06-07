use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_ref_names, fetch_branch_rows, render_template},
};
use serde::Serialize;
use tera::Tera;

async fn build_refs_heads(repo: &CachingRepo) -> RefsHeadsTemplate {
    let (branch_names, _) = collect_ref_names(repo).await;
    let branches = fetch_branch_rows(&branch_names, repo).await;
    RefsHeadsTemplate { branches }
}

#[derive(Serialize)]
struct RefsHeadsTemplate {
    branches: Vec<RefRow>,
}

pub(crate) async fn render_refs_heads(
    tera: &Tera,
    repo: &CachingRepo,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_refs_heads(repo).await;
    render_template(tera, "refs_heads.html", &template, output)
}
