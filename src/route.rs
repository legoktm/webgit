use crate::cache::{CachingRepo, ClearTarget};
use crate::console_log;
use crate::error::{GitContext, error_html};
use crate::render::about::render_about;
use crate::render::commit::render_commit;
use crate::render::log::render_log;
use crate::render::refs_all::render_refs_all;
use crate::render::refs_heads::render_refs_heads;
use crate::render::refs_tags::render_refs_tags;
use crate::render::tag::render_tag;
use crate::render::{blob::render_blob, summary::render_summary, tree::render_tree};
use git_async::object::{ObjectId, Tree, TreeEntryType};
use git_async::reference::RefName;
use std::rc::Rc;
use tera::Tera;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
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

fn update_path_bar(doc: &Document, path: &str) {
    let bar = doc.get_element_by_id("path-bar").unwrap();
    let mut html = String::from("<a href=\"#!/tree\">root</a>");
    let mut cumulative = String::new();
    for component in path.split('/').filter(|s| !s.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(component);
        html.push_str(&format!(
            " / <a href=\"#!/tree/{0}\">{1}</a>",
            cumulative, component
        ));
    }
    bar.set_inner_html(&format!("path: {}", html));
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
    Log { offset: usize, head: Option<String> },
    CommitHead,
    Commit(String),
    Refs(RefsRoute),
    Tree(String),
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
        if rest.is_empty() {
            return Route::Log {
                offset: 0,
                head: None,
            };
        }
        if let Some(query_string) = rest.strip_prefix('?') {
            let (offset, head) = parse_log_query(query_string);
            return Route::Log { offset, head };
        }
        return Route::Log {
            offset: 0,
            head: None,
        };
    }

    if hash == "#!/commit" {
        return Route::CommitHead;
    }
    if let Some(sha) = hash.strip_prefix("#!/commit/") {
        return Route::Commit(sha.to_string());
    }

    if hash.starts_with("#!/tree") {
        let rest = hash.strip_prefix("#!/tree").unwrap();
        return Route::Tree(rest.trim_start_matches('/').to_string());
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

async fn resolve_ref_to_commit(
    repo: &CachingRepo,
    name: &str,
) -> anyhow::Result<git_async::object::Commit> {
    let tags_ref = RefName::Ref(format!("tags/{name}").into_bytes());
    if let Ok(r) = repo.lookup_ref(&tags_ref).await
        && let Ok(Some(commit)) = repo.peel_ref_to_commit(&r).await
    {
        return Ok(commit);
    }
    let heads_ref = RefName::Ref(format!("heads/{name}").into_bytes());
    let r = repo
        .lookup_ref(&heads_ref)
        .await
        .context(format!("ref not found: {name}"))?;
    repo.peel_ref_to_commit(&r)
        .await
        .context(format!("peel ref {name}"))?
        .ok_or_else(|| anyhow::anyhow!("ref {name} does not point to a commit"))
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
    match parse_hash(&hash) {
        Route::About => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/about");
            render_about(tera, repo, clone_url, output).await?;
            attach_about_handlers(doc, output, repo, tera, clone_url);
        }
        Route::Summary => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/summary");
            render_summary(tera, head_commit, repo, clone_url, output).await?;
        }
        Route::Log { offset, head } => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/log");
            let resolved;
            let log_commit = if let Some(ref ref_name) = head {
                resolved = resolve_ref_to_commit(repo, ref_name).await?;
                &resolved
            } else {
                head_commit
            };
            render_log(tera, log_commit, repo, offset, head.as_deref(), output).await?;
        }
        Route::CommitHead => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(tera, repo, format!("{}", head_commit.id()), output).await?;
        }
        Route::Commit(sha) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(tera, repo, sha, output).await?;
        }
        Route::Refs(RefsRoute::Heads) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_heads(tera, repo, output).await?;
        }
        Route::Refs(RefsRoute::Tags) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_tags(tera, repo, output).await?;
        }
        Route::Refs(RefsRoute::Tag(tag)) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            console_log(&tag);
            render_tag(tera, repo, tag, output).await?;
        }
        Route::Refs(RefsRoute::All) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_all(tera, repo, output).await?;
        }
        Route::Tree(path) => {
            update_path_bar(doc, &path);
            show(doc, "path-bar");
            set_active_tab(doc, "#!/tree");

            if let Some(subtree) = walk_to_tree(root_tree, &path, repo).await {
                return render_tree(tera, &subtree, &path, output);
            }

            output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");
            match walk_to_blob(root_tree, &path, repo).await {
                Some((id, data)) => render_blob(tera, id, &data, output)?,
                None => output.set_inner_html(&format!(
                    "<p class=\"msg error\">Not found: <code>{}</code></p>",
                    path
                )),
            }
        }
    }
    Ok(())
}

