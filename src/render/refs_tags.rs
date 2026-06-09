use crate::{
    cache::CachingRepo,
    render::{RefRow, collect_ref_names, fetch_tag_rows, render_template},
};
use serde::Serialize;
use tera::Tera;

async fn build_refs_tags(repo: &CachingRepo) -> RefsTagsTemplate {
    let (_, tag_names) = collect_ref_names(repo).await;
    let mut tags = fetch_tag_rows(&tag_names, repo).await;
    tags.sort_by_key(|t| t.age);
    RefsTagsTemplate { tags }
}

#[derive(Serialize)]
struct RefsTagsTemplate {
    tags: Vec<RefRow>,
}

pub(crate) async fn render_refs_tags(
    tera: &Tera,
    repo: &CachingRepo,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_refs_tags(repo).await;
    render_template(tera, "refs_tags.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{fixtures, init_tera, render_to_string};

    #[test]
    fn test_refs_tags_html() {
        let template = RefsTagsTemplate {
            tags: vec![
                fixtures::ref_row("v1.1.0", "Release 1.1.0", "Kunal Mehta", 86400),
                fixtures::ref_row("v1.0.0", "Release 1.0.0", "Kunal Mehta", 86400 * 400),
            ],
        };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "refs_tags.html", &template).unwrap()
        );
    }

    #[test]
    fn test_refs_tags_html_empty() {
        let template = RefsTagsTemplate { tags: vec![] };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "refs_tags.html", &template).unwrap()
        );
    }
}
