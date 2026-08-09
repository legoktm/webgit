use crate::cache::CachingRepo;
use crate::render::collect_refs;
use gib::reference::{RefName, RefTarget};
use git_version::git_version;
use std::rc::Rc;
use yew::prelude::*;

const COMMIT: &str = git_version!();

/// The view inputs for the about page. The `on_clear` callback is wired to the
/// "(clear)" button so the cache can be flushed and the page re-rendered; it's
/// unused when caching is unavailable (no button is shown).
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct AboutProps {
    pub clone_url: String,
    pub head_branch: String,
    pub branch_count: usize,
    pub tag_count: usize,
    pub idb_available: bool,
    pub objects: usize,
    pub size_mb: String,
    pub commit: String,
    pub on_clear: Callback<MouseEvent>,
}

/// The Yew component used to mount the about view into the DOM. Unlike the
/// other ported views, the markup isn't split into a plain function: it holds
/// `use_state` (so it must run as a component), and its tests render it through
/// SSR anyway, so a renderer-free markup fn would buy nothing.
#[function_component(AboutView)]
pub(crate) fn about_view(props: &AboutProps) -> Html {
    let AboutProps {
        clone_url,
        head_branch,
        branch_count,
        tag_count,
        idb_available,
        objects,
        size_mb,
        commit,
        on_clear,
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

    html! {
        <>
            <h3 class="summary-heading">{ "repository" }</h3>
            <table class="tag-table">
                <tbody>
                    <tr>
                        <td class="label">{ "clone URL" }</td>
                        <td class="mono">{ clone_url }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "HEAD branch" }</td>
                        <td>{ head_branch }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "branches" }</td>
                        <td>{ branch_count }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "tags" }</td>
                        <td>{ tag_count }</td>
                    </tr>
                </tbody>
            </table>

            <h3 class="summary-heading">{ "viewer" }</h3>
            <p>
                { "This viewer allows you to view repositories entirely \
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
        </>
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
        clone_url: clone_url.as_str().to_string(),
        head_branch,
        branch_count,
        tag_count,
        idb_available,
        objects,
        size_mb,
        commit: COMMIT.to_string(),
        on_clear,
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
            clone_url: "https://example.org/repo.git".to_string(),
            head_branch: "main".to_string(),
            branch_count: 3,
            tag_count: 7,
            idb_available,
            objects: 5678,
            size_mb: "56.78".to_string(),
            commit: "0123abcd".to_string(),
            on_clear: Callback::from(|_| ()),
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
}
