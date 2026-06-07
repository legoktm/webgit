use crate::cache::CachingRepo;
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::{Context, Tera};

async fn build_tag(repo: &CachingRepo, tag: String) -> Result<TagTemplate, String> {
    let ref_name = RefName::Ref(format!("tags/{tag}").into_bytes());
    let ref_ = repo
        .lookup_ref(&ref_name)
        .await
        .map_err(|e| format!("unable to find ref for {tag}: {e:?}"))?;
    let commit = repo
        .peel_ref_to_commit(&ref_)
        .await
        .map_err(|e| format!("unable to peel ref for {tag}: {e:?}"))?
        .ok_or_else(|| format!("unable to find commit for {tag}"))?;
    let tag_object_id = match ref_.target() {
        RefTarget::Direct(object_id) => object_id,
        _ => return Err(format!("ref target for {tag} is not direct")),
    };
    let tag_obj = repo
        .lookup_object(*tag_object_id)
        .await
        .map_err(|e| format!("{e:?}"))?
        .tag()
        .map_err(|e| format!("{e:?}"))?;

    Ok(TagTemplate {
        name: tag.clone(),
        date: tag_obj
            .date()
            .ok_or_else(|| format!("no date on tag {tag}"))?
            .to_string(),
        tagger_name: String::from_utf8_lossy(
            tag_obj.tagger_name().ok_or_else(|| format!("no tagger name on tag {tag}"))?,
        )
        .to_string(),
        commit: commit.id().to_string(),
        contents: String::from_utf8(tag_obj.message().to_vec())
            .map_err(|e| format!("{e}"))?,
    })
}

#[derive(Serialize)]
struct TagTemplate {
    name: String,
    date: String,
    tagger_name: String,
    commit: String,
    contents: String,
}

pub(crate) async fn render_tag(
    tera: &Tera,
    repo: &CachingRepo,
    tag: String,
    output: &web_sys::Element,
) -> Result<(), String> {
    let template = build_tag(repo, tag).await?;
    let ctx = Context::from_serialize(&template).map_err(|e| format!("{e}"))?;
    let html = tera.render("tag.html", &ctx).map_err(|e| format!("Template error: {e}"))?;
    output.set_inner_html(&html);
    Ok(())
}
