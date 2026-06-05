mod error;
mod fetch;
mod fs;

use error::fmt_git_err;
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use git_async::object::{Commit, ObjectId, Tree, TreeEntryType};
use git_async::reference::RefName;
use serde::Serialize;

use std::collections::BinaryHeap;
use std::rc::Rc;
use tera::{Context, Tera};
use wasm_bindgen::prelude::*;
use web_sys::Document;

const TREE_TEMPLATE: &str = include_str!("templates/tree.html");
const BLOB_TEMPLATE: &str = include_str!("templates/blob.html");
const SUMMARY_TEMPLATE: &str = include_str!("templates/summary.html");

// ---------------------------------------------------------------------------
// Tree rendering
// ---------------------------------------------------------------------------

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

fn render_tree(tree: &Tree, prefix: &str, output: &web_sys::Element) {
    let rows = tree_rows(tree, prefix);
    let mut ctx = Context::new();
    ctx.insert("entries", &rows);
    match Tera::one_off(TREE_TEMPLATE, &ctx, true) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}

// ---------------------------------------------------------------------------
// Blob rendering
// ---------------------------------------------------------------------------

fn render_blob(blob_id: ObjectId, data: &[u8], output: &web_sys::Element) {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = match lines.as_slice() {
        [rest @ .., ""] => rest.to_vec(),
        other => other.to_vec(),
    };
    let mut ctx = Context::new();
    ctx.insert("blob_id", &format!("{}", blob_id));
    ctx.insert("lines", &lines);
    match Tera::one_off(BLOB_TEMPLATE, &ctx, true) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}

// ---------------------------------------------------------------------------
// Summary rendering
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct RefRow {
    name: String,
    short_hash: String,
    message: String,
    author: String,
    age: String,
}

#[derive(Serialize)]
struct CommitRow {
    short_hash: String,
    message: String,
    author: String,
    age: String,
}

fn age_string(dt: &chrono::DateTime<chrono::FixedOffset>) -> String {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp_millis() as f64;
    let secs = ((now_ms - then_ms) / 1000.0).max(0.0) as u64;
    match secs {
        s if s < 90 => format!("{} seconds", s),
        s if s < 90 * 60 => format!("{} minutes", s / 60),
        s if s < 36 * 3600 => format!("{} hours", s / 3600),
        s if s < 14 * 86400 => format!("{} days", s / 86400),
        s if s < 8 * 7 * 86400 => format!("{} weeks", s / (7 * 86400)),
        s if s < 24 * 30 * 86400 => format!("{} months", s / (30 * 86400)),
        s => format!("{} years", s / (365 * 86400)),
    }
}

