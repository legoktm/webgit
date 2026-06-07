use crate::{
    cache::CachingRepo,
    render::{RefRow, ref_row},
};

use git_async::reference::RefName;
use serde::Serialize;
use tera::{Context, Tera};

async fn build_refs_heads(repo: &CachingRepo) -> RefsHeadsTemplate {
    let ref_names = repo.ref_names().await.unwrap_or_default();
    let mut branch_names = vec![];

    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b).into_owned(),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            branch_names.push(short.to_string());
        }
    }

    // --- Fetch commit data only for the selected refs ---
    let mut branches: Vec<RefRow> = Vec::new();
    for short in &branch_names {
        let rn = RefName::Ref(format!("heads/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await else {
            continue;
        };
        branches.push(ref_row(short.clone(), &commit));
    }

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
) -> Result<(), String> {
    let template = build_refs_heads(repo).await;
    let ctx = Context::from_serialize(&template).map_err(|e| format!("{e}"))?;
    let html =
        tera.render("refs_heads.html", &ctx).map_err(|e| format!("Template error: {e}"))?;
    output.set_inner_html(&html);
    Ok(())
}
