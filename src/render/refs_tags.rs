use crate::{
    cache::CachingRepo,
    render::{RefRow, fetch_tag_rows},
};
use git_async::reference::RefName;
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_tags(repo: &CachingRepo) -> RefsTagsTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();
    let tag_names: Vec<String> = ref_names
        .iter()
        .filter_map(|r| match r {
            RefName::Ref(b) => String::from_utf8_lossy(b)
                .strip_prefix("tags/")
                .map(|s| s.to_string()),
            RefName::Head => None,
        })
        .collect();

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
