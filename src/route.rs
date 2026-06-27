use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::about::{AboutProps, build_about};
use crate::render::blob::{BlobProps, build_blob_props};
use crate::render::commit::{CommitProps, build_commit};
use crate::render::log::{LogProps, build_log};
use crate::render::refs_all::{RefsAllProps, build_refs_all};
use crate::render::refs_heads::{RefsHeadsProps, build_refs_heads};
use crate::render::refs_tags::{RefsTagsProps, build_refs_tags};
use crate::render::summary::{SummaryProps, build_summary};
use crate::render::tag::{TagProps, build_tag};
use crate::render::tree::{TreeProps, build_tree_props};
use crate::render::{commit_for_entry, head_branch_name};
use git_async::object::{ObjectId, Tree, TreeEntryType};
use git_async::reference::RefName;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use web_sys::Document;

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

pub(crate) fn set_text(doc: &Document, id: &str, text: &str) {
    doc.get_element_by_id(id)
        .unwrap()
        .set_text_content(Some(text));
}

fn show(doc: &Document, id: &str) {
    doc.get_element_by_id(id)
        .unwrap()
        .class_list()
        .remove_1("hide")
        .unwrap();
}

fn hide_path_bar(doc: &Document) {
    doc.get_element_by_id("path-bar")
        .unwrap()
        .class_list()
        .add_1("hide")
        .unwrap();
}

fn update_nav_for_head(doc: &Document, head: Option<&str>, path: &str) {
    let tabs = doc.query_selector_all("#nav a").unwrap();
    for i in 0..tabs.length() {
        let Some(node) = tabs.get(i) else { continue };
        let Ok(el) = node.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let href = el.get_attribute("href").unwrap_or_default();
        // The href may already carry a path/query from a previous render
        // (e.g. "#!/log/src?h=main"); reduce it to its tab root first.
        let base = href.split('?').next().unwrap_or(&href);
        let new_href = if base == "#!/log" || base.starts_with("#!/log/") {
            // Scope the log tab to whatever path is currently being viewed, so
            // clicking "log" from a subtree shows that subtree's history.
            log_url(path, 0, head)
        } else if base == "#!/tree" || base.starts_with("#!/tree/") {
            match head {
                Some(h) => format!("#!/tree?h={h}"),
                None => "#!/tree".to_string(),
            }
        } else {
            continue;
        };
        el.set_attribute("href", &new_href).ok();
    }
}

fn update_path_bar(
    doc: &Document,
    path: &str,
    url_head: Option<&str>,
    display: Option<(&str, &RefKind)>,
) {
    let bar = doc.get_element_by_id("path-bar").unwrap();
    let head_suffix = url_head.map_or(String::new(), |h| format!("?h={h}"));
    let mut html = String::new();
    if let Some((name, kind)) = display {
        let label = match kind {
            RefKind::Tag => "tag",
            RefKind::Branch => "branch",
        };
        html.push_str(&format!("{label}: {name} | "));
    }
    html.push_str(&format!("path: <a href=\"#!/tree{head_suffix}\">root</a>"));
    let mut cumulative = String::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(component);
        html.push_str(&format!(
            " / <a href=\"#!/tree/{0}{1}\">{2}</a>",
            cumulative, head_suffix, component
        ));
    }
    bar.set_inner_html(&html);
}

