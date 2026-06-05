mod cache;
mod error;
mod fetch;
mod fs;
mod render;
mod route;
mod stats;

use cache::CachingRepo;
use error::fmt_git_err;
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use route::{handle_route, parse_hash, set_text};
use stats::{format_stats, set_stats_loaded};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::Document;

// ---------------------------------------------------------------------------
// Initial repo load
// ---------------------------------------------------------------------------

async fn load_repo(url: String, doc: Document) {
    let output = match doc.get_element_by_id("output") {
        Some(el) => el,
        None => return,
    };

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
    let repo = CachingRepo::open(repo).await;

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

    let commit = match repo.peel_ref_to_commit(&head).await {
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
    set_stats_loaded(&doc);

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
            set_stats_loaded(&doc);
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
