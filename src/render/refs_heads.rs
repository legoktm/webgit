use crate::{
    cache::CachingRepo,
    render::{RefRow, fetch_branch_rows},
};
use git_async::reference::RefName;
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_heads(repo: &CachingRepo) -> RefsHeadsTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();
    let branch_names: Vec<String> = ref_names
        .iter()
        .filter_map(|r| match r {
            RefName::Ref(b) => String::from_utf8_lossy(b)
                .strip_prefix("heads/")
                .map(|s| s.to_string()),
            RefName::Head => None,
        })
        .collect();

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
    let ctx = Context::from_serialize(&template)?;
    let html = tera.render("refs_heads.html", &ctx)?;
    output.set_inner_html(&html);
    Ok(())
}
