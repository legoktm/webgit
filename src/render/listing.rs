use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use std::fmt;
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
    /// The prefix the repositories live under; empty for top-level ones.
    section: String,
    repos: Vec<RepoEntry>,
}

/// The view inputs for the repository index: the sections of `listing.json`, in
/// the order the file gives them. Doubles as the component's props and the
/// unit-test fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct ListingProps {
    groups: Vec<RepoGroup>,
}

/// One element of `listing.json`: an object mapping a prefix to the
/// repositories under it, e.g. `{"public": ["foo.git", "bar.git"]}`.
///
/// Deserialized by hand rather than as a map because `serde_json`'s map type
/// sorts its keys, and the point of the array-of-objects shape is that the page
/// mirrors the file's order. An object with several keys is allowed, and its
/// sections stay in the order written.
struct ListingEntry {
    sections: Vec<(String, Vec<String>)>,
}

impl<'de> Deserialize<'de> for ListingEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = ListingEntry;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object mapping a prefix to a list of repositories")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut sections = Vec::new();
                while let Some(entry) = map.next_entry::<String, Vec<String>>()? {
                    sections.push(entry);
                }
                Ok(ListingEntry { sections })
            }
        }

        deserializer.deserialize_map(EntryVisitor)
    }
}

/// Turn the parsed file into view groups, one per section, keeping both the
/// section order and the repository order the file gave. Sections with nothing
/// in them are dropped so the table never shows a dangling header.
fn listing_groups(entries: Vec<ListingEntry>) -> Vec<RepoGroup> {
    entries
        .into_iter()
        .flat_map(|entry| entry.sections)
        .map(|(section, repos)| {
            let section = section.trim_matches('/');
            let repos = repos
                .iter()
                .map(|name| name.trim_matches('/'))
                .filter(|name| !name.is_empty())
                .map(|name| RepoEntry {
                    name: name.to_string(),
                    href: if section.is_empty() {
                        format!("/{name}/")
                    } else {
                        format!("/{section}/{name}/")
                    },
                })
                .collect::<Vec<_>>();
            RepoGroup {
                section: section.to_string(),
                repos,
            }
        })
        .filter(|group| !group.repos.is_empty())
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
                        { for groups.iter().enumerate().map(repo_group_rows) }
                    </tbody>
                </table>
            }
        </>
    }
}

/// A section's rows: an optional section header (omitted for top-level repos,
/// which have no prefix) followed by one row per repository. Keys are the row's
/// position, not its href: the file decides the listing, so the same prefix —
/// or the same repository — may appear more than once, and keys must stay
/// unique across the table regardless.
fn repo_group_rows((i, g): (usize, &RepoGroup)) -> Html {
    html! {
        <>
            if !g.section.is_empty() {
                <tr class="repo-section" key={format!("{i}:section")}>
                    <td>{ g.section.clone() }</td>
                </tr>
            }
            { for g.repos.iter().enumerate().map(|(j, r)| repo_row(i, j, r)) }
        </>
    }
}

fn repo_row(i: usize, j: usize, r: &RepoEntry) -> Html {
    html! {
        <tr key={format!("{i}:{j}")}>
            <td class="name"><a href={r.href.clone()}>{ r.name.clone() }</a></td>
        </tr>
    }
}

/// Parse the body of `listing.json` into the repository-index props.
pub(crate) fn parse_listing(json: &str) -> serde_json::Result<ListingProps> {
    Ok(ListingProps {
        groups: listing_groups(serde_json::from_str(json)?),
    })
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

    fn groups(json: &str) -> Vec<RepoGroup> {
        parse_listing(json).unwrap().groups
    }

    fn sections(groups: &[RepoGroup]) -> Vec<&str> {
        groups.iter().map(|g| g.section.as_str()).collect()
    }

    fn names(group: &RepoGroup) -> Vec<&str> {
        group.repos.iter().map(|r| r.name.as_str()).collect()
    }

    /// The page mirrors the file: sections in the order written, repositories
    /// in the order written, neither one sorted.
    #[test]
    fn keeps_the_files_order() {
        let groups = groups(
            r#"[
                {"public": ["foo.git", "bar.git"]},
                {"private": ["secret.git"]},
                {"a": ["z.git"]}
            ]"#,
        );
        assert_eq!(sections(&groups), ["public", "private", "a"]);
        assert_eq!(names(&groups[0]), ["foo.git", "bar.git"]);
        assert_eq!(groups[0].repos[0].href, "/public/foo.git/");
    }

    /// An empty prefix means the repositories sit at the web root: no section
    /// header, and hrefs without a directory component.
    #[test]
    fn empty_prefix_is_top_level() {
        let groups = groups(r#"[{"": ["top.git"]}]"#);
        assert_eq!(sections(&groups), [""]);
        assert_eq!(groups[0].repos[0].href, "/top.git/");
    }

    /// A prefix may be several directories deep, and repeating one is fine —
    /// it is two sections, not a regrouping.
    #[test]
    fn repeated_and_nested_prefixes_stay_separate() {
        let groups = groups(r#"[{"a": ["a.git"]}, {"a/b": ["y.git"]}, {"a": ["x.git"]}]"#);
        assert_eq!(sections(&groups), ["a", "a/b", "a"]);
        assert_eq!(names(&groups[2]), ["x.git"]);
        assert_eq!(groups[1].repos[0].href, "/a/b/y.git/");
    }

    /// Surrounding slashes on either half are tolerated, and a section left
    /// with no repositories is dropped rather than rendered as a bare header.
    #[test]
    fn trims_slashes_and_drops_empty_sections() {
        let groups = groups(r#"[{"/public/": ["/foo.git/", ""]}, {"empty": []}]"#);
        assert_eq!(sections(&groups), ["public"]);
        assert_eq!(names(&groups[0]), ["foo.git"]);
        assert_eq!(groups[0].repos[0].href, "/public/foo.git/");
    }

    /// Several prefixes in one object are allowed and keep their written order.
    #[test]
    fn one_object_may_hold_several_sections() {
        let groups = groups(r#"[{"z": ["z.git"], "a": ["a.git"]}]"#);
        assert_eq!(sections(&groups), ["z", "a"]);
    }

    #[test]
    fn rejects_the_old_flat_array_of_paths() {
        assert!(parse_listing(r#"["public/foo.git"]"#).is_err());
    }

    #[test]
    fn test_listing_html() {
        insta::assert_snapshot!(render(
            parse_listing(r#"[{"": ["top.git"]}, {"public": ["foo.git", "bar.git"]}]"#).unwrap()
        ));
    }

    #[test]
    fn test_listing_html_empty() {
        insta::assert_snapshot!(render(ListingProps { groups: vec![] }));
    }
}
