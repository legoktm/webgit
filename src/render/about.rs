use crate::{cache::CachingRepo, render::render_template};
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::Tera;

#[derive(Serialize)]
struct AboutTemplate {
    version: &'static str,
    clone_url: String,
    head_branch: String,
    branch_count: usize,
    tag_count: usize,
    idb_available: bool,
    repo_objects: usize,
    repo_size_mb: String,
    global_objects: usize,
    global_size_mb: String,
    repo_tag_refs: usize,
    global_tag_refs: usize,
}

pub(crate) async fn render_about(
    tera: &Tera,
    repo: &CachingRepo,
    clone_url: &str,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let head_branch = repo
        .head()
        .await
        .ok()
        .and_then(|r| match r.target() {
            RefTarget::Symbolic(RefName::Ref(b)) => b
                .strip_prefix(b"heads/")
                .map(|s| String::from_utf8_lossy(s).into_owned()),
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

    let template = match repo.about_stats().await {
        Some((repo_obj, repo_mb, global_obj, global_mb, repo_tags, global_tags)) => {
            AboutTemplate {
                version: env!("CARGO_PKG_VERSION"),
                clone_url: clone_url.to_string(),
                head_branch,
                branch_count,
                tag_count,
                idb_available: true,
                repo_objects: repo_obj,
                repo_size_mb: format!("{repo_mb:.2}"),
                global_objects: global_obj,
                global_size_mb: format!("{global_mb:.2}"),
                repo_tag_refs: repo_tags,
                global_tag_refs: global_tags,
            }
        }
        None => AboutTemplate {
            version: env!("CARGO_PKG_VERSION"),
            clone_url: clone_url.to_string(),
            head_branch,
            branch_count,
            tag_count,
            idb_available: false,
            repo_objects: 0,
            repo_size_mb: String::new(),
            global_objects: 0,
            global_size_mb: String::new(),
            repo_tag_refs: 0,
            global_tag_refs: 0,
        },
    };

    render_template(tera, "about.html", &template, output)
}
