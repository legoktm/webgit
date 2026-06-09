use git_async::object::{Tree, TreeEntryType};
use serde::Serialize;
use tera::{Context, Tera};

#[derive(Serialize)]
struct TreeEntryRow {
    mode: String,
    name: String,
    path: String,
    is_dir: bool,
}

fn mode_string(entry_type: TreeEntryType) -> &'static str {
    match entry_type {
        TreeEntryType::Tree => "d---------",
        TreeEntryType::File => "-rw-r--r--",
        TreeEntryType::Executable => "-rwxr-xr-x",
        TreeEntryType::Symlink => "l---------",
        TreeEntryType::Commit => "m---------",
    }
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

fn tree_context(rows: &[TreeEntryRow], head: Option<&str>) -> Context {
    let head_suffix = head.map_or(String::new(), |h| format!("?h={h}"));
    let mut ctx = Context::new();
    ctx.insert("entries", &rows);
    ctx.insert("head_suffix", &head_suffix);
    ctx
}

pub(crate) fn render_tree(
    tera: &Tera,
    tree: &Tree,
    prefix: &str,
    head: Option<&str>,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let rows = tree_rows(tree, prefix);
    let html = tera.render("tree.html", &tree_context(&rows, head))?;
    output.set_inner_html(&html);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::init_tera;

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
        let html = init_tera()
            .render("tree.html", &tree_context(&fixture_rows(), None))
            .unwrap();
        insta::assert_snapshot!(html);
    }

    #[test]
    fn test_tree_html_with_head() {
        let html = init_tera()
            .render("tree.html", &tree_context(&fixture_rows(), Some("stable")))
            .unwrap();
        insta::assert_snapshot!(html);
    }
}
