use crate::{
    cache::CachingRepo,
    render::{collect_refs, render_template},
};
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
    objects: usize,
    size_mb: String,
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

    let (branches, tags) = collect_refs(repo).await;
    let (branch_count, tag_count) = (branches.len(), tags.len());

    let template = match repo.about_stats().await {
        Some((objects, size_mb)) => AboutTemplate {
            version: env!("CARGO_PKG_VERSION"),
            clone_url: clone_url.to_string(),
            head_branch,
            branch_count,
            tag_count,
            idb_available: true,
            objects,
            size_mb: format!("{size_mb:.2}"),
        },
        None => AboutTemplate {
            version: env!("CARGO_PKG_VERSION"),
            clone_url: clone_url.to_string(),
            head_branch,
            branch_count,
            tag_count,
            idb_available: false,
            objects: 0,
            size_mb: String::new(),
        },
    };

    render_template(tera, "about.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{init_tera, render_to_string};

    fn fixture(idb_available: bool) -> AboutTemplate {
        AboutTemplate {
            version: "0.0.0-test",
            clone_url: "https://example.org/repo.git".to_string(),
            head_branch: "main".to_string(),
            branch_count: 3,
            tag_count: 7,
            idb_available,
            objects: 5678,
            size_mb: "56.78".to_string(),
        }
    }

    #[test]
    fn test_about_html_with_idb() {
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "about.html", &fixture(true)).unwrap()
        );
    }

    #[test]
    fn test_about_html_without_idb() {
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "about.html", &fixture(false)).unwrap()
        );
    }
}
