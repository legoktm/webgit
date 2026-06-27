#![deny(clippy::all)]

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
use git_async::object::{Commit, Tree};
use render::about::AboutView;
use render::blob::BlobView;
use render::commit::CommitView;
use render::log::LogView;
use render::refs_all::RefsAllView;
use render::refs_heads::RefsHeadsView;
use render::refs_tags::RefsTagsView;
use render::summary::SummaryView;
use render::tag::TagView;
use render::tree::TreeView;
use route::{LoadedView, build_route, handle_route, set_text};
use stats::{format_stats, set_stats_loaded};
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::Document;
use yew::prelude::*;

fn console_log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

fn current_hash() -> String {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Initial repo load
// ---------------------------------------------------------------------------

/// Everything a route render needs from the loaded repository. Cheaply
/// cloneable (all `Rc`), so it can live in Yew state and be handed to
/// `handle_route`.
#[derive(Clone)]
struct RepoBundle {
    repo: Rc<CachingRepo>,
    head_commit: Rc<Commit>,
    root_tree: Rc<Tree>,
    clone_url: Rc<String>,
}

/// A bundle is created once per repository load and never mutated, so identity
/// (pointer equality on the `Rc`s) is the right equality for prop-diffing —
/// and avoids requiring `PartialEq` on `CachingRepo`/`Commit`/`Tree`.
impl PartialEq for RepoBundle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.repo, &other.repo)
            && Rc::ptr_eq(&self.head_commit, &other.head_commit)
            && Rc::ptr_eq(&self.root_tree, &other.root_tree)
            && Rc::ptr_eq(&self.clone_url, &other.clone_url)
    }
}

/// Open the repository, populate the header chrome, and assemble a
/// [`RepoBundle`]. Does not render a route — that's driven reactively from the
/// `App` route effect once the bundle lands in state.
async fn load_repo_bundle(url: String, doc: &Document) -> anyhow::Result<RepoBundle> {
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

    match repo.commit_graph_info() {
        Some((commits, bloom)) => console_log(&format!(
            "webgit: commit-graph present ({commits} commits, changed-path filters: {})",
            if bloom { "yes" } else { "no" }
        )),
        None => console_log("webgit: no commit-graph; walking commit objects directly"),
    }

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
    set_text(doc, "repo-path-name", &path);

    let root_tree = repo
        .lookup_object(commit.tree())
        .await
        .context("Failed to load root tree")?
        .tree()
        .map_err(|e| anyhow::anyhow!("Root object is not a tree: {e:?}"))?;

    Ok(RepoBundle {
        repo: Rc::new(repo),
        head_commit: Rc::new(commit),
        root_tree: Rc::new(root_tree),
        clone_url: Rc::new(url),
    })
}

// ---------------------------------------------------------------------------
// Root component
// ---------------------------------------------------------------------------

/// The single Yew root. It renders the static application shell, loads the
/// repository into state, and re-runs the route renderer whenever the location
/// hash changes.
#[function_component(App)]
fn app() -> Html {
    // The loaded repository, or `None` while loading / on the repo index page.
    let bundle = use_state(|| None::<RepoBundle>);
    // Current location hash; updated by the hashchange listener below. This is
    // the "router": the route effect re-parses it via `route::parse_hash`.
    let hash = use_state(current_hash);

    // Mount: register the hashchange listener and kick off the repo load (or
    // the repository index when the URL doesn't name a repo).
    {
        let bundle = bundle.clone();
        let hash = hash.clone();
        use_effect_with((), move |_| {
            let window = web_sys::window().expect("no window");

            let hash_setter = hash.clone();
            let on_hash = Closure::<dyn Fn()>::new(move || hash_setter.set(current_hash()));
            window
                .add_event_listener_with_callback("hashchange", on_hash.as_ref().unchecked_ref())
                .expect("failed to add hashchange listener");

            wasm_bindgen_futures::spawn_local(async move {
                let window = web_sys::window().expect("no window");
                let doc = window.document().expect("no document");
                match resolve_repo_url(&window) {
                    Some(url) => {
                        let output = doc.get_element_by_id("output").unwrap();
                        match load_repo_bundle(url, &doc).await {
                            Ok(b) => bundle.set(Some(b)),
                            Err(e) => output.set_inner_html(&error_html(&format!("{e:#}"))),
                        }
                    }
                    None => load_index(doc).await,
                }
            });

            move || drop(on_hash)
        });
    }

    // Update the chrome (nav active tab, nav hrefs, path bar) whenever the hash
    // changes or the repo finishes loading. The route *content* is rendered by
    // `RouteView` below; this only touches the static shell, which Yew never
    // re-diffs.
    {
        let bundle = bundle.clone();
        use_effect_with(((*hash).clone(), bundle.is_some()), move |(hash, _)| {
            if let Some(b) = (*bundle).clone() {
                let hash = hash.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let doc = web_sys::window().unwrap().document().unwrap();
                    handle_route(hash, &b.repo, &doc).await;
                    set_stats_loaded(&doc);
                });
            }
            || ()
        });
    }

    html! {
        <>
            <div id="idb-warning" class="hide">
                { "IndexedDB is unavailable, so object caching is disabled. \
                   Pages will load more slowly and re-fetch data on every navigation." }
            </div>

            <div id="header">
                <div id="header-sub">
                    <h1 id="repo-path"><span id="repo-path-name">{ "\u{2014}" }</span></h1>
                    <div id="repo-desc">{ "\u{00a0}" }</div>
                </div>
            </div>

            <nav id="nav">
                <a href="#!/summary" class="nav-tab">{ "summary" }</a>
                <a href="#!/refs" class="nav-tab">{ "refs" }</a>
                <a href="#!/log" class="nav-tab">{ "log" }</a>
                <a href="#!/tree" class="nav-tab">{ "tree" }</a>
                <a href="#!/commit" class="nav-tab">{ "commit" }</a>
                <a href="#!/about" class="nav-tab">{ "about" }</a>
            </nav>

            <div id="fetch-stats"></div>
            <div id="path-bar" class="hide"></div>

            <div id="content">
                {
                    // Once a repo is loaded, its content is a real Yew subtree
                    // (`RouteView`). Until then — while loading, or on the repo
                    // index / error paths — `#output` stays an imperative mount
                    // point used by the loaders below.
                    match (*bundle).clone() {
                        Some(b) => html! { <RouteView bundle={b} hash={(*hash).clone()} /> },
                        None => html! { <div id="output"></div> },
                    }
                }
            </div>
        </>
    }
}

