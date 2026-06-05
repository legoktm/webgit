use crate::cache::CachingRepo;
use crate::render::{blob::render_blob, summary::render_summary, tree::render_tree};
use git_async::object::{ObjectId, Tree, TreeEntryType};
use wasm_bindgen::JsCast;
use web_sys::Document;

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

pub(crate) fn set_text(doc: &Document, id: &str, text: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        el.set_text_content(Some(text));
    }
}

fn show(doc: &Document, id: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        let _ = el.remove_attribute("style");
    }
}

fn hide_path_bar(doc: &Document) {
    if let Some(el) = doc.get_element_by_id("path-bar") {
        el.set_attribute("style", "display:none").ok();
    }
}

fn update_path_bar(doc: &Document, path: &str) {
    let bar = match doc.get_element_by_id("path-bar") {
        Some(el) => el,
        None => return,
    };
    let mut html = String::from("<a href=\"#!/tree\">root</a>");
    let mut cumulative = String::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(component);
        html.push_str(&format!(
            " / <a href=\"#!/tree/{0}\">{1}</a>",
            cumulative, component
        ));
    }
    bar.set_inner_html(&format!("path: {}", html));
}

fn set_active_tab(doc: &Document, tab: &str) {
    if let Ok(tabs) = doc.query_selector_all("#nav a") {
        for i in 0..tabs.length() {
            if let Some(node) = tabs.get(i)
                && let Ok(el) = node.dyn_into::<web_sys::Element>()
            {
                let href = el.get_attribute("href").unwrap_or_default();
                if href.starts_with(tab) {
                    el.class_list().add_1("active").ok();
                } else {
                    el.class_list().remove_1("active").ok();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tree / blob walking
// ---------------------------------------------------------------------------

async fn walk_to_tree(root: &Tree, path: &str, repo: &CachingRepo) -> Option<Tree> {
    let mut current = root.clone();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        let entry = current
            .entries()
            .find(|e| e.name() == component.as_bytes())?;
        if entry.entry_type() != TreeEntryType::Tree {
            return None;
        }
        let obj = repo.lookup_object(entry.id()).await.ok()?;
        current = obj.tree().ok()?;
    }
    Some(current)
}

async fn walk_to_blob(root: &Tree, path: &str, repo: &CachingRepo) -> Option<(ObjectId, Vec<u8>)> {
    let (dir_path, filename) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let tree = walk_to_tree(root, dir_path, repo).await?;
    let entry = tree.entries().find(|e| e.name() == filename.as_bytes())?;
    let obj = repo.lookup_object(entry.id()).await.ok()?;
    let blob = obj.blob().ok()?;
    let id = blob.id();
    Some((id, blob.data_owned()))
}

// ---------------------------------------------------------------------------
// Hash routing
// ---------------------------------------------------------------------------

pub(crate) enum Route {
    Summary,
    Tree(String),
}

pub(crate) fn parse_hash(hash: &str) -> Option<Route> {
    if hash == "#!/summary" || hash.is_empty() || hash == "#" {
        return Some(Route::Summary);
    }
    let rest = hash.strip_prefix("#!/tree")?;
    Some(Route::Tree(rest.trim_start_matches('/').to_string()))
}

pub(crate) async fn handle_route(
    hash: String,
    head_commit: &git_async::object::Commit,
    root_tree: &Tree,
    repo: &CachingRepo,
    clone_url: &str,
    doc: &Document,
) {
    let output = match doc.get_element_by_id("output") {
        Some(el) => el,
        None => return,
    };

    match parse_hash(&hash) {
        None => {
            // Unknown route — redirect to summary.
            web_sys::window()
                .unwrap()
                .location()
                .set_hash("#!/summary")
                .ok();
        }
        Some(Route::Summary) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/summary");
            render_summary(head_commit, repo, clone_url, &output).await;
        }
        Some(Route::Tree(path)) => {
            update_path_bar(doc, &path);
            show(doc, "path-bar");
            set_active_tab(doc, "#!/tree");

            if let Some(subtree) = walk_to_tree(root_tree, &path, repo).await {
                render_tree(&subtree, &path, &output);
                return;
            }

            output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");
            match walk_to_blob(root_tree, &path, repo).await {
                Some((id, data)) => render_blob(id, &data, &output),
                None => output.set_inner_html(&format!(
                    "<p class=\"msg error\">Not found: <code>{}</code></p>",
                    path
                )),
            }
        }
    }
}
