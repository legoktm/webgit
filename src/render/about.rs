use crate::cache::{CachingRepo, GlobalCache};
use crate::import::import_bundle;
use crate::render::{collect_refs, file_from_event};
use gib::reference::{RefName, RefTarget};
use git_version::git_version;
use std::cell::Cell;
use std::rc::Rc;
use yew::prelude::*;

pub(crate) const COMMIT: &str = git_version!();

/// The "repository" section of the about page. `None` on the repository
/// index's about page, which names no repository to describe — only the
/// viewer-wide half below it applies there.
#[derive(PartialEq, Clone)]
pub(crate) struct AboutRepo {
    pub clone_url: String,
    pub head_branch: String,
    pub branch_count: usize,
    pub tag_count: usize,
}

/// How far an import of a bundle has got. The view holds one of these; the
/// work itself reports into it through the callback `on_import` was handed.
#[derive(PartialEq, Clone, Debug)]
pub(crate) enum ImportState {
    /// No bundle has been picked yet, or the page has just loaded.
    Idle,
    /// The file is being read into memory, before any of it is understood.
    Reading,
    /// Objects are going into the cache: how many of the bundle's, out of how
    /// many it holds.
    Storing { done: usize, total: usize },
    /// Finished: what the bundle turned out to hold, and the cache figures to
    /// redraw the row above with, since it has just grown.
    Done {
        objects: usize,
        refs: usize,
        cached: (usize, String),
    },
    /// The file wasn't a bundle, or stopped being readable partway through.
    Failed(String),
}

/// The view inputs for the about page. The `on_clear` callback is wired to the
/// "(clear)" button so the cache can be flushed and the page re-rendered; it's
/// unused when caching is unavailable (no button is shown).
///
/// `on_import` is the same arrangement in the other direction: it's handed the
/// file the user picked, along with a callback to report back through, because
/// reading a bundle takes long enough that the page has to show it happening.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct AboutProps {
    pub repo: Option<AboutRepo>,
    pub idb_available: bool,
    pub objects: usize,
    pub size_mb: String,
    pub commit: String,
    pub on_clear: Callback<MouseEvent>,
    pub on_import: Callback<(web_sys::File, Callback<ImportState>)>,
}

