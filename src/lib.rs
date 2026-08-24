#![deny(clippy::all)]

mod archive;
mod assets;
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
use gib::Repo;
use gib::object::{Commit, Tree, TreeEntryType};
use gib_mailmap::Mailmap;
use render::about::{AboutProps, AboutView, build_index_about};
use render::blame::BlameView;
use render::blob::BlobView;
use render::commit::CommitView;
use render::listing::{ListingProps, ListingView, parse_listing};
use render::log::LogView;
use render::readme::ReadmeView;
use render::refs_all::RefsAllView;
use render::refs_heads::RefsHeadsView;
use render::refs_tags::RefsTagsView;
use render::snapshot::SnapshotView;
use render::summary::SummaryView;
use render::tag::TagView;
use render::tree::TreeView;
use route::{
    IndexRoute, LineRange, LoadedView, RefKind, Route, active_tab, build_route, encode_component,
    index_url, log_url, parse_hash, parse_index_hash, resolve_display_head,
};
use stats::format_stats;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
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
pub(crate) struct RepoBundle {
    pub(crate) repo: Rc<CachingRepo>,
    pub(crate) head_commit: Rc<Commit>,
    pub(crate) root_tree: Rc<Tree>,
    pub(crate) mailmap: Rc<Mailmap>,
    pub(crate) clone_url: Rc<String>,
    /// The repository's name, resolved once from the URL — what snapshots are
    /// named after. See [`repo_name`].
    pub(crate) repo_name: Rc<String>,
}

/// A bundle is created once per repository load and never mutated, so identity
/// (pointer equality on the `Rc`s) is the right equality for prop-diffing —
/// and avoids requiring `PartialEq` on `CachingRepo`/`Commit`/`Tree`.
impl PartialEq for RepoBundle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.repo, &other.repo)
            && Rc::ptr_eq(&self.head_commit, &other.head_commit)
            && Rc::ptr_eq(&self.root_tree, &other.root_tree)
            && Rc::ptr_eq(&self.mailmap, &other.mailmap)
            && Rc::ptr_eq(&self.clone_url, &other.clone_url)
            && Rc::ptr_eq(&self.repo_name, &other.repo_name)
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

/// Open the repository and assemble a [`RepoBundle`]. Touches no DOM and renders
/// no route — the header, the IndexedDB banner, and the route content are all
/// derived reactively from the resulting `Content` state; fetch progress is
/// shown by the [`FetchStats`] component.
async fn load_repo_bundle(url: String) -> anyhow::Result<RepoBundle> {
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

    let head = repo.head().await.context("Failed to read HEAD")?;

    let commit = repo
        .peel_ref_to_commit(&head)
        .await
        .context("Failed to peel HEAD to commit")?
        .ok_or_else(|| anyhow::anyhow!("HEAD does not point to a commit"))?;

    let root_tree = repo
        .lookup_object(commit.tree())
        .await
        .context("Failed to load root tree")?
        .tree()
        .map_err(|e| anyhow::anyhow!("Root object is not a tree: {e:?}"))?;

    let mailmap = load_mailmap(&root_tree, &repo).await;

    Ok(RepoBundle {
        repo: Rc::new(repo),
        head_commit: Rc::new(commit),
        root_tree: Rc::new(root_tree),
        mailmap: Rc::new(mailmap),
        repo_name: Rc::new(repo_name(&url)),
        clone_url: Rc::new(url),
    })
}