/// Props for [`RouteView`]: the loaded repository and the current location hash.
#[derive(Properties, PartialEq, Clone)]
struct RouteViewProps {
    bundle: RepoBundle,
    hash: String,
}

/// Renders the content for the current route as a child of the single Yew tree.
/// An effect (re-run on hash/repo change) resolves the route's data via
/// [`build_route`] into local state; the markup is a `match` over the result,
/// with loading and error states. Replaces the old per-route mount-and-leak.
#[function_component(RouteView)]
fn route_view(props: &RouteViewProps) -> Html {
    let RouteViewProps { bundle, hash } = props;
    // `None` while resolving; `Some(Ok)` rendered; `Some(Err)` an error message.
    let loaded = use_state(|| None::<Result<LoadedView, String>>);

    {
        let loaded = loaded.clone();
        use_effect_with(
            (hash.clone(), bundle.clone()),
            move |(hash, bundle): &(String, RepoBundle)| {
                loaded.set(None);
                // Guard against a stale in-flight resolution overwriting a newer
                // one: navigating away flips the flag in the cleanup closure.
                let cancelled = Rc::new(Cell::new(false));
                {
                    let (loaded, bundle, hash, cancelled) = (
                        loaded.clone(),
                        bundle.clone(),
                        hash.clone(),
                        cancelled.clone(),
                    );
                    wasm_bindgen_futures::spawn_local(async move {
                        let result = build_route(
                            &hash,
                            &bundle.head_commit,
                            &bundle.root_tree,
                            &bundle.repo,
                            &bundle.clone_url,
                        )
                        .await
                        .map_err(|e| format!("{e:#}"));
                        if !cancelled.get() {
                            loaded.set(Some(result));
                        }
                    });
                }
                move || cancelled.set(true)
            },
        );
    }

    match &*loaded {
        None => html! { <p class="msg">{ "Loading\u{2026}" }</p> },
        Some(Err(e)) => html! { <p class="msg error">{ e.clone() }</p> },
        Some(Ok(view)) => render_loaded(view),
    }
}

/// Render a resolved [`LoadedView`] by handing its props to the matching view
/// component (the props spread `..p` provides every field at once).
fn render_loaded(view: &LoadedView) -> Html {
    match view {
        LoadedView::About(p) => html! { <AboutView ..p.clone() /> },
        LoadedView::Summary(p) => html! { <SummaryView ..p.clone() /> },
        LoadedView::Log(p) => html! { <LogView ..p.clone() /> },
        LoadedView::Commit(p) => html! { <CommitView ..p.clone() /> },
        LoadedView::RefsHeads(p) => html! { <RefsHeadsView ..p.clone() /> },
        LoadedView::RefsTags(p) => html! { <RefsTagsView ..p.clone() /> },
        LoadedView::RefsAll(p) => html! { <RefsAllView ..p.clone() /> },
        LoadedView::Tag(p) => html! { <TagView ..p.clone() /> },
        LoadedView::Tree(p) => html! { <TreeView ..p.clone() /> },
        LoadedView::Blob(p) => html! { <BlobView ..p.clone() /> },
        LoadedView::NotFound(path) => html! {
            <p class="msg error">{ "Not found: " }<code>{ path.clone() }</code></p>
        },
    }
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
    render::listing::render_listing(paths, output)
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
    let on_loopback = location
        .hostname()
        .map(|h| h == "127.0.0.1" || h == "localhost")
        .unwrap_or(false);
    if on_loopback && let Ok(search) = location.search() {
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
    yew::Renderer::<App>::new().render();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_path() {
        assert_eq!(repo_path("https://example.org/foo/bar.git"), "foo/bar.git");
        assert_eq!(repo_path("https://example.org/foo/bar.git/"), "foo/bar.git");
        assert_eq!(repo_path("https://example.org/foo/bar"), "foo/bar");
        assert_eq!(
            repo_path("https://git.example.com/public/webgit.git/"),
            "public/webgit.git"
        );
        // No host component: the whole string is the path.
        assert_eq!(repo_path("bar.git"), "bar.git");
        assert_eq!(repo_path(""), "");
    }
}