fn commit_first_line(c: &Commit) -> String {
    String::from_utf8_lossy(c.message())
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn ref_row(name: String, c: &Commit) -> RefRow {
    let hash = format!("{}", c.id());
    RefRow {
        name,
        short_hash: hash[..8].to_string(),
        message: commit_first_line(c),
        author: String::from_utf8_lossy(c.author_name()).into_owned(),
        age: age_string(&c.author_date()),
    }
}

async fn build_summary(
    head_commit: &Commit,
    repo: &Repo<HttpFilesystem>,
) -> (Vec<RefRow>, Vec<RefRow>, Vec<CommitRow>) {
    let ref_names = repo.ref_names().await.unwrap_or_default();

    // --- Collect and select branch names before fetching any commits ---
    // Primary branch (main/master) goes first; remaining are alpha-sorted.
    // Total cap: 1 primary + 9 others = 10.
    let mut primary: Option<String> = None;
    let mut other_branches: Vec<String> = Vec::new();
    let mut tag_names: Vec<String> = Vec::new();

    for ref_name in &ref_names {
        let label = match ref_name {
            RefName::Head => continue,
            RefName::Ref(b) => String::from_utf8_lossy(b).into_owned(),
        };
        if let Some(short) = label.strip_prefix("heads/") {
            if short == "main" || short == "master" {
                primary = Some(short.to_string());
            } else {
                other_branches.push(short.to_string());
            }
        } else if let Some(short) = label.strip_prefix("tags/") {
            tag_names.push(short.to_string());
        }
    }

    other_branches.sort();
    let others_limit = if primary.is_some() { 9 } else { 10 };
    other_branches.truncate(others_limit);
    let branch_names: Vec<String> = primary.into_iter().chain(other_branches).collect();

    // Tags: reverse alpha, cap at 10.
    tag_names.sort_by(|a, b| b.cmp(a));
    tag_names.truncate(10);

    // --- Fetch commit data only for the selected refs ---
    let mut branches: Vec<RefRow> = Vec::new();
    for short in &branch_names {
        let rn = RefName::Ref(format!("heads/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = r.peel_to_commit(repo).await else {
            continue;
        };
        branches.push(ref_row(short.clone(), &commit));
    }

    let mut tags: Vec<RefRow> = Vec::new();
    for short in &tag_names {
        let rn = RefName::Ref(format!("tags/{}", short).into_bytes());
        let Ok(r) = repo.lookup_ref(&rn).await else {
            continue;
        };
        let Ok(Some(commit)) = r.peel_to_commit(repo).await else {
            continue;
        };
        tags.push(ref_row(short.clone(), &commit));
    }

    // --- Walk 10 commits from HEAD via full DAG, ordered by commit date ---
    let mut heap: BinaryHeap<(chrono::DateTime<chrono::FixedOffset>, Commit)> = BinaryHeap::new();
    heap.push((head_commit.commit_date(), head_commit.clone()));

    let mut commits: Vec<CommitRow> = Vec::new();
    while commits.len() < 10 {
        let (_, current) = match heap.pop() {
            Some(e) => e,
            None => break,
        };
        let hash = format!("{}", current.id());
        commits.push(CommitRow {
            short_hash: hash[..8].to_string(),
            message: commit_first_line(&current),
            author: String::from_utf8_lossy(current.author_name()).into_owned(),
            age: age_string(&current.author_date()),
        });
        let parents = match current.lookup_parents(repo).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        for parent in parents {
            heap.push((parent.commit_date(), parent));
        }
    }

    (branches, tags, commits)
}

async fn render_summary(
    head_commit: &Commit,
    repo: &Repo<HttpFilesystem>,
    clone_url: &str,
    output: &web_sys::Element,
) {
    let (branches, tags, commits) = build_summary(head_commit, repo).await;
    let mut ctx = Context::new();
    ctx.insert("branches", &branches);
    ctx.insert("tags", &tags);
    ctx.insert("commits", &commits);
    ctx.insert("clone_url", clone_url);
    match Tera::one_off(SUMMARY_TEMPLATE, &ctx, true) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}

// ---------------------------------------------------------------------------
// Path bar
// ---------------------------------------------------------------------------

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

fn hide_path_bar(doc: &Document) {
    if let Some(el) = doc.get_element_by_id("path-bar") {
        el.set_attribute("style", "display:none").ok();
    }
}

// ---------------------------------------------------------------------------
// Tree / blob walking
// ---------------------------------------------------------------------------

async fn walk_to_tree(root: &Tree, path: &str, repo: &Repo<HttpFilesystem>) -> Option<Tree> {
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

async fn walk_to_blob(
    root: &Tree,
    path: &str,
    repo: &Repo<HttpFilesystem>,
) -> Option<(ObjectId, Vec<u8>)> {
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
// DOM helpers
// ---------------------------------------------------------------------------

fn set_text(doc: &Document, id: &str, text: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        el.set_text_content(Some(text));
    }
}

fn show(doc: &Document, id: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        let _ = el.remove_attribute("style");
    }
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
// Hash routing
// ---------------------------------------------------------------------------

enum Route {
    Summary,
    Tree(String),
}

fn parse_hash(hash: &str) -> Option<Route> {
    if hash == "#!/summary" || hash.is_empty() || hash == "#" {
        return Some(Route::Summary);
    }
    let rest = hash.strip_prefix("#!/tree")?;
    Some(Route::Tree(rest.trim_start_matches('/').to_string()))
}

async fn handle_route(
    hash: String,
    head_commit: &Commit,
    root_tree: &Tree,
    repo: &Repo<HttpFilesystem>,
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
            output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");
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

// ---------------------------------------------------------------------------
// Initial repo load
// ---------------------------------------------------------------------------

async fn load_repo(url: String, doc: Document) {
    let output = match doc.get_element_by_id("output") {
        Some(el) => el,
        None => return,
    };

    output.set_inner_html(&format!(
        "<p class=\"msg\">Opening repo at <code>{}</code>\u{2026}</p>",
        url
    ));

    let dir = HttpDirectory::new(url.clone());
    let repo = match Repo::<HttpFilesystem>::open(dir).await {
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"msg error\">Failed to open repo: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
        Ok(r) => r,
    };

    let head = match repo.head().await {
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"msg error\">Failed to read HEAD: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
        Ok(h) => h,
    };

    output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");

    let commit = match head.peel_to_commit(&repo).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            output.set_inner_html("<p class=\"msg error\">HEAD does not point to a commit</p>");
            return;
        }
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"msg error\">Failed to peel HEAD to commit: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
    };

    let repo_name = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(&url)
        .trim_end_matches(".git")
        .to_string();

    set_text(&doc, "repo-path-name", &repo_name);

    let root_tree = match repo.lookup_object(commit.tree()).await {
        Ok(obj) => match obj.tree() {
            Ok(t) => t,
            Err(e) => {
                output.set_inner_html(&format!(
                    "<p class=\"msg error\">Root object is not a tree: {:?}</p>",
                    e
                ));
                return;
            }
        },
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"msg error\">Failed to load root tree: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
    };

    let head_commit = Rc::new(commit);
    let root_tree = Rc::new(root_tree);
    let repo = Rc::new(repo);
    let clone_url = Rc::new(url.clone());

    // Initial route.
    let hash = web_sys::window()
        .unwrap()
        .location()
        .hash()
        .unwrap_or_default();
    let initial_hash = if parse_hash(&hash).is_some() {
        hash
    } else {
        web_sys::window()
            .unwrap()
            .location()
            .set_hash("#!/summary")
            .ok();
        "#!/summary".to_string()
    };
    handle_route(
        initial_hash,
        &head_commit,
        &root_tree,
        &repo,
        &clone_url,
        &doc,
    )
    .await;

    // hashchange listener.
    let doc_c = doc.clone();
    let head_commit_c = Rc::clone(&head_commit);
    let root_tree_c = Rc::clone(&root_tree);
    let repo_c = Rc::clone(&repo);
    let clone_url_c = Rc::clone(&clone_url);
    let cb = Closure::<dyn Fn(web_sys::Event)>::new(move |_: web_sys::Event| {
        let hash = web_sys::window()
            .unwrap()
            .location()
            .hash()
            .unwrap_or_default();
        let doc = doc_c.clone();
        let head_commit = Rc::clone(&head_commit_c);
        let root_tree = Rc::clone(&root_tree_c);
        let repo = Rc::clone(&repo_c);
        let clone_url = Rc::clone(&clone_url_c);
        wasm_bindgen_futures::spawn_local(async move {
            handle_route(hash, &head_commit, &root_tree, &repo, &clone_url, &doc).await;
        });
    });

    web_sys::window()
        .unwrap()
        .add_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref())
        .expect("failed to add hashchange listener");

    cb.forget();
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

fn resolve_repo_url(window: &web_sys::Window) -> Option<String> {
    let location = window.location();

    if let Ok(href) = location.href() {
        let bare = href.split(['?', '#']).next().unwrap_or(&href);
        if bare.ends_with(".git") || bare.ends_with(".git/") {
            return Some(bare.trim_end_matches('/').to_string());
        }
    }

    if let Ok(search) = location.search() {
        for param in search.trim_start_matches('?').split('&') {
            if let Some(val) = param.strip_prefix("url=") {
                let decoded = js_sys::decode_uri_component(val)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_else(|| val.to_string());
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }

    None
}

#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();

    let window = web_sys::window().expect("no window");
    let document: Document = window.document().expect("no document");

    let url = match resolve_repo_url(&window) {
        Some(u) => u,
        None => {
            if let Some(output) = document.get_element_by_id("output") {
                output.set_inner_html(
                    "<p class=\"msg error\">No repository URL found. \
                     Navigate to a <code>.git</code> URL or add a \
                     <code>?url=https://\u{2026}/repo.git</code> query parameter.</p>",
                );
            }
            return;
        }
    };

    wasm_bindgen_futures::spawn_local(async move {
        load_repo(url, document).await;
    });
}