/// Read `HEAD:.mailmap`
async fn load_mailmap(root_tree: &Tree, repo: &CachingRepo) -> Mailmap {
    let Some(entry) = root_tree
        .entries()
        .find(|e| e.name() == gib_mailmap::MAILMAP.as_bytes())
    else {
        return Mailmap::default();
    };
    if !matches!(
        entry.entry_type(),
        TreeEntryType::File | TreeEntryType::Executable
    ) {
        return Mailmap::default();
    }
    match repo.lookup_object(entry.id()).await {
        Ok(object) => match object.blob() {
            Ok(blob) => Mailmap::parse(blob.data()),
            Err(_) => Mailmap::default(),
        },
        Err(e) => {
            console_log(&format!("webgit: could not read .mailmap: {e:?}"));
            Mailmap::default()
        }
    }
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
                content.set(match resolve_repo_url(&window) {
                    Some(url) => match load_repo_bundle(url).await {
                        Ok(b) => Content::Repo(b),
                        Err(e) => Content::Error(format!("{e:#}")),
                    },
                    None => match load_listing().await {
                        Ok(props) => Content::Index(props),
                        Err(e) => Content::Error(format!("{e:#}")),
                    },
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
            // Same staleness guard as `RouteView`: the ref lookup is async, so
            // navigating again before it lands must not let the old route's
            // path bar overwrite the new one. The cleanup closure runs on the
            // next hash change, flipping the flag the resolution checks.
            let cancelled = Rc::new(Cell::new(false));
            if let Content::Repo(b) = &*content {
                let repo = Rc::clone(&b.repo);
                let hash = hash.clone();
                let path_bar = path_bar.clone();
                let cancelled = cancelled.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let model = compute_path_bar(&hash, &repo).await;
                    if !cancelled.get() {
                        path_bar.set(model);
                    }
                });
            }
            move || cancelled.set(true)
        });
    }

    // The repository display name (and document title): the repo path, or
    // "repositories" for the index; `None` while loading or on error.
    let doc_name: Option<String> = match &*content {
        Content::Repo(b) => Some(repo_path(&b.clone_url)),
        Content::Index(_) => Some("repositories".to_string()),
        Content::Loading | Content::Error(_) => None,
    };
    // `#!/index/<section>` names a section heading of the listing, which only
    // exists once the (async) listing has mounted — and which the browser would
    // not scroll to on its own regardless, the hash not being the heading's id.
    {
        let listing_loaded = matches!(&*content, Content::Index(_));
        let hash = (*hash).clone();
        use_effect_with((listing_loaded, hash), move |(listing_loaded, hash)| {
            if *listing_loaded
                && let IndexRoute::Listing { section } = parse_index_hash(hash)
                && !section.is_empty()
                && let Some(document) = web_sys::window().and_then(|window| window.document())
                && let Some(target) = document.get_element_by_id(&section)
            {
                target.scroll_into_view();
            }
            || ()
        });
    }

    let doc_name_node = doc_name.clone().map(|name| {
        if let Some((prefix, suffix)) = name.split_once("/") {
            let prefix = prefix.to_owned();
            let suffix = suffix.to_owned();
            html! {
                <>
                    <a href={ format!("/{}", index_url(&prefix)) }>{ prefix }</a>
                    { " / " }
                    <span>{ suffix }</span>
                </>
            }
        } else {
            html! { name }
        }
    });
    // `<title>` lives in `<head>`, outside the Yew root, so it's the one thing
    // still set imperatively — but now declaratively, keyed off the name.
    {
        let doc_name = doc_name.clone();
        use_effect_with(doc_name, |name| {
            if let Some(name) = name
                && let Some(doc) = web_sys::window().and_then(|w| w.document())
            {
                doc.set_title(name);
            }
            || ()
        });
    }
    let idb_unavailable = matches!(&*content, Content::Repo(b) if !b.repo.idb_available());

    html! {
        <>
            <div id="idb-warning" class={classes!((!idb_unavailable).then_some("hide"))}>
                { "IndexedDB is unavailable, so object caching is disabled. \
                   Pages will load more slowly and re-fetch data on every navigation." }
            </div>

            <div id="header">
                <div id="header-sub">
                    <h1 id="repo-path">
                        <span id="repo-path-name">
                            { doc_name_node.clone().unwrap_or_else(|| html!{ "\u{2014}" }) }
                        </span>
                    </h1>
                    <div id="repo-desc">{ "\u{00a0}" }</div>
                </div>
            </div>

            if is_repo {
                <NavBar hash={(*hash).clone()} />
                <FetchStats />
            } else {
                <IndexNavBar hash={(*hash).clone()} />
            }
            { render_path_bar(&path_bar) }

            <div id="content">
                {
                    match &*content {
                        Content::Loading => html! { <p class="msg">{ render::loading_dots() }</p> },
                        Content::Repo(b) => {
                            html! { <RouteView bundle={b.clone()} hash={(*hash).clone()} /> }
                        }
                        Content::Index(props) => html! {
                            <IndexView listing={props.clone()} hash={(*hash).clone()} />
                        },
                        Content::Error(e) => html! { <p class="msg error">{ e.clone() }</p> },
                    }
                }
            </div>
        </>
    }
}

