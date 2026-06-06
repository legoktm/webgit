use crate::cache::CachingRepo;
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::{Context, Tera};

async fn build_tag(repo: &CachingRepo, tag: String) -> TagTemplate {
    let ref_name = RefName::Ref(format!("tags/{tag}").into_bytes());
    let ref_ = repo
        .lookup_ref(&ref_name)
        .await
        .unwrap_or_else(|_| panic!("unable to find commit for {tag}"));
    let commit = repo
        .peel_ref_to_commit(&ref_)
        .await
        .unwrap_or_else(|_| panic!("unable to find commit for {tag}"))
        .ok_or_else(|| format!("unable to find commit for {tag}"))
        .unwrap();
    let tag_object_id = if let RefTarget::Direct(object_id) = ref_.target() {
        object_id
    } else {
        panic!("reftarget is symbolic");
    };
    let tag_obj = repo
        .lookup_object(*tag_object_id)
        .await
        .unwrap()
        .tag()
        .unwrap();

    TagTemplate {
        name: tag,
        date: tag_obj.date().unwrap().to_string(),
        tagger_name: String::from_utf8_lossy(tag_obj.tagger_name().unwrap()).to_string(),
        commit: commit.id().to_string(),
        contents: String::from_utf8(tag_obj.message().to_vec()).unwrap(),
    }
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
) {
    let template = build_tag(repo, tag).await;
    let ctx = Context::from_serialize(&template).unwrap();
    match tera.render("tag.html", &ctx) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}
