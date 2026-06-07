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

pub(crate) fn render_tree(
    tera: &Tera,
    tree: &Tree,
    prefix: &str,
    output: &web_sys::Element,
) -> Result<(), String> {
    let rows = tree_rows(tree, prefix);
    let mut ctx = Context::new();
    ctx.insert("entries", &rows);
    let html = tera.render("tree.html", &ctx).map_err(|e| format!("Template error: {e}"))?;
    output.set_inner_html(&html);
    Ok(())
}
