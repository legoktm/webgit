use crate::{
    cache::CachingRepo,
    render::{RefRow, ref_row},
};
use git_async::reference::RefName;
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_tags(repo: &CachingRepo) -> RefsTagsTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();
    let mut tag_names: Vec<String> = Vec::new();

    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b).into_owned(),
        };
        if let Some(short) = label.strip_prefix("tags/") {
            tag_names.push(short.to_string());
        }
    }

    // --- Fetch commit data only for the selected refs ---
    let mut tags: Vec<RefRow> = Vec::new();
    for short in &tag_names {
        let rn = RefName::Ref(format!("tags/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await else {
            continue;
        };
        tags.push(ref_row(short.clone(), &commit));
    }
    tags.sort_by_key(|tag| tag.age);

    RefsTagsTemplate { tags }
}

#[derive(Serialize)]
struct RefsTagsTemplate {
    tags: Vec<RefRow>,
}

pub(crate) async fn render_refs_tags(tera: &Tera, repo: &CachingRepo, output: &web_sys::Element) {
    let template = build_refs_tags(repo).await;
    let ctx = Context::from_serialize(&template).unwrap();
    match tera.render("refs_tags.html", &ctx) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}
