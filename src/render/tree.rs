use crate::route::{encode_component, encode_path};
use git_async::object::{Tree, TreeEntryType};
use yew::prelude::*;

fn mode_string(entry_type: TreeEntryType) -> &'static str {
    match entry_type {
        TreeEntryType::Tree => "d---------",
        TreeEntryType::File => "-rw-r--r--",
        TreeEntryType::Executable => "-rwxr-xr-x",
        TreeEntryType::Symlink => "l---------",
        TreeEntryType::Commit => "m---------",
    }
}

/// A single row in the tree listing. Doubles as the per-entry data the
/// component renders and as the unit-test fixture.
#[derive(PartialEq, Clone)]
pub(crate) struct TreeEntryRow {
    mode: String,
    name: String,
    path: String,
    is_dir: bool,
}

fn tree_rows(tree: &Tree, prefix: &str) -> Vec<TreeEntryRow> {
    tree.entries()
        .map(|e| {
            let name = String::from_utf8_lossy(e.name()).into_owned();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            TreeEntryRow {
                mode: mode_string(e.entry_type()).to_string(),
                is_dir: e.entry_type() == TreeEntryType::Tree,
                name,
                path,
            }
        })
        .collect()
}

/// The view inputs for a tree listing: the rows and the `?h=…` suffix appended
/// to every link so navigation stays pinned to the current ref.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct TreeProps {
    pub entries: Vec<TreeEntryRow>,
    pub head_suffix: String,
}

pub(crate) fn build_tree_props(tree: &Tree, prefix: &str, head: Option<&str>) -> TreeProps {
    TreeProps {
        entries: tree_rows(tree, prefix),
        head_suffix: head.map_or(String::new(), |h| format!("?h={}", encode_component(h))),
    }
}

/// The Yew component used to mount the tree view into the DOM. The markup lives
/// in the plain `tree_view` function below so it can be unit-tested without a
/// renderer.
#[function_component(TreeView)]
pub(crate) fn tree_view_component(props: &TreeProps) -> Html {
    tree_view(props)
}

pub(crate) fn tree_view(props: &TreeProps) -> Html {
    let TreeProps {
        entries,
        head_suffix,
    } = props;

    html! {
        <table class="tree-table">
            <thead>
                <tr>
                    <th>{ "Mode" }</th>
                    <th>{ "Name" }</th>
                </tr>
            </thead>
            <tbody>
                { for entries.iter().map(|e| tree_row(e, head_suffix)) }
            </tbody>
        </table>
    }
}

fn tree_row(entry: &TreeEntryRow, head_suffix: &str) -> Html {
    let href = format!("#!/tree/{}{}", encode_path(&entry.path), head_suffix);
    html! {
        <tr key={entry.path.clone()}>
            <td class="mode">{ &entry.mode }</td>
            <td class="name">
                if entry.is_dir {
                    <a href={href}>{ &entry.name }</a>
                } else {
                    <a class="file" href={href}>{ &entry.name }</a>
                }
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `TreeView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines so the snapshot reads as a tree. See the
    /// equivalent helper in `render::tag` for why we go through SSR and why
    /// indentation is omitted.
    fn render(props: TreeProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<TreeView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_mode_string() {
        assert_eq!(mode_string(TreeEntryType::Tree), "d---------");
        assert_eq!(mode_string(TreeEntryType::File), "-rw-r--r--");
        assert_eq!(mode_string(TreeEntryType::Executable), "-rwxr-xr-x");
        assert_eq!(mode_string(TreeEntryType::Symlink), "l---------");
        assert_eq!(mode_string(TreeEntryType::Commit), "m---------");
    }

    fn fixture_rows() -> Vec<TreeEntryRow> {
        vec![
            TreeEntryRow {
                mode: mode_string(TreeEntryType::Tree).to_string(),
                name: "src".to_string(),
                path: "src".to_string(),
                is_dir: true,
            },
            TreeEntryRow {
                mode: mode_string(TreeEntryType::File).to_string(),
                name: "README.md".to_string(),
                path: "README.md".to_string(),
                is_dir: false,
            },
            TreeEntryRow {
                mode: mode_string(TreeEntryType::Executable).to_string(),
                name: "build.sh".to_string(),
                path: "scripts/build.sh".to_string(),
                is_dir: false,
            },
        ]
    }

    #[test]
    fn test_tree_html() {
        insta::assert_snapshot!(render(TreeProps {
            entries: fixture_rows(),
            head_suffix: String::new(),
        }));
    }

    #[test]
    fn test_tree_html_with_head() {
        insta::assert_snapshot!(render(TreeProps {
            entries: fixture_rows(),
            head_suffix: "?h=stable".to_string(),
        }));
    }
}
