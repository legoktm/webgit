use crate::render::render_template;
use serde::Serialize;
use std::collections::BTreeMap;
use tera::Tera;

#[derive(Serialize)]
struct RepoEntry {
    /// Basename within its section, e.g. `foo.git`.
    name: String,
    /// Root-absolute link to the repository's webgit view.
    href: String,
}

#[derive(Serialize)]
struct RepoGroup {
    /// The shared parent-directory prefix; empty for top-level repositories.
    section: String,
    repos: Vec<RepoEntry>,
}

#[derive(Serialize)]
struct ListingTemplate {
    groups: Vec<RepoGroup>,
}

/// Group repositories by their common parent-directory prefix, cgit-style: each
/// distinct directory becomes a section, with the repositories listed by their
/// basename underneath. Sections are ordered alphabetically (top-level repos,
/// with no prefix, first) and repositories within a section by name.
fn group_repos(paths: &[String]) -> Vec<RepoGroup> {
    let mut by_section: BTreeMap<&str, Vec<RepoEntry>> = BTreeMap::new();
    for path in paths {
        let path = path.trim_matches('/');
        if path.is_empty() {
            continue;
        }
        let (section, name) = path.rsplit_once('/').unwrap_or(("", path));
        by_section.entry(section).or_default().push(RepoEntry {
            name: name.to_string(),
            href: format!("/{path}/"),
        });
    }
    by_section
        .into_iter()
        .map(|(section, mut repos)| {
            repos.sort_by(|a, b| a.name.cmp(&b.name));
            RepoGroup {
                section: section.to_string(),
                repos,
            }
        })
        .collect()
}

pub(crate) fn render_listing(
    tera: &Tera,
    paths: Vec<String>,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = ListingTemplate {
        groups: group_repos(&paths),
    };
    render_template(tera, "listing.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{init_tera, render_to_string};

    fn entries(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn deserializes_array_of_strings() {
        let json = r#"["public/foo.git", "public/bar.git"]"#;
        let paths: Vec<String> = serde_json::from_str(json).unwrap();
        assert_eq!(paths, ["public/foo.git", "public/bar.git"]);
    }

    fn names(group: &RepoGroup) -> Vec<&str> {
        group.repos.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn groups_by_parent_directory() {
        let groups = group_repos(&entries(&[
            "public/foo.git",
            "public/bar.git",
            "private/secret.git",
            "top.git",
        ]));
        let sections: Vec<&str> = groups.iter().map(|g| g.section.as_str()).collect();
        // Top-level (no prefix) first, then sections alphabetically.
        assert_eq!(sections, ["", "private", "public"]);
        // Repos within a section sorted by name, basename keeps its `.git`.
        assert_eq!(names(&groups[2]), ["bar.git", "foo.git"]);
        assert_eq!(groups[2].repos[0].href, "/public/bar.git/");
        assert_eq!(names(&groups[0]), ["top.git"]);
        assert_eq!(groups[0].repos[0].href, "/top.git/");
    }

    #[test]
    fn groups_handle_interleaved_subdirs() {
        // `a/a.git` and `a/x.git` share section `a` even though `a/b/y.git`
        // sorts between them by full path — grouping must not split `a`.
        let groups = group_repos(&entries(&["a/a.git", "a/b/y.git", "a/x.git"]));
        let sections: Vec<&str> = groups.iter().map(|g| g.section.as_str()).collect();
        assert_eq!(sections, ["a", "a/b"]);
        assert_eq!(names(&groups[0]), ["a.git", "x.git"]);
    }

    #[test]
    fn test_listing_html() {
        let template = ListingTemplate {
            groups: group_repos(&entries(&[
                "public/foo.git",
                "public/bar.git",
                "top.git",
            ])),
        };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "listing.html", &template).unwrap()
        );
    }

    #[test]
    fn test_listing_html_empty() {
        let template = ListingTemplate { groups: vec![] };
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "listing.html", &template).unwrap()
        );
    }
}
