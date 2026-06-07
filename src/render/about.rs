use crate::cache::CachingRepo;
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::{Context, Tera};

#[derive(Serialize)]
struct AboutTemplate {
    version: &'static str,
    clone_url: String,
    head_branch: String,
    branch_count: usize,
    tag_count: usize,
    idb_available: bool,
    object_count: usize,
    size_mb: String,
    cached_tag_refs: usize,
}

pub(crate) async fn render_about(
    tera: &Tera,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) {
    let head_branch = repo
        .head()
        .await
        .ok()
        .and_then(|r| match r.target() {
            RefTarget::Symbolic(RefName::Ref(b)) => {
                b.strip_prefix(b"heads/")
                    .map(|s| String::from_utf8_lossy(s).into_owned())
            }
            _ => None,
        })
        .unwrap_or_else(|| "(detached)".to_string());

    let (branch_count, tag_count) = repo
        .ref_names()
        .await
        .map(|names| {
            let branches = names
                .iter()
                .filter(|n| matches!(n, RefName::Ref(b) if b.starts_with(b"heads/")))
                .count();
            let tags = names
                .iter()
                .filter(|n| matches!(n, RefName::Ref(b) if b.starts_with(b"tags/")))
                .count();
            (branches, tags)
        })
        .unwrap_or((0, 0));

    let (idb_available, object_count, size_mb, cached_tag_refs) =
        match repo.about_stats().await {
            Some((objects, mb, tag_refs)) => {
                (true, objects, format!("{mb:.2}"), tag_refs)
            }
            None => (false, 0, String::new(), 0),
        };

    let template = AboutTemplate {
        version: env!("CARGO_PKG_VERSION"),
        clone_url: clone_url.to_string(),
        head_branch,
        branch_count,
        tag_count,
        idb_available,
        object_count,
        size_mb,
        cached_tag_refs,
    };

    let ctx = Context::from_serialize(&template).unwrap();
    match tera.render("about.html", &ctx) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {e}</p>"))
        }
    }
}
