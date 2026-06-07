use crate::{
    cache::CachingRepo,
    render::{RefRow, fetch_branch_rows, fetch_tag_rows},
};
use git_async::reference::RefName;
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_all(repo: &CachingRepo) -> RefsAllTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();

    let mut branch_names: Vec<String> = Vec::new();
    let mut tag_names: Vec<String> = Vec::new();

    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b).into_owned(),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            branch_names.push(short.to_string());
        } else if let Some(short) = label.strip_prefix("tags/") {
            tag_names.push(short.to_string());
        }
    }

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
