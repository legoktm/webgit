mod cache;
mod error;
mod fetch;
mod fs;
mod render;
mod route;
mod stats;

use cache::CachingRepo;
use error::{GitContext, error_html};
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use route::{handle_route, set_text};
use stats::{format_stats, set_stats_loaded};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::Document;

fn console_log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

// ---------------------------------------------------------------------------
// Initial repo load
// ---------------------------------------------------------------------------

async fn load_repo(url: String, doc: Document) {
    let output = doc.get_element_by_id("output").unwrap();
    if let Err(e) = try_load_repo(url, doc.clone()).await {
        output.set_inner_html(&error_html(&format!("{e:#}")));
    }
}

async fn try_load_repo(url: String, doc: Document) -> anyhow::Result<()> {
    // Register live progress updates on the persistent stats bar.
    let stats_el = doc.get_element_by_id("fetch-stats");
    fetch::reset_and_watch(Box::new(move |reqs, bytes, cached_bytes| {
        if let Some(el) = &stats_el {
            el.set_text_content(Some(&format_stats(
                "Loading\u{2026}",
                reqs,
                bytes,
                cached_bytes,
            )));
        }
    }));

    let dir = HttpDirectory::new(url.clone());
    let repo = Repo::<HttpFilesystem>::open(dir)
        .await
        .context("Failed to open repo")?;
    let repo = CachingRepo::open(repo, url.clone()).await;

    // Surface a banner when caching is disabled so the slow performance is
    // explained rather than mysterious.
    if !repo.idb_available()
        && let Some(el) = doc.get_element_by_id("idb-warning")
    {
        el.class_list().remove_1("hide").ok();
    }

    let head = repo.head().await.context("Failed to read HEAD")?;

    let commit = repo
        .peel_ref_to_commit(&head)
        .await
        .context("Failed to peel HEAD to commit")?
        .ok_or_else(|| anyhow::anyhow!("HEAD does not point to a commit"))?;

    let path = repo_path(&url);
    doc.set_title(&path);
    set_text(&doc, "repo-path-name", &path);

    let root_tree = repo
        .lookup_object(commit.tree())
        .await
        .context("Failed to load root tree")?
        .tree()
        .map_err(|e| anyhow::anyhow!("Root object is not a tree: {e:?}"))?;

    let head_commit = Rc::new(commit);
    let root_tree = Rc::new(root_tree);
    let repo = Rc::new(repo);
    let clone_url = Rc::new(url.clone());
    let tera = Rc::new(render::init_tera());

    // Initial route.
    let hash = web_sys::window()
        .unwrap()
        .location()
        .hash()
        .unwrap_or_default();
    handle_route(
        hash,
        &head_commit,
        &root_tree,
        &repo,
        &clone_url,
        &doc,
        &tera,
    )
    .await;
    set_stats_loaded(&doc);

    // hashchange listener.
    let doc_c = doc.clone();
    let head_commit_c = Rc::clone(&head_commit);
    let root_tree_c = Rc::clone(&root_tree);
    let repo_c = Rc::clone(&repo);
    let clone_url_c = Rc::clone(&clone_url);
    let tera_c = Rc::clone(&tera);
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
        let tera = Rc::clone(&tera_c);
        wasm_bindgen_futures::spawn_local(async move {
            handle_route(
                hash,
                &head_commit,
                &root_tree,
                &repo,
                &clone_url,
                &doc,
                &tera,
            )
            .await;
            set_stats_loaded(&doc);
        });
    });

    web_sys::window()
        .unwrap()
        .add_event_listener_with_callback("hashchange", cb.as_ref().unchecked_ref())
        .expect("failed to add hashchange listener");

    cb.forget();
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository index
// ---------------------------------------------------------------------------

/// Shown when the URL doesn't name a repository: lists the server's repos from
/// `/listing.json`.
async fn load_index(doc: Document) {
    let output = doc.get_element_by_id("output").unwrap();
    // The repo-scoped chrome (nav tabs, fetch stats) is meaningless here.
    for id in ["nav", "fetch-stats"] {
        if let Some(el) = doc.get_element_by_id(id) {
            el.class_list().add_1("hide").ok();
        }
    }
    set_text(&doc, "repo-path-name", "repositories");
    doc.set_title("repositories");
    if let Err(e) = try_load_index(&output).await {
        output.set_inner_html(&error_html(&format!("{e:#}")));
    }
}

async fn try_load_index(output: &web_sys::Element) -> anyhow::Result<()> {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    let url = format!("{origin}/listing.json");
    let text = fetch::fetch_text(&url)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load {url}: {e:?}"))?;
    let paths: Vec<String> = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse listing.json: {e}"))?;
    let tera = render::init_tera();
    render::listing::render_listing(&tera, paths, output)
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

/// The path portion of the repository URL, used as its display name —
/// e.g. `https://git.example.com/public/webgit.git/` becomes
/// `public/webgit.git`.
fn repo_path(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    // Drop the host (everything up to the first '/'); if there is no '/', there
    // is no host component, so treat the whole string as the path.
    let path = without_scheme
        .split_once('/')
        .map_or(without_scheme, |(_host, path)| path);
    path.trim_matches('/').to_string()
}

fn resolve_repo_url(window: &web_sys::Window) -> Option<String> {
    let location = window.location();

    if let Ok(href) = location.href() {
        let bare = href.split(['?', '#']).next().unwrap_or(&href);
        if bare.ends_with(".git") || bare.ends_with(".git/") {
            return Some(bare.trim_end_matches('/').to_string());
        }
    }

    // The ?url= override lets a local dev build point at any remote repo. It's
    // only honored on a loopback host, so a deployed instance can't be coaxed
    // into fetching arbitrary URLs on a visitor's behalf.
    let on_loopback = location.hostname().map(|h| h == "127.0.0.1").unwrap_or(false);
    if on_loopback
        && let Ok(search) = location.search()
    {
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
            // No repository in the URL: show the server's repository index.
            wasm_bindgen_futures::spawn_local(async move {
                load_index(document).await;
            });
            return;
        }
    };

    wasm_bindgen_futures::spawn_local(async move {
        load_repo(url, document).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_path() {
        assert_eq!(repo_path("https://example.org/foo/bar.git"), "foo/bar.git");
        assert_eq!(repo_path("https://example.org/foo/bar.git/"), "foo/bar.git");
        assert_eq!(repo_path("https://example.org/foo/bar"), "foo/bar");
        assert_eq!(repo_path("https://git.example.com/public/webgit.git/"), "public/webgit.git");
        // No host component: the whole string is the path.
        assert_eq!(repo_path("bar.git"), "bar.git");
        assert_eq!(repo_path(""), "");
    }
}
