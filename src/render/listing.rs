use std::collections::BTreeMap;
use yew::prelude::*;

#[derive(PartialEq, Clone)]
struct RepoEntry {
    /// Basename within its section, e.g. `foo.git`.
    name: String,
    /// Root-absolute link to the repository's webgit view.
    href: String,
}

#[derive(PartialEq, Clone)]
struct RepoGroup {
    /// The shared parent-directory prefix; empty for top-level repositories.
    section: String,
    repos: Vec<RepoEntry>,
}

/// The view inputs for the repository index: repositories grouped by their
/// common parent directory. Doubles as the component's props and the unit-test
/// fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct ListingProps {
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

/// The Yew component used to mount the repository index into the DOM. The markup
/// lives in the plain `listing_view` function below so it can be exercised
/// without a renderer.
#[function_component(ListingView)]
pub(crate) fn listing_view_component(props: &ListingProps) -> Html {
    listing_view(props)
}

pub(crate) fn listing_view(props: &ListingProps) -> Html {
    let ListingProps { groups } = props;

    html! {
        <>
            <h3 class="summary-heading">{ "Repositories" }</h3>
            if groups.is_empty() {
                <p class="msg">{ "No repositories found." }</p>
            } else {
                <table class="summary-table repo-listing">
                    <thead>
                        <tr><th>{ "Name" }</th></tr>
                    </thead>
                    <tbody>
                        { for groups.iter().map(repo_group_rows) }
                    </tbody>
                </table>
            }
        </>
    }
}

/// A section's rows: an optional section header (omitted for top-level repos,
/// which have no prefix) followed by one row per repository.
fn repo_group_rows(g: &RepoGroup) -> Html {
    html! {
        <>
            if !g.section.is_empty() {
                <tr class="repo-section" key={format!("section:{}", g.section)}>
                    <td>{ g.section.clone() }</td>
                </tr>
            }
            { for g.repos.iter().map(repo_row) }
        </>
    }
}

fn repo_row(r: &RepoEntry) -> Html {
    html! {
        <tr key={r.href.clone()}>
            <td class="name"><a href={r.href.clone()}>{ r.name.clone() }</a></td>
        </tr>
    }
}

pub(crate) fn build_listing_props(paths: Vec<String>) -> ListingProps {
    ListingProps {
        groups: group_repos(&paths),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `ListingView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR.
    fn render(props: ListingProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<ListingView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

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
        insta::assert_snapshot!(render(ListingProps {
            groups: group_repos(&entries(&["public/foo.git", "public/bar.git", "top.git"])),
        }));
    }

    #[test]
    fn test_listing_html_empty() {
        insta::assert_snapshot!(render(ListingProps { groups: vec![] }));
    }
}
