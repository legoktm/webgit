#![deny(clippy::all)]

mod cache;
mod error;
mod fetch;
mod fs;
mod render;
mod route;
mod stats;

use cache::CachingRepo;
use error::GitContext;
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use git_async::object::{Commit, Tree};
use render::about::AboutView;
use render::blob::BlobView;
use render::commit::CommitView;
use render::listing::{ListingProps, ListingView, build_listing_props};
use render::log::LogView;
use render::refs_all::RefsAllView;
use render::refs_heads::RefsHeadsView;
use render::refs_tags::RefsTagsView;
use render::summary::SummaryView;
use render::tag::TagView;
use render::tree::TreeView;
use route::{
    LoadedView, RefKind, Route, active_tab, build_route, log_url, parse_hash, resolve_display_head,
    set_text,
};
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
/// cloneable (all `Rc`), so it can live in Yew state and be passed as a prop to
/// `RouteView`.
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

/// What the content area (`#content`) is showing. The initial `Loading` is
/// replaced once the mount effect resolves the URL into a repo or the index.
enum Content {
    Loading,
    Repo(RepoBundle),
    Index(ListingProps),
    Error(String),
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
    // What the content area shows: loading, a loaded repo (→ `RouteView`), the
    // repository index, or a load error.
    let content = use_state(|| Content::Loading);
    // Current location hash; updated by the hashchange listener below. This is
    // the "router": the route effect re-parses it via `route::parse_hash`.
    let hash = use_state(current_hash);
    // Repo mode vs the repository index (no repo in the URL). Gates the
    // repo-scoped chrome (nav tabs, fetch stats).
    let is_repo = resolve_repo_url(&web_sys::window().expect("no window")).is_some();
    // The breadcrumb / ref-label shown in `#path-bar`, resolved per route.
    let path_bar = use_state(|| PathBar::Hidden);

