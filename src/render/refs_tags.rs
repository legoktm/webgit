use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_ref_names, fetch_tag_rows},
};
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_tags(repo: &CachingRepo) -> RefsTagsTemplate {
    let (_, tag_names) = collect_ref_names(repo).await;
    let mut tags = fetch_tag_rows(&tag_names, repo).await;
    tags.sort_by_key(|t| t.age);
    RefsTagsTemplate { tags }
}

#[derive(Serialize)]
struct RefsTagsTemplate {
    tags: Vec<RefRow>,
}

pub(crate) async fn render_refs_tags(
    tera: &Tera,
    repo: &CachingRepo,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_refs_tags(repo).await;
    let ctx = Context::from_serialize(&template)?;
    let html = tera.render("refs_tags.html", &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}
