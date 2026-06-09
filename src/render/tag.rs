use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::render_template;
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::Tera;

async fn build_tag(repo: &CachingRepo, tag: String) -> anyhow::Result<TagTemplate> {
    let ref_name = RefName::Ref(format!("tags/{tag}").into_bytes());
    let ref_ = repo
        .lookup_ref(&ref_name).await
        .context(format!("lookup ref for {tag}"))?;
    let commit = repo
        .peel_ref_to_commit(&ref_).await
        .context(format!("peel ref for {tag}"))?
        .ok_or_else(|| anyhow::anyhow!("no commit for {tag}"))?;
    let tag_object_id = match ref_.target() {
        RefTarget::Direct(object_id) => object_id,
        _ => anyhow::bail!("ref target for {tag} is not direct"),
    };
    let object = repo
        .lookup_object(*tag_object_id).await
        .context(format!("lookup tag object {tag}"))?;

    // Annotated (and signed) tags point at a tag object that carries its own
    // tagger and message. Lightweight tags point straight at the commit and
    // have no metadata of their own, so fall back to the commit's details.
    match object.tag() {
        Ok(tag_obj) => Ok(TagTemplate {
            name: tag.clone(),
            date: tag_obj
                .date()
                .ok_or_else(|| anyhow::anyhow!("no date on tag {tag}"))?
                .to_string(),
            tagger_name: Some(
                String::from_utf8_lossy(
                    tag_obj.tagger_name().ok_or_else(|| anyhow::anyhow!("no tagger on {tag}"))?,
                )
                .into_owned(),
            ),
            commit: commit.id().to_string(),
            contents: Some(String::from_utf8(tag_obj.message().to_vec())?),
        }),
        Err(_) => Ok(TagTemplate {
            name: tag,
            date: commit.author_date().to_string(),
            tagger_name: None,
            commit: commit.id().to_string(),
            contents: None,
        }),
    }
}

#[derive(Serialize)]
struct TagTemplate {
    name: String,
    date: String,
    tagger_name: Option<String>,
    commit: String,
    contents: Option<String>,
}

pub(crate) async fn render_tag(
    tera: &Tera,
    repo: &CachingRepo,
    tag: String,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_tag(repo, tag).await?;
    render_template(tera, "tag.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{init_tera, render_to_string};

    #[test]
    fn test_tag_html_annotated() {
        let template = TagTemplate {
            name: "v1.0.0".to_string(),
            date: "2026-01-15 12:34:56 +00:00".to_string(),
            tagger_name: Some("Kunal Mehta".to_string()),
            commit: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            contents: Some("Release 1.0.0\n\nSigned-off-by: Kunal Mehta".to_string()),
        };
        insta::assert_snapshot!(render_to_string(&init_tera(), "tag.html", &template).unwrap());
    }

    #[test]
    fn test_tag_html_lightweight() {
        let template = TagTemplate {
            name: "v0.9.0".to_string(),
            date: "2025-11-02 08:00:00 +00:00".to_string(),
            tagger_name: None,
            commit: "89abcdef89abcdef89abcdef89abcdef89abcdef".to_string(),
            contents: None,
        };
        insta::assert_snapshot!(render_to_string(&init_tera(), "tag.html", &template).unwrap());
    }
}