    // Mount: register the hashchange listener and kick off the repo load (or
    // the repository index when the URL doesn't name a repo).
    {
        let content = content.clone();
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
                content.set(match resolve_repo_url(&window) {
                    Some(url) => match load_repo_bundle(url, &doc).await {
                        Ok(b) => Content::Repo(b),
                        Err(e) => Content::Error(format!("{e:#}")),
                    },
                    None => {
                        set_text(&doc, "repo-path-name", "repositories");
                        doc.set_title("repositories");
                        match load_listing().await {
                            Ok(props) => Content::Index(props),
                            Err(e) => Content::Error(format!("{e:#}")),
                        }
                    }
                });
            });

            move || drop(on_hash)
        });
    }

    // Resolve the path-bar model whenever the hash changes or the repo finishes
    // loading. The nav (active tab, hrefs) is derived synchronously by `NavBar`;
    // only the path bar's ref label needs an async ref lookup.
    {
        let content = content.clone();
        let path_bar = path_bar.clone();
        let repo_loaded = matches!(&*content, Content::Repo(_));
        use_effect_with(((*hash).clone(), repo_loaded), move |(hash, _)| {
            if let Content::Repo(b) = &*content {
                let repo = Rc::clone(&b.repo);
                let hash = hash.clone();
                let path_bar = path_bar.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    path_bar.set(compute_path_bar(&hash, &repo).await);
                    let doc = web_sys::window().unwrap().document().unwrap();
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

            if is_repo {
                <NavBar hash={(*hash).clone()} />
                <div id="fetch-stats"></div>
            }
            { render_path_bar(&path_bar) }

            <div id="content">
                {
                    match &*content {
                        Content::Loading => html! { <p class="msg">{ "Loading\u{2026}" }</p> },
                        Content::Repo(b) => {
                            html! { <RouteView bundle={b.clone()} hash={(*hash).clone()} /> }
                        }
                        Content::Index(props) => html! { <ListingView ..props.clone() /> },
                        Content::Error(e) => html! { <p class="msg error">{ e.clone() }</p> },
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
// Reactive chrome (nav + path bar)
// ---------------------------------------------------------------------------

/// Props for [`NavBar`]: the current location hash, from which the active tab
/// and the head-scoped log/tree hrefs are derived.
#[derive(Properties, PartialEq, Clone)]
struct NavBarProps {
    hash: String,
}

/// The repository nav tabs. Active highlight and the log/tree hrefs (scoped to
/// the current `?h=`/path) are derived synchronously from the route — replacing
/// the old imperative `set_active_tab` + `update_nav_for_head`.
#[function_component(NavBar)]
fn nav_bar(props: &NavBarProps) -> Html {
    let route = parse_hash(&props.hash);
    let (head, nav_path): (Option<&str>, &str) = match &route {
        Route::Log { head, path, .. } => (head.as_deref(), path.as_str()),
        Route::Tree { head, path } => (head.as_deref(), path.as_str()),
        _ => (None, ""),
    };
    let active = active_tab(&route);
    let log_href = log_url(nav_path, 0, head);
    let tree_href = match head {
        Some(h) => format!("#!/tree?h={h}"),
        None => "#!/tree".to_string(),
    };
    let tab = |base: &'static str, href: String, label: &'static str| -> Html {
        let class = if base == active {
            classes!("nav-tab", "active")
        } else {
            classes!("nav-tab")
        };
        html! { <a href={href} {class}>{ label }</a> }
    };
    html! {
        <nav id="nav">
            { tab("#!/summary", "#!/summary".to_string(), "summary") }
            { tab("#!/refs", "#!/refs".to_string(), "refs") }
            { tab("#!/log", log_href, "log") }
            { tab("#!/tree", tree_href, "tree") }
            { tab("#!/commit", "#!/commit".to_string(), "commit") }
            { tab("#!/about", "#!/about".to_string(), "about") }
        </nav>
    }
}

/// The `#path-bar` contents for the current route: a breadcrumb (tree, or
/// path-scoped log), a bare ref label (whole-history log on a ref), or hidden.
#[derive(Clone, PartialEq)]
enum PathBar {
    Hidden,
    Crumbs {
        display: Option<(String, RefKind)>,
        path: String,
        head: Option<String>,
    },
    RefOnly {
        name: String,
        kind: RefKind,
    },
}

/// Resolve the path-bar model for `hash`. Only the ref label needs an async ref
/// lookup; the breadcrumb itself is derived from the path.
async fn compute_path_bar(hash: &str, repo: &CachingRepo) -> PathBar {
    match parse_hash(hash) {
        Route::Tree { path, head } => {
            let display = resolve_display_head(repo, head.as_deref()).await;
            PathBar::Crumbs {
                display,
                path,
                head,
            }
        }
        Route::Log { path, head, .. } if !path.is_empty() => {
            let display = resolve_display_head(repo, head.as_deref()).await;
            PathBar::Crumbs {
                display,
                path,
                head,
            }
        }
        Route::Log { head, .. } => match resolve_display_head(repo, head.as_deref()).await {
            Some((name, kind)) => PathBar::RefOnly { name, kind },
            None => PathBar::Hidden,
        },
        _ => PathBar::Hidden,
    }
}

fn ref_label(kind: &RefKind) -> &'static str {
    match kind {
        RefKind::Tag => "tag",
        RefKind::Branch => "branch",
    }
}

fn render_path_bar(model: &PathBar) -> Html {
    match model {
        PathBar::Hidden => html! { <div id="path-bar" class="hide"></div> },
        PathBar::RefOnly { name, kind } => html! {
            <div id="path-bar">{ format!("{}: {}", ref_label(kind), name) }</div>
        },
        PathBar::Crumbs {
            display,
            path,
            head,
        } => {
            let head_suffix = head.as_deref().map_or(String::new(), |h| format!("?h={h}"));
            let root_href = format!("#!/tree{head_suffix}");
            html! {
                <div id="path-bar">
                    if let Some((name, kind)) = display {
                        { format!("{}: {} | ", ref_label(kind), name) }
                    }
                    { "path: " }
                    <a href={root_href}>{ "root" }</a>
                    { for crumb_links(path, &head_suffix) }
                </div>
            }
        }
    }
}

/// The `" / <segment>"` breadcrumb links after the root, each linking to the
/// cumulative tree path (with the same `?h=` suffix).
fn crumb_links(path: &str, head_suffix: &str) -> Vec<Html> {
    let mut cumulative = String::new();
    let mut out = Vec::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(component);
        let href = format!("#!/tree/{cumulative}{head_suffix}");
        out.push(html! { <>{ " / " }<a href={href}>{ component.to_string() }</a></> });
    }
    out
}

// ---------------------------------------------------------------------------
// Repository index
// ---------------------------------------------------------------------------

/// Shown when the URL doesn't name a repository: fetch and parse the server's
/// `/listing.json` into the repository-index props.
async fn load_listing() -> anyhow::Result<ListingProps> {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    let url = format!("{origin}/listing.json");
    let text = fetch::fetch_text(&url)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load {url}: {e:?}"))?;
    let paths: Vec<String> = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse listing.json: {e}"))?;
    Ok(build_listing_props(paths))
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

    // --- Reactive chrome snapshots (see `render::tag` for the SSR approach) ---

    /// Render `NavBar` for `hash` to a static HTML string, breaking adjacent
    /// tags onto their own lines. Exercises the active-tab highlight and the
    /// head/path-scoped log & tree hrefs.
    fn render_nav(hash: &str) -> String {
        let hash = hash.to_string();
        let html = futures::executor::block_on(
            yew::ServerRenderer::<NavBar>::with_props(move || NavBarProps { hash })
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn nav_summary() {
        // Plain route: summary active, log/tree hrefs at their defaults.
        insta::assert_snapshot!(render_nav("#!/summary"));
    }

    #[test]
    fn nav_tree_scoped() {
        // A subtree on a branch: tree active, and both the log and tree tabs
        // carry the current path / `?h=`.
        insta::assert_snapshot!(render_nav("#!/tree/src?h=main"));
    }

    #[test]
    fn nav_log_on_tag() {
        // Whole-history log on a tag: log active, hrefs scoped to the ref.
        insta::assert_snapshot!(render_nav("#!/log?h=v1.0.0"));
    }

    // A throwaway host so the plain `render_path_bar` fn can go through SSR
    // (a renderer needs a component, and `PathBar` itself isn't `Properties`).
    #[derive(Properties, PartialEq, Clone)]
    struct PbHostProps {
        model: PathBar,
    }

    #[function_component(PbHost)]
    fn pb_host(props: &PbHostProps) -> Html {
        render_path_bar(&props.model)
    }

    fn render_pb(model: PathBar) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<PbHost>::with_props(move || PbHostProps { model })
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn path_bar_hidden() {
        insta::assert_snapshot!(render_pb(PathBar::Hidden));
    }

    #[test]
    fn path_bar_crumbs_on_branch() {
        // Nested path on a branch: ref label prefix + root/segment breadcrumb,
        // every link carrying `?h=`.
        insta::assert_snapshot!(render_pb(PathBar::Crumbs {
            display: Some(("main".to_string(), RefKind::Branch)),
            path: "src/render".to_string(),
            head: Some("main".to_string()),
        }));
    }

    #[test]
    fn path_bar_crumbs_no_ref() {
        // Implicit HEAD (no `?h=`): no ref label, no suffix on the hrefs.
        insta::assert_snapshot!(render_pb(PathBar::Crumbs {
            display: None,
            path: "src".to_string(),
            head: None,
        }));
    }

    #[test]
    fn path_bar_ref_only() {
        insta::assert_snapshot!(render_pb(PathBar::RefOnly {
            name: "v1.0.0".to_string(),
            kind: RefKind::Tag,
        }));
    }
}