fn set_active_tab(doc: &Document, tab: &str) {
    let tabs = doc.query_selector_all("#nav a").unwrap();
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

pub(crate) enum RefsRoute {
    All,
    Heads,
    Tags,
    Tag(String),
}

pub(crate) enum Route {
    About,
    Summary,
    Log {
        offset: usize,
        head: Option<String>,
        path: String,
    },
    CommitHead,
    Commit(String),
    Refs(RefsRoute),
    Tree {
        path: String,
        head: Option<String>,
    },
}

pub(crate) fn parse_hash(hash: &str) -> Route {
    // most likely scenario
    if hash == "#!/summary" || hash.is_empty() || hash == "#" {
        return Route::Summary;
    }
    if hash == "#!/about" {
        return Route::About;
    }

    if let Some(rest) = hash.strip_prefix("#!/log") {
        // rest is one of: "", "?query", "/path", or "/path?query".
        let (path_part, query_string) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let path = path_part.trim_start_matches('/').to_string();
        let (offset, head) = parse_log_query(query_string);
        return Route::Log { offset, head, path };
    }

    if hash == "#!/commit" {
        return Route::CommitHead;
    }
    if let Some(sha) = hash.strip_prefix("#!/commit/") {
        return Route::Commit(sha.to_string());
    }

    if let Some(rest) = hash.strip_prefix("#!/tree") {
        let (path, head) = parse_tree_rest(rest);
        return Route::Tree { path, head };
    }

    if hash.starts_with("#!/refs") {
        let subroute = if hash == "#!/refs/tags" {
            RefsRoute::Tags
        } else if hash == "#!/refs/heads" {
            RefsRoute::Heads
        } else if let Some(tag) = hash.strip_prefix("#!/refs/tags/") {
            RefsRoute::Tag(tag.to_string())
        } else {
            RefsRoute::All
        };
        return Route::Refs(subroute);
    }

    // fallback to summary on invalid routes
    Route::Summary
}

fn parse_tree_rest(rest: &str) -> (String, Option<String>) {
    let rest = rest.trim_start_matches('/');
    let (path_part, query_string) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let head = query_string.and_then(|qs| {
        qs.split('&')
            .find_map(|part| part.strip_prefix("h="))
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    });
    (path_part.to_string(), head)
}

fn parse_log_query(query_string: &str) -> (usize, Option<String>) {
    let mut offset = 0usize;
    let mut head = None;
    for part in query_string.split('&') {
        if let Some(v) = part.strip_prefix("offset=") {
            offset = v.parse().unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("h=")
            && !v.is_empty()
        {
            head = Some(v.to_string());
        }
    }
    (offset, head)
}

pub(crate) enum RefKind {
    Tag,
    Branch,
}

async fn resolve_ref_to_commit(
    repo: &CachingRepo,
    name: &str,
) -> anyhow::Result<(git_async::object::Commit, RefKind)> {
    let refs = repo.all_refs().await.context("list refs")?;
    let tags_ref = RefName::Ref(format!("tags/{name}").into_bytes());
    if let Some(entry) = refs.get(&tags_ref)
        && let Some(commit) = commit_for_entry(entry, repo).await
    {
        return Ok((commit, RefKind::Tag));
    }
    let heads_ref = RefName::Ref(format!("heads/{name}").into_bytes());
    let entry = refs
        .get(&heads_ref)
        .ok_or_else(|| anyhow::anyhow!("ref not found: {name}"))?;
    let commit = commit_for_entry(entry, repo)
        .await
        .ok_or_else(|| anyhow::anyhow!("ref {name} does not point to a commit"))?;
    Ok((commit, RefKind::Branch))
}

/// Update only the chrome (nav active tab, nav hrefs, path bar) for `hash`. The
/// content is rendered separately by `RouteView` via [`build_route`]; resolving
/// refs here is best-effort, since content-level errors surface in the view.
pub(crate) async fn handle_route(hash: String, repo: &Rc<CachingRepo>, doc: &Document) {
    let route = parse_hash(&hash);
    let head = match &route {
        Route::Log { head, .. } | Route::Tree { head, .. } => head.as_deref(),
        _ => None,
    };
    let nav_path = match &route {
        Route::Log { path, .. } | Route::Tree { path, .. } => path.as_str(),
        _ => "",
    };
    update_nav_for_head(doc, head, nav_path);

    let active = match &route {
        Route::About => "#!/about",
        Route::Summary => "#!/summary",
        Route::Log { .. } => "#!/log",
        Route::CommitHead | Route::Commit(_) => "#!/commit",
        Route::Refs(_) => "#!/refs",
        Route::Tree { .. } => "#!/tree",
    };
    set_active_tab(doc, active);

    match &route {
        Route::Tree { path, head } => {
            let display = resolve_display_head(repo, head.as_deref()).await;
            update_path_bar(doc, path, head.as_deref(), display_ref(&display));
            show(doc, "path-bar");
        }
        Route::Log { path, head, .. } if !path.is_empty() => {
            // Path-scoped log: show the same breadcrumb the tree view uses.
            let display = resolve_display_head(repo, head.as_deref()).await;
            update_path_bar(doc, path, head.as_deref(), display_ref(&display));
            show(doc, "path-bar");
        }
        Route::Log { head, .. } => {
            // Whole-history log: just label the ref, if any.
            match resolve_display_head(repo, head.as_deref()).await {
                Some((name, kind)) => {
                    let label = match kind {
                        RefKind::Tag => "tag",
                        RefKind::Branch => "branch",
                    };
                    doc.get_element_by_id("path-bar")
                        .unwrap()
                        .set_inner_html(&format!("{label}: {name}"));
                    show(doc, "path-bar");
                }
                None => hide_path_bar(doc),
            }
        }
        _ => hide_path_bar(doc),
    }
}

/// The ref name + kind shown in the path bar / log header: the explicit `?h=`
/// ref (with its resolved kind), or the implicit HEAD branch. `None` if it
/// can't be resolved — the content view reports the real error.
async fn resolve_display_head(repo: &CachingRepo, head: Option<&str>) -> Option<(String, RefKind)> {
    match head {
        Some(name) => resolve_ref_to_commit(repo, name)
            .await
            .ok()
            .map(|(_, kind)| (name.to_string(), kind)),
        None => head_branch_name(repo).await.map(|n| (n, RefKind::Branch)),
    }
}

fn display_ref(display: &Option<(String, RefKind)>) -> Option<(&str, &RefKind)> {
    display.as_ref().map(|(n, k)| (n.as_str(), k))
}

/// A route's resolved content, ready to render. The chrome (nav, path bar) is
/// handled separately by [`handle_route`].
pub(crate) enum LoadedView {
    About(AboutProps),
    Summary(SummaryProps),
    Log(LogProps),
    Commit(CommitProps),
    RefsHeads(RefsHeadsProps),
    RefsTags(RefsTagsProps),
    RefsAll(RefsAllProps),
    Tag(TagProps),
    Tree(TreeProps),
    Blob(BlobProps),
    /// A tree path that resolved to neither a subtree nor a blob.
    NotFound(String),
}

/// Resolve `hash` into the props for the view it names. Errors (bad refs,
/// missing objects) propagate so `RouteView` can show them in the content area.
pub(crate) async fn build_route(
    hash: &str,
    head_commit: &git_async::object::Commit,
    root_tree: &Tree,
    repo: &Rc<CachingRepo>,
    clone_url: &Rc<String>,
) -> anyhow::Result<LoadedView> {
    match parse_hash(hash) {
        Route::About => Ok(LoadedView::About(build_about(repo, clone_url).await)),
        Route::Summary => Ok(LoadedView::Summary(
            build_summary(head_commit, repo, clone_url.as_str()).await,
        )),
        Route::Log { offset, head, path } => {
            let resolved;
            let log_commit: &git_async::object::Commit = match &head {
                Some(name) => {
                    resolved = resolve_ref_to_commit(repo, name).await?.0;
                    &resolved
                }
                None => head_commit,
            };
            Ok(LoadedView::Log(
                build_log(log_commit, repo, &path, offset, head.as_deref()).await,
            ))
        }
        Route::CommitHead => Ok(LoadedView::Commit(
            build_commit(repo, &format!("{}", head_commit.id())).await?,
        )),
        Route::Commit(sha) => Ok(LoadedView::Commit(build_commit(repo, &sha).await?)),
        Route::Refs(RefsRoute::Heads) => Ok(LoadedView::RefsHeads(build_refs_heads(repo).await)),
        Route::Refs(RefsRoute::Tags) => Ok(LoadedView::RefsTags(build_refs_tags(repo).await)),
        Route::Refs(RefsRoute::All) => Ok(LoadedView::RefsAll(build_refs_all(repo).await)),
        Route::Refs(RefsRoute::Tag(tag)) => Ok(LoadedView::Tag(build_tag(repo, tag).await?)),
        Route::Tree { path, head } => {
            let resolved_tree;
            let tree: &Tree = if let Some(ref ref_name) = head {
                let (commit, _kind) = resolve_ref_to_commit(repo, ref_name).await?;
                resolved_tree = repo
                    .lookup_object(commit.tree())
                    .await
                    .context(format!("lookup tree for {ref_name}"))?
                    .tree()
                    .map_err(git_async::error::Error::from)
                    .context(format!("expected tree for {ref_name}"))?;
                &resolved_tree
            } else {
                root_tree
            };

            if let Some(subtree) = walk_to_tree(tree, &path, repo).await {
                Ok(LoadedView::Tree(build_tree_props(
                    &subtree,
                    &path,
                    head.as_deref(),
                )))
            } else if let Some((id, data)) = walk_to_blob(tree, &path, repo).await {
                Ok(LoadedView::Blob(build_blob_props(id, &data)))
            } else {
                Ok(LoadedView::NotFound(path))
            }
        }
    }
}

pub(crate) fn log_url(path: &str, offset: usize, head: Option<&str>) -> String {
    let base = if path.is_empty() {
        "#!/log".to_string()
    } else {
        format!("#!/log/{path}")
    };
    match (offset, head) {
        (0, None) => base,
        (n, None) => format!("{base}?offset={n}"),
        (0, Some(head)) => format!("{base}?h={head}"),
        (n, Some(head)) => format!("{base}?h={head}&offset={n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hash_summary() {
        assert!(matches!(parse_hash(""), Route::Summary));
        assert!(matches!(parse_hash("#"), Route::Summary));
        assert!(matches!(parse_hash("#!/summary"), Route::Summary));
    }

    #[test]
    fn test_parse_hash_about() {
        assert!(matches!(parse_hash("#!/about"), Route::About));
    }

    #[test]
    fn test_parse_hash_log_bare() {
        assert!(matches!(
            parse_hash("#!/log"),
            Route::Log {
                offset: 0,
                head: None,
                path,
            } if path.is_empty()
        ));
    }

    #[test]
    fn test_parse_hash_log_head_only() {
        let route = parse_hash("#!/log?h=main");
        assert!(matches!(
            route,
            Route::Log {
                offset: 0,
                head: Some(_),
                ..
            }
        ));
        if let Route::Log {
            head: Some(head),
            path,
            ..
        } = route
        {
            assert_eq!(head, "main");
            assert!(path.is_empty());
        }
    }

    #[test]
    fn test_parse_hash_log_head_with_offset() {
        let route = parse_hash("#!/log?h=stable&offset=100");
        if let Route::Log {
            offset,
            head: Some(head),
            ..
        } = route
        {
            assert_eq!(head, "stable");
            assert_eq!(offset, 100);
        } else {
            panic!("expected Log with head and offset");
        }
    }

    #[test]
    fn test_parse_hash_log_offset_only() {
        let route = parse_hash("#!/log?offset=50");
        assert!(matches!(
            route,
            Route::Log {
                offset: 50,
                head: None,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_hash_log_empty_head_ignored() {
        let route = parse_hash("#!/log?h=");
        assert!(matches!(
            route,
            Route::Log {
                offset: 0,
                head: None,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_hash_log_path() {
        let route = parse_hash("#!/log/src/route.rs");
        assert!(matches!(
            route,
            Route::Log {
                offset: 0,
                head: None,
                path,
            } if path == "src/route.rs"
        ));
    }

    #[test]
    fn test_parse_hash_log_path_with_head_and_offset() {
        let route = parse_hash("#!/log/src?h=main&offset=50");
        if let Route::Log {
            offset,
            head: Some(head),
            path,
        } = route
        {
            assert_eq!(offset, 50);
            assert_eq!(head, "main");
            assert_eq!(path, "src");
        } else {
            panic!("expected Log with path, head and offset");
        }
    }

    #[test]
    fn test_parse_hash_commit() {
        assert!(matches!(parse_hash("#!/commit"), Route::CommitHead));
        assert!(matches!(parse_hash("#!/commit/abc123"), Route::Commit(_)));
    }

    #[test]
    fn test_parse_hash_tree() {
        assert!(matches!(
            parse_hash("#!/tree"),
            Route::Tree { path, head: None } if path.is_empty()
        ));
        assert!(matches!(
            parse_hash("#!/tree/src/main.rs"),
            Route::Tree { path, head: None } if path == "src/main.rs"
        ));
    }

    #[test]
    fn test_parse_tree_rest() {
        assert_eq!(parse_tree_rest(""), ("".into(), None));
        assert_eq!(parse_tree_rest("/src"), ("src".into(), None));
        assert_eq!(parse_tree_rest("?h=main"), ("".into(), Some("main".into())));
        assert_eq!(
            parse_tree_rest("/src?h=stable"),
            ("src".into(), Some("stable".into()))
        );
        assert_eq!(parse_tree_rest("?h="), ("".into(), None));
    }

    #[test]
    fn test_parse_hash_refs() {
        assert!(matches!(parse_hash("#!/refs"), Route::Refs(RefsRoute::All)));
        assert!(matches!(
            parse_hash("#!/refs/heads"),
            Route::Refs(RefsRoute::Heads)
        ));
        assert!(matches!(
            parse_hash("#!/refs/tags"),
            Route::Refs(RefsRoute::Tags)
        ));
        assert!(matches!(
            parse_hash("#!/refs/tags/v1.0"),
            Route::Refs(RefsRoute::Tag(_))
        ));
    }

    #[test]
    fn test_log_url() {
        assert_eq!(log_url("", 0, None), "#!/log");
        assert_eq!(log_url("", 50, None), "#!/log?offset=50");
        assert_eq!(log_url("", 0, Some("main")), "#!/log?h=main");
        assert_eq!(
            log_url("", 100, Some("stable")),
            "#!/log?h=stable&offset=100"
        );
        assert_eq!(log_url("src/route.rs", 0, None), "#!/log/src/route.rs");
        assert_eq!(
            log_url("src", 50, Some("main")),
            "#!/log/src?h=main&offset=50"
        );
    }
}