fn attach_about_handlers(
    doc: &Document,
    output: &web_sys::Element,
    repo: &Rc<CachingRepo>,
    tera: &Rc<Tera>,
    clone_url: &Rc<String>,
) {
    let Ok(nodes) = output.query_selector_all("[data-target]") else {
        return;
    };
    for i in 0..nodes.length() {
        let Some(node) = nodes.get(i) else { continue };
        let Ok(btn) = node.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let Some(target_str) = btn.get_attribute("data-target") else {
            continue;
        };
        let target = match target_str.as_str() {
            "repo-objects" => ClearTarget::RepoObjects,
            "all-objects" => ClearTarget::AllObjects,
            "repo-tags" => ClearTarget::RepoTags,
            "all-tags" => ClearTarget::AllTags,
            _ => continue,
        };
        let repo = Rc::clone(repo);
        let tera = Rc::clone(tera);
        let clone_url = Rc::clone(clone_url);
        let doc = doc.clone();
        let output = output.clone();
        let cb = Closure::<dyn Fn()>::new(move || {
            let repo = Rc::clone(&repo);
            let tera = Rc::clone(&tera);
            let clone_url = Rc::clone(&clone_url);
            let doc = doc.clone();
            let output = output.clone();
            wasm_bindgen_futures::spawn_local(async move {
                repo.clear_cache(target).await;
                if let Err(e) = render_about(&tera, &repo, &clone_url, &output).await {
                    output.set_inner_html(&error_html(&format!("{e:#}")));
                    return;
                }
                attach_about_handlers(&doc, &output, &repo, &tera, &clone_url);
            });
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .ok();
        cb.forget();
    }
}

pub(crate) fn log_url(offset: usize, head: Option<&str>) -> String {
    match (offset, head) {
        (0, None) => "#!/log".to_string(),
        (n, None) => format!("#!/log?offset={n}"),
        (0, Some(head)) => format!("#!/log?h={head}"),
        (n, Some(head)) => format!("#!/log?h={head}&offset={n}"),
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
                head: None
            }
        ));
    }

    #[test]
    fn test_parse_hash_log_head_only() {
        let route = parse_hash("#!/log?h=main");
        assert!(matches!(
            route,
            Route::Log {
                offset: 0,
                head: Some(_)
            }
        ));
        if let Route::Log {
            head: Some(head), ..
        } = route
        {
            assert_eq!(head, "main");
        }
    }

    #[test]
    fn test_parse_hash_log_head_with_offset() {
        let route = parse_hash("#!/log?h=stable&offset=100");
        if let Route::Log {
            offset,
            head: Some(head),
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
                head: None
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
                head: None
            }
        ));
    }

    #[test]
    fn test_parse_hash_commit() {
        assert!(matches!(parse_hash("#!/commit"), Route::CommitHead));
        assert!(matches!(parse_hash("#!/commit/abc123"), Route::Commit(_)));
    }

    #[test]
    fn test_parse_hash_tree() {
        assert!(matches!(parse_hash("#!/tree"), Route::Tree(_)));
        assert!(matches!(parse_hash("#!/tree/src/main.rs"), Route::Tree(_)));
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
        assert_eq!(log_url(0, None), "#!/log");
        assert_eq!(log_url(50, None), "#!/log?offset=50");
        assert_eq!(log_url(0, Some("main")), "#!/log?h=main");
        assert_eq!(log_url(100, Some("stable")), "#!/log?h=stable&offset=100");
    }
}