/// The Yew component used to mount the about view into the DOM. Unlike the
/// other ported views, the markup isn't split into a plain function: it holds
/// `use_state` (so it must run as a component), and its tests render it through
/// SSR anyway, so a renderer-free markup fn would buy nothing.
#[function_component(AboutView)]
pub(crate) fn about_view(props: &AboutProps) -> Html {
    let AboutProps {
        repo,
        idb_available,
        objects,
        size_mb,
        commit,
        on_clear,
        on_import,
    } = props;

    // The cached-objects figure is the only thing that changes when the cache
    // is cleared, so keep it in local state and update just it — Yew diffs that
    // text node rather than re-rendering (and re-fetching) the whole page.
    // `clear_cache` empties the object store, so 0 / 0.00 MB is exactly what a
    // refetch would report.
    let stats = use_state(|| (*objects, size_mb.clone()));
    let on_clear = {
        let on_clear = on_clear.clone();
        let stats = stats.clone();
        Callback::from(move |e: MouseEvent| {
            on_clear.emit(e);
            stats.set((0, "0.00".to_string()));
        })
    };
    let (objects, size_mb) = &*stats;

    // An import reports into the same pair: its own progress, and — once it is
    // done — the cached-objects row, which has just grown by everything the
    // bundle carried.
    let import = use_state(|| ImportState::Idle);
    let on_file = {
        let on_import = on_import.clone();
        let import = import.clone();
        let stats = stats.clone();
        Callback::from(move |e: Event| {
            let Some(file) = file_from_event(&e) else {
                return;
            };
            import.set(ImportState::Reading);
            let report = {
                let import = import.clone();
                let stats = stats.clone();
                Callback::from(move |state: ImportState| {
                    if let ImportState::Done { cached, .. } = &state {
                        stats.set(cached.clone());
                    }
                    import.set(state);
                })
            };
            on_import.emit((file, report));
        })
    };

    html! {
        <>
            if let Some(repo) = repo {
                <h3 class="summary-heading">{ "repository" }</h3>
                <table class="tag-table">
                    <tbody>
                        <tr>
                            <td class="label">{ "clone URL" }</td>
                            <td class="mono">{ repo.clone_url.clone() }</td>
                        </tr>
                        <tr>
                            <td class="label">{ "HEAD branch" }</td>
                            <td>{ repo.head_branch.clone() }</td>
                        </tr>
                        <tr>
                            <td class="label">{ "branches" }</td>
                            <td>{ repo.branch_count }</td>
                        </tr>
                        <tr>
                            <td class="label">{ "tags" }</td>
                            <td>{ repo.tag_count }</td>
                        </tr>
                    </tbody>
                </table>
            }

            <h3 class="summary-heading">{ "gib viewer" }</h3>
            <p>
                { "gib (\"git-in-browser\") allows you to view repositories entirely \
                   client-side. Objects are fetched as needed and stored in \
                   IndexedDB, similar to how " }
                <code>{ "git" }</code>
                { " normally works." }
                <br />
                { "The " }
                <a href="https://git.legoktm.com/public/webgit.git/">{ "source code" }</a>
                { " is available (" }<code>{ commit }</code>{ ")." }
            </p>
            <table class="tag-table">
                <tbody>
                    if *idb_available {
                        <tr>
                            <td class="label">{ "cached objects" }</td>
                            <td>
                                { format!("{objects} ({size_mb} MB) ") }
                                <button class="clear-btn" onclick={on_clear.clone()}>
                                    { "(clear)" }
                                </button>
                            </td>
                        </tr>
                    } else {
                        <tr>
                            <td class="label">{ "cache" }</td>
                            <td>{ "IndexedDB unavailable" }</td>
                        </tr>
                    }
                </tbody>
            </table>

            // Only where there is a cache to import into; without one every
            // object read out of the bundle would have nowhere to go.
            if *idb_available {
                <h3 class="summary-heading">{ "import a bundle" }</h3>
                <p>
                    { "A " }
                    <a href="https://git-scm.com/docs/git-bundle">{ "bundle" }</a>
                    { " contains Git objects. Loading one preloads objects into gib's \
                       storage, greatly speeding up performance. You can create one from \
                       a Git clone with: " }
                    <code>{ "git bundle create repo.bundle --all" }</code>
                    { ", or download one from the branches and tags pages." }
                </p>
                <p>
                    <input
                        type="file"
                        class="import-input"
                        accept=".bundle,application/x-git-bundle"
                        onchange={on_file}
                    />
                </p>
                { import_status(&import) }
            }
        </>
    }
}

/// What the import is doing, under the file picker: nothing at all before a
/// file is chosen, a bar while one is being read, and what came out of it
/// after.
fn import_status(state: &ImportState) -> Html {
    match state {
        ImportState::Idle => Html::default(),
        ImportState::Reading => html! {
            <p class="import-status">{ "reading the file\u{2026}" }</p>
        },
        ImportState::Storing { done, total } => html! {
            <>
                <p class="import-status">
                    { format!("storing objects\u{2026} {done}/{total}") }
                </p>
                <progress
                    class="import-progress"
                    value={done.to_string()}
                    // As on the snapshot page: a `max` of zero is not a valid
                    // progress element, and an empty bundle would be one.
                    max={total.max(&1).to_string()}
                >
                    { format!("{done}/{total}") }
                </progress>
            </>
        },
        ImportState::Done { objects, refs, .. } => html! {
            <p class="import-status">
                { format!(
                    "imported {objects} object{} from {refs} ref{}.",
                    if *objects == 1 { "" } else { "s" },
                    if *refs == 1 { "" } else { "s" },
                ) }
            </p>
        },
        ImportState::Failed(error) => html! {
            <p class="msg error">{ format!("That bundle didn't load: {error}") }</p>
        },
    }
}