/// Props for [`IndexView`]: the parsed listing, and the hash deciding which of
/// the index's two tabs is showing.
#[derive(Properties, PartialEq, Clone)]
struct IndexViewProps {
    listing: ListingProps,
    hash: String,
}

/// The repository index's content: the listing, or the about page when its tab
/// is selected. There is no repository to build a route against, so this is a
/// plain two-way switch rather than a [`RouteView`].
#[function_component(IndexView)]
fn index_view(props: &IndexViewProps) -> Html {
    if matches!(parse_index_hash(&props.hash), IndexRoute::About) {
        html! { <IndexAboutView /> }
    } else {
        html! { <ListingView ..props.listing.clone() /> }
    }
}

/// The about page with no repository behind it. Its figures come from
/// IndexedDB, so they're resolved on mount — in a component of its own, so the
/// hooks that do it don't run while the listing is what's showing.
#[function_component(IndexAboutView)]
fn index_about_view() -> Html {
    let props = use_state(|| None::<AboutProps>);
    {
        let props = props.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                props.set(Some(build_index_about().await));
            });
            || ()
        });
    }
    match &*props {
        Some(p) => html! { <AboutView ..p.clone() /> },
        None => html! { <p class="msg">{ render::loading_dots() }</p> },
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
    let (route, lines) = route::split_line_anchor(hash);
    let route = route.to_string();
    // `None` while resolving; `Some(Ok)` rendered; `Some(Err)` an error message.
    let loaded = use_state(|| None::<Result<LoadedView, String>>);

    {
        let loaded = loaded.clone();
        use_effect_with(
            (route, bundle.clone()),
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
                        // Render each partial as it streams in (log pages fill
                        // in, commit diffs grow file-by-file). The same cancel
                        // guard drops stale partials from a superseded route.
                        let emit = {
                            let (loaded, cancelled) = (loaded.clone(), cancelled.clone());
                            move |view: LoadedView| {
                                if !cancelled.get() {
                                    loaded.set(Some(Ok(view)));
                                }
                            }
                        };
                        let result = build_route(&hash, &bundle, &emit)
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
        None => html! { <p class="msg">{ render::loading_dots() }</p> },
        Some(Err(e)) => html! { <p class="msg error">{ e.clone() }</p> },
        Some(Ok(view)) => render_loaded(view, lines),
    }
}

/// Render a resolved [`LoadedView`] by handing its props to the matching view
/// component (the props spread `..p` provides every field at once).
fn render_loaded(view: &LoadedView, lines: Option<LineRange>) -> Html {
    match view {
        LoadedView::About(p) => html! { <AboutView ..p.clone() /> },
        LoadedView::Readme(p) => html! { <ReadmeView ..p.clone() /> },
        LoadedView::Summary(p) => html! { <SummaryView ..p.clone() /> },
        LoadedView::Log(p) => html! { <LogView ..p.clone() /> },
        LoadedView::Commit(p) => html! { <CommitView ..(**p).clone() /> },
        LoadedView::RefsHeads(p) => html! { <RefsHeadsView ..p.clone() /> },
        LoadedView::RefsTags(p) => html! { <RefsTagsView ..p.clone() /> },
        LoadedView::RefsAll(p) => html! { <RefsAllView ..p.clone() /> },
        LoadedView::Tag(p) => html! { <TagView ..p.clone() /> },
        LoadedView::Tree(p) => html! { <TreeView ..p.clone() /> },
        LoadedView::Blob(p) => {
            let mut p = p.clone();
            p.lines = lines;
            html! { <BlobView ..p /> }
        }
        LoadedView::Blame(p) => html! { <BlameView ..(**p).clone() /> },
        LoadedView::Snapshot(p) => html! { <SnapshotView ..p.clone() /> },
        LoadedView::NotFound(path) => html! {
            <p class="msg error">{ "Not found: " }<code>{ path.clone() }</code></p>
        },
    }
}

// ---------------------------------------------------------------------------
// Reactive chrome (nav + path bar + fetch stats)
// ---------------------------------------------------------------------------

/// How long (ms) the fetch line waits with no new fetches before relabelling
/// "Loading…" → "Loaded".
const STATS_SETTLE_MS: i32 = 200;

/// The pending "settle" timer: its `setTimeout` handle (to cancel/re-arm) and
/// the closure it runs, kept alive alongside it. Shared so each progress tick
/// can replace it.
type SettleTimer = Rc<RefCell<Option<(i32, Closure<dyn FnMut()>)>>>;

/// The persistent fetch-progress line. It subscribes to the fetch layer's
/// progress callback and renders the running totals; after [`STATS_SETTLE_MS`]
/// with no further fetches it relabels "Loading…" → "Loaded". Fully
/// self-contained — nothing else writes to it (replacing the old imperative
/// `#fetch-stats` text + the `set_stats_loaded` call wedged into the path-bar
/// effect).
#[function_component(FetchStats)]
fn fetch_stats() -> Html {
    let text = use_state(String::new);

    {
        let text = text.clone();
        use_effect_with((), move |_| {
            let window = web_sys::window().expect("no window");
            // Debounce slot: each progress tick cancels the pending "settle" and
            // re-arms it. Holds the timer id and its closure, which must outlive
            // the `set_timeout` call.
            let settle: SettleTimer = Rc::new(RefCell::new(None));

            let on_progress = {
                let text = text.clone();
                let settle = settle.clone();
                let window = window.clone();
                move |reqs, bytes, cached| {
                    text.set(format_stats("Loading\u{2026}", reqs, bytes, cached));
                    if let Some((id, _)) = settle.borrow_mut().take() {
                        window.clear_timeout_with_handle(id);
                    }
                    let settle_cb = {
                        let text = text.clone();
                        Closure::<dyn FnMut()>::new(move || {
                            let (r, b, c) = fetch::fetch_stats();
                            text.set(format_stats("Loaded", r, b, c));
                        })
                    };
                    let id = window
                        .set_timeout_with_callback_and_timeout_and_arguments_0(
                            settle_cb.as_ref().unchecked_ref(),
                            STATS_SETTLE_MS,
                        )
                        .unwrap_or(0);
                    *settle.borrow_mut() = Some((id, settle_cb));
                }
            };
            fetch::reset_and_watch(Box::new(on_progress));

            move || {
                fetch::clear_watch();
                if let Some((id, _)) = settle.borrow_mut().take()
                    && let Some(w) = web_sys::window()
                {
                    w.clear_timeout_with_handle(id);
                }
            }
        });
    }

    html! { <div id="fetch-stats">{ (*text).clone() }</div> }
}

/// Props for [`NavBar`] and [`IndexNavBar`]: the current location hash, from
/// which the active tab and the head-scoped log/tree hrefs are derived.
#[derive(Properties, PartialEq, Clone)]
struct NavBarProps {
    hash: String,
}

/// One nav tab, lit when it's the one the current route lives under.
fn nav_tab(href: String, label: &'static str, active: bool) -> Html {
    let class = if active {
        classes!("nav-tab", "active")
    } else {
        classes!("nav-tab")
    };
    html! { <a href={href} {class}>{ label }</a> }
}

/// The repository nav tabs. Active highlight and the log/tree hrefs (scoped to
/// the current `?h=`/path) are derived synchronously from the route — replacing
/// the old imperative `set_active_tab` + `update_nav_for_head`.
#[function_component(NavBar)]
fn nav_bar(props: &NavBarProps) -> Html {
    let route = parse_hash(&props.hash);
    let (head, nav_path): (Option<&str>, &str) = match &route {
        Route::Log { head, path, .. } => (head.as_deref(), path.as_str()),
        Route::Tree { head, path, .. } => (head.as_deref(), path.as_str()),
        _ => (None, ""),
    };
    // The log tab keeps an expanded log expanded, the way it keeps the `?h=`
    // and path it was opened with; from anywhere else it starts collapsed.
    let showmsg = matches!(route, Route::Log { showmsg: true, .. });
    let active = active_tab(&route);
    let log_href = log_url(nav_path, 0, head, showmsg);
    let tree_href = match head {
        Some(h) => format!("#!/tree?h={}", encode_component(h)),
        None => "#!/tree".to_string(),
    };
    let tab = |base: &'static str, href: String, label: &'static str| -> Html {
        nav_tab(href, label, base == active)
    };
    html! {
        <nav id="nav">
            { tab("#!/readme", "#!/readme".to_string(), "readme") }
            { tab("#!/summary", "#!/summary".to_string(), "summary") }
            { tab("#!/refs", "#!/refs".to_string(), "refs") }
            { tab("#!/log", log_href, "log") }
            { tab("#!/tree", tree_href, "tree") }
            { tab("#!/commit", "#!/commit".to_string(), "commit") }
            { tab("#!/about", "#!/about".to_string(), "about") }
        </nav>
    }
}

/// The repository index's nav: the listing itself, and the about page's
/// viewer-wide half. Its own component rather than a mode of [`NavBar`], whose
/// every tab is a route inside a repository.
#[function_component(IndexNavBar)]
fn index_nav_bar(props: &NavBarProps) -> Html {
    let about = matches!(parse_index_hash(&props.hash), IndexRoute::About);
    html! {
        <nav id="nav">
            { nav_tab(index_url(""), "index", !about) }
            { nav_tab("#!/about".to_string(), "about", about) }
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
        Route::Tree { path, head, .. } => {
            let display = resolve_display_head(repo, head.as_deref()).await;
            PathBar::Crumbs {
                display,
                path,
                head,
            }
        }
        // Blame is a way of reading one file in the tree, so it gets the
        // tree's breadcrumb: the trail back out of the file is the same one.
        Route::Blame { path, head } => {
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
        RefKind::Commit => "commit",
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
            let head_suffix = head
                .as_deref()
                .map_or(String::new(), |h| format!("?h={}", encode_component(h)));
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
/// cumulative tree path (with the same `?h=` suffix). The link text is the
/// component as it really is; only the href is encoded.
fn crumb_links(path: &str, head_suffix: &str) -> Vec<Html> {
    let mut cumulative = String::new();
    let mut out = Vec::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(&encode_component(component));
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
    parse_listing(&text).map_err(|e| anyhow::anyhow!("Failed to parse listing.json: {e}"))
}

// ---------------------------------------------------------------------------
// URL resolution
// ---------------------------------------------------------------------------

/// The repository's own name, from its URL: the last path component, without
/// the `.git` suffix — `…/public/webgit.git/` becomes `webgit`.
///
/// Resolved once when the repository is opened and carried on [`RepoBundle`],
/// since it is a property of the repository rather than of any one view. The
/// snapshot links and the archives they build are named from it; the full URL
/// stays reserved for the two places that actually show a URL, the summary's
/// `git clone` line and the about page.
fn repo_name(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        "repository".to_string()
    } else {
        name.to_string()
    }
}

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
    assets::init();
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

    #[test]
    fn test_repo_name() {
        assert_eq!(repo_name("https://example.org/public/webgit.git"), "webgit");
        assert_eq!(
            repo_name("https://example.org/public/webgit.git/"),
            "webgit"
        );
        assert_eq!(repo_name("https://example.org/public/webgit"), "webgit");
        assert_eq!(repo_name("webgit.git"), "webgit");
        // Nothing left to name it after.
        assert_eq!(repo_name("https://example.org/"), "example.org");
        assert_eq!(repo_name(""), "repository");
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

    /// The repository index's nav, which has only the two tabs.
    fn render_index_nav(hash: &str) -> String {
        let hash = hash.to_string();
        let html = futures::executor::block_on(
            yew::ServerRenderer::<IndexNavBar>::with_props(move || NavBarProps { hash })
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn nav_index() {
        // The listing, reached by its own tab...
        insta::assert_snapshot!(render_index_nav(&index_url("")));
    }

    #[test]
    fn nav_index_section_anchor() {
        // ...and scrolled to a section, which is still the listing.
        insta::assert_snapshot!(render_index_nav(&index_url("public")));
    }

    #[test]
    fn nav_index_about() {
        insta::assert_snapshot!(render_index_nav("#!/about"));
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

    /// A `?h=` that named a commit outright: labelled "commit", and by the short
    /// hash — the full 40 characters stay in the hrefs, which is where they are
    /// load-bearing.
    #[test]
    fn path_bar_crumbs_on_commit() {
        insta::assert_snapshot!(render_pb(PathBar::Crumbs {
            display: Some(("6121d0b9".to_string(), RefKind::Commit)),
            path: "src".to_string(),
            head: Some("6121d0b97779278fcc32cc8a02754e7c588d9c18".to_string()),
        }));
    }

    #[test]
    fn path_bar_ref_only_on_commit() {
        insta::assert_snapshot!(render_pb(PathBar::RefOnly {
            name: "6121d0b9".to_string(),
            kind: RefKind::Commit,
        }));
    }
}
