use crate::cache::CachingRepo;
use crate::console_log;
use crate::error::{GitContext, error_html};
use crate::render::about::render_about;
use crate::render::commit::render_commit;
use crate::render::log::render_log;
use crate::render::refs_all::render_refs_all;
use crate::render::refs_heads::render_refs_heads;
use crate::render::refs_tags::render_refs_tags;
use crate::render::tag::render_tag;
use crate::render::{
    blob::render_blob, commit_for_entry, head_branch_name, summary::render_summary,
    tree::render_tree,
};
use git_async::object::{ObjectId, Tree, TreeEntryType};
use git_async::reference::RefName;
use std::rc::Rc;
use tera::Tera;
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

pub(crate) async fn handle_route(
    hash: String,
    head_commit: &git_async::object::Commit,
    root_tree: &Tree,
    repo: &Rc<CachingRepo>,
    clone_url: &Rc<String>,
    doc: &Document,
    tera: &Rc<Tera>,
) {
    let output = doc.get_element_by_id("output").unwrap();
    output.set_inner_html("");
    if let Err(e) = try_handle_route(hash, head_commit, root_tree, repo, clone_url, doc, tera).await
    {
        output.set_inner_html(&error_html(&format!("{e:#}")));
    }
}

async fn try_handle_route(
    hash: String,
    head_commit: &git_async::object::Commit,
    root_tree: &Tree,
    repo: &Rc<CachingRepo>,
    clone_url: &Rc<String>,
    doc: &Document,
    tera: &Rc<Tera>,
) -> anyhow::Result<()> {
    let output = &doc.get_element_by_id("output").unwrap();
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
    match route {
        Route::About => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/about");
            render_about(repo, clone_url, output).await?;
        }
        Route::Summary => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/summary");
            render_summary(tera, head_commit, repo, clone_url, output).await?;
        }
        Route::Log { offset, head, path } => {
            set_active_tab(doc, "#!/log");
            let (resolved, display_head): (
                Option<git_async::object::Commit>,
                Option<(String, RefKind)>,
            ) = if let Some(ref ref_name) = head {
                let (commit, kind) = resolve_ref_to_commit(repo, ref_name).await?;
                (Some(commit), Some((ref_name.clone(), kind)))
            } else {
                let implicit = head_branch_name(repo).await;
                (None, implicit.map(|n| (n, RefKind::Branch)))
            };
            let log_commit = resolved.as_ref().unwrap_or(head_commit);
            let display = display_head.as_ref().map(|(n, k)| (n.as_str(), k));
            if path.is_empty() {
                // Whole-history log: just label the ref, if any.
                if let Some((name, kind)) = display {
                    let label = match kind {
                        RefKind::Tag => "tag",
                        RefKind::Branch => "branch",
                    };
                    doc.get_element_by_id("path-bar")
                        .unwrap()
                        .set_inner_html(&format!("{label}: {name}"));
                    show(doc, "path-bar");
                } else {
                    hide_path_bar(doc);
                }
            } else {
                // Path-scoped log: show the same breadcrumb the tree view uses.
                update_path_bar(doc, &path, head.as_deref(), display);
                show(doc, "path-bar");
            }
            render_log(log_commit, repo, &path, offset, head.as_deref(), output).await?;
        }
        Route::CommitHead => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(repo, format!("{}", head_commit.id()), output).await?;
        }
        Route::Commit(sha) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(repo, sha, output).await?;
        }
        Route::Refs(RefsRoute::Heads) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_heads(repo, output).await?;
        }
        Route::Refs(RefsRoute::Tags) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_tags(repo, output).await?;
        }
        Route::Refs(RefsRoute::Tag(tag)) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            console_log(&tag);
            render_tag(repo, tag, output).await?;
        }
        Route::Refs(RefsRoute::All) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_all(repo, output).await?;
        }
        Route::Tree { path, head } => {
            let resolved_tree;
            let (tree, display_head): (&Tree, Option<(String, RefKind)>) =
                if let Some(ref ref_name) = head {
                    let (commit, kind) = resolve_ref_to_commit(repo, ref_name).await?;
                    resolved_tree = repo
                        .lookup_object(commit.tree())
                        .await
                        .context(format!("lookup tree for {ref_name}"))?
                        .tree()
                        .map_err(git_async::error::Error::from)
                        .context(format!("expected tree for {ref_name}"))?;
                    (&resolved_tree, Some((ref_name.clone(), kind)))
                } else {
                    let implicit = head_branch_name(repo).await;
                    (root_tree, implicit.map(|n| (n, RefKind::Branch)))
                };

            let display = display_head.as_ref().map(|(n, k)| (n.as_str(), k));
            update_path_bar(doc, &path, head.as_deref(), display);
            show(doc, "path-bar");
            set_active_tab(doc, "#!/tree");

            if let Some(subtree) = walk_to_tree(tree, &path, repo).await {
                return render_tree(&subtree, &path, head.as_deref(), output);
            }

            output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");
            match walk_to_blob(tree, &path, repo).await {
                Some((id, data)) => render_blob(id, &data, output)?,
                None => output.set_inner_html(&format!(
                    "<p class=\"msg error\">Not found: <code>{}</code></p>",
                    path
                )),
            }
        }
    }
    Ok(())
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