pub(crate) async fn build_about(repo: &Rc<CachingRepo>, clone_url: &Rc<String>) -> AboutProps {
    let head_branch = repo
        .head()
        .await
        .ok()
        .and_then(|r| match r.target() {
            RefTarget::Symbolic(RefName::Ref(b)) => b
                .strip_prefix(b"heads/")
                .map(|s| String::from_utf8_lossy(s).into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| "(detached)".to_string());

    let (branches, tags) = collect_refs(repo).await;
    let (branch_count, tag_count) = (branches.len(), tags.len());

    let (idb_available, objects, size_mb) = match repo.about_stats().await {
        Some((objects, size_mb)) => (true, objects, format!("{size_mb:.2}")),
        None => (false, 0, String::new()),
    };

    // Clicking "(clear)" flushes the object cache; the view updates its own
    // cached-objects row optimistically (see `about_view`). Binding the handler
    // here (rather than querying the DOM after render) also avoids racing Yew's
    // asynchronous initial mount.
    let on_clear = {
        let repo = Rc::clone(repo);
        Callback::from(move |_: MouseEvent| {
            let repo = Rc::clone(&repo);
            wasm_bindgen_futures::spawn_local(async move {
                repo.clear_cache().await;
            });
        })
    };

    AboutProps {
        repo: Some(AboutRepo {
            clone_url: clone_url.as_str().to_string(),
            head_branch,
            branch_count,
            tag_count,
        }),
        idb_available,
        objects,
        size_mb,
        commit: COMMIT.to_string(),
        on_clear,
        // Through this repository's own connection rather than a second one:
        // the objects a bundle carries go into the store every repo shares.
        on_import: import_callback(Rc::new(repo.global_cache())),
    }
}

/// The handler behind the file picker: read the bundle the user chose into the
/// object cache, reporting back as it goes.
///
/// The work runs detached, as the "(clear)" button's does, because a callback
/// can't be async; what makes this one different is that it takes long enough
/// to need saying so, hence the reply callback it reports through.
fn import_callback(cache: Rc<GlobalCache>) -> Callback<(web_sys::File, Callback<ImportState>)> {
    Callback::from(
        move |(file, report): (web_sys::File, Callback<ImportState>)| {
            let cache = Rc::clone(&cache);
            wasm_bindgen_futures::spawn_local(async move {
                // Rate-limited the way the snapshot page's bar is: the reader
                // reports every object, and each report that reaches the view
                // costs a render.
                let last_emit = Cell::new(0.0f64);
                let result = import_bundle(&cache, &file, &|done, total| {
                    let now = js_sys::Date::now();
                    let last = done == total;
                    if last || now - last_emit.get() >= PROGRESS_EMIT_INTERVAL_MS {
                        last_emit.set(now);
                        report.emit(ImportState::Storing { done, total });
                    }
                })
                .await;

                report.emit(match result {
                    Ok(imported) => ImportState::Done {
                        objects: imported.objects,
                        refs: imported.refs,
                        cached: cached_stats(&cache).await,
                    },
                    Err(e) => ImportState::Failed(format!("{e:#}")),
                });
            });
        },
    )
}

/// How often, in milliseconds of wall time, an import may redraw the page.
/// Same interval, and the same reasoning, as the snapshot view's.
const PROGRESS_EMIT_INTERVAL_MS: f64 = 50.0;

/// The cached-objects row's two figures, as the view wants them.
async fn cached_stats(cache: &GlobalCache) -> (usize, String) {
    match cache.stats().await {
        Some((objects, size_mb)) => (objects, format!("{size_mb:.2}")),
        None => (0, String::new()),
    }
}

/// The about page for the repository index: the viewer-wide half only.
pub(crate) async fn build_index_about() -> AboutProps {
    let cache = Rc::new(GlobalCache::open().await);

    let (idb_available, objects, size_mb) = match cache.stats().await {
        Some((objects, size_mb)) => (true, objects, format!("{size_mb:.2}")),
        None => (false, 0, String::new()),
    };

    let on_clear = {
        let cache = Rc::clone(&cache);
        Callback::from(move |_: MouseEvent| {
            let cache = Rc::clone(&cache);
            wasm_bindgen_futures::spawn_local(async move {
                cache.clear().await;
            });
        })
    };

    AboutProps {
        repo: None,
        idb_available,
        objects,
        size_mb,
        commit: COMMIT.to_string(),
        on_clear,
        on_import: import_callback(Rc::clone(&cache)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `AboutView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR
    /// and why indentation is omitted. (SSR omits event listeners, so the
    /// "(clear)" button renders without its `onclick`.)
    fn render(props: AboutProps) -> String {
        // `LocalServerRenderer` (not `ServerRenderer`) because `AboutProps`
        // holds a `Callback`, which is `!Send`.
        let html = futures::executor::block_on(
            yew::LocalServerRenderer::<AboutView>::with_props(props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    fn fixture(idb_available: bool) -> AboutProps {
        AboutProps {
            repo: Some(AboutRepo {
                clone_url: "https://example.org/repo.git".to_string(),
                head_branch: "main".to_string(),
                branch_count: 3,
                tag_count: 7,
            }),
            idb_available,
            objects: 5678,
            size_mb: "56.78".to_string(),
            commit: "0123abcd".to_string(),
            on_clear: Callback::from(|_| ()),
            on_import: Callback::from(|_| ()),
        }
    }

    #[test]
    fn test_about_html_with_idb() {
        insta::assert_snapshot!(render(fixture(true)));
    }

    #[test]
    fn test_about_html_without_idb() {
        insta::assert_snapshot!(render(fixture(false)));
    }

    /// A host component so the plain `import_status` fn can go through SSR:
    /// the real states only ever exist inside a running component. Same
    /// arrangement as the snapshot view's `SvHost`.
    #[derive(Properties, PartialEq, Clone)]
    struct StatusHostProps {
        state: ImportState,
    }

    #[function_component(StatusHost)]
    fn status_host(props: &StatusHostProps) -> Html {
        import_status(&props.state)
    }

    fn render_status(state: ImportState) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<StatusHost>::with_props(move || StatusHostProps { state })
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    /// Mid-import: how many of the bundle's objects have been stored, and a bar
    /// at the ratio between them. Unlike the snapshot page's walk, the
    /// denominator here is known from the first object — a pack says how many
    /// it holds.
    #[test]
    fn test_import_html_storing() {
        insta::assert_snapshot!(render_status(ImportState::Storing {
            done: 4096,
            total: 13658,
        }));
    }

    /// Finished, and what the bundle turned out to hold.
    #[test]
    fn test_import_html_done() {
        insta::assert_snapshot!(render_status(ImportState::Done {
            objects: 13658,
            refs: 7,
            cached: (13658, "56.78".to_string()),
        }));
    }

    /// One object and one ref: the nouns in the summary are singular.
    #[test]
    fn test_import_html_done_single() {
        insta::assert_snapshot!(render_status(ImportState::Done {
            objects: 1,
            refs: 1,
            cached: (1, "0.01".to_string()),
        }));
    }

    /// A file that wasn't a bundle says so where the progress bar was, rather
    /// than failing silently or taking the page down.
    #[test]
    fn test_import_html_failed() {
        insta::assert_snapshot!(render_status(ImportState::Failed(
            "not a v2 or v3 git bundle: PK\u{3}\u{4}".to_string()
        )));
    }

    /// Before a file is picked there is nothing to say, and nothing is said.
    #[test]
    fn test_import_html_idle() {
        assert_eq!(render_status(ImportState::Idle), "");
    }

    /// The repository index's about page: the "gib viewer" section on its own,
    /// with no repository heading or table above it.
    #[test]
    fn test_about_html_index() {
        insta::assert_snapshot!(render(AboutProps {
            repo: None,
            ..fixture(true)
        }));
    }
}
