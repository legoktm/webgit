mod error;
mod fetch;
mod fs;

use error::fmt_git_err;
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use git_async::object::{Tree, TreeEntryType};
use git_async::reference::{RefName, RefTarget};
use serde::Serialize;
use tera::{Context, Tera};
use wasm_bindgen::prelude::*;
use web_sys::Document;

const TREE_TEMPLATE: &str = include_str!("templates/tree.html");

#[derive(Serialize)]
struct TreeEntryRow {
    mode: String,
    name: String,
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

/// Collect the shallow entries of `tree` into a vec of `TreeEntryRow`.
fn tree_rows(tree: &Tree) -> Vec<TreeEntryRow> {
    tree.entries()
        .map(|e| TreeEntryRow {
            mode: mode_string(e.entry_type()).to_string(),
            name: String::from_utf8_lossy(e.name()).into_owned(),
            is_dir: e.entry_type() == TreeEntryType::Tree,
        })
        .collect()
}

fn set_text(doc: &web_sys::Document, id: &str, text: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        el.set_text_content(Some(text));
    }
}

fn set_inner_html(doc: &web_sys::Document, id: &str, html: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        el.set_inner_html(html);
    }
}

fn show(doc: &web_sys::Document, id: &str) {
    if let Some(el) = doc.get_element_by_id(id) {
        let _ = el.remove_attribute("style");
    }
}

async fn load_repo(url: String, doc: web_sys::Document) {
    let output = match doc.get_element_by_id("output") {
        Some(el) => el,
        None => return,
    };

    output.set_inner_html(&format!(
        "<p class=\"msg\">Opening repo at <code>{}</code>\u{2026}</p>",
        url
    ));

    // ── Open repo ────────────────────────────────────────────────────
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

    // ── Resolve HEAD ─────────────────────────────────────────────────
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

    let branch_label = match head.target() {
        RefTarget::Symbolic(RefName::Head) => "HEAD".to_string(),
        RefTarget::Symbolic(RefName::Ref(name)) => String::from_utf8_lossy(name).into_owned(),
        RefTarget::Direct(oid) => format!("{}", oid),
    };

    // ── Peel to commit ───────────────────────────────────────────────
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

    // ── Populate commit strip ────────────────────────────────────────
    let commit_hash = format!("{}", commit.id());
    let short_hash = &commit_hash[..8];
    let author = String::from_utf8_lossy(commit.author_name()).into_owned();
    let date = commit.author_date().format("%Y-%m-%d %H:%M %z").to_string();
    let message = String::from_utf8_lossy(commit.message())
        .trim_end()
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    // Derive a display name from the URL (last path component, strip .git).
    let repo_name = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(&url)
        .trim_end_matches(".git")
        .to_string();

    set_text(&doc, "repo-path-name", &repo_name);
    set_text(&doc, "strip-branch", &branch_label);
    set_inner_html(
        &doc,
        "strip-hash",
        &format!("<a href=\"#\">{}</a>", short_hash),
    );
    set_text(&doc, "strip-author", &author);
    set_text(&doc, "strip-date", &date);
    set_text(&doc, "strip-msg", &message);
    show(&doc, "commit-strip");

    // ── Load root tree ───────────────────────────────────────────────
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

    // ── Render tree table via Tera ───────────────────────────────────
    let rows = tree_rows(&root_tree);
    let mut ctx = Context::new();
    ctx.insert("entries", &rows);

    match Tera::one_off(TREE_TEMPLATE, &ctx, true) {
        Ok(html) => output.set_inner_html(&html),
        Err(e) => {
            output.set_inner_html(&format!("<p class=\"msg error\">Template error: {}</p>", e))
        }
    }
}

fn resolve_repo_url(window: &web_sys::Window) -> Option<String> {
    let location = window.location();

    // 1. Current page URL ends in .git
    if let Ok(href) = location.href() {
        let bare = href.split('?').next().unwrap_or(&href);
        if bare.ends_with(".git") || bare.ends_with(".git/") {
            return Some(bare.trim_end_matches('/').to_string());
        }
    }

    // 2. ?url= query parameter
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
                     <code>?url=https://…/repo.git</code> query parameter.</p>",
                );
            }
            return;
        }
    };

    wasm_bindgen_futures::spawn_local(async move {
        load_repo(url, document).await;
    });
}
