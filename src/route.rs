use crate::cache::{CachingRepo, ClearTarget};
use crate::console_log;
use crate::render::about::render_about;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use crate::render::commit::render_commit;
use crate::render::log::render_log;
use crate::render::refs_all::render_refs_all;
use crate::render::refs_heads::render_refs_heads;
use crate::render::refs_tags::render_refs_tags;
use crate::render::tag::render_tag;
use crate::render::{blob::render_blob, summary::render_summary, tree::render_tree};
use git_async::object::{ObjectId, Tree, TreeEntryType};
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
    Log(usize),
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

    if hash == "#!/log" || hash.starts_with("#!/log/") {
        let offset = hash
            .strip_prefix("#!/log/")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        return Route::Log(offset);
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

    match parse_hash(&hash) {
        Route::About => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/about");
            render_about(tera, repo, clone_url, &output).await;
            attach_about_handlers(doc, &output, repo, tera, clone_url);
        }
        Route::Summary => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/summary");
            render_summary(tera, head_commit, repo, clone_url, &output).await;
        }
        Route::Log(offset) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/log");
            render_log(tera, head_commit, repo, offset, &output).await;
        }
        Route::CommitHead => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(tera, repo, format!("{}", head_commit.id()), &output).await;
        }
        Route::Commit(sha) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/commit");
            render_commit(tera, repo, sha, &output).await;
        }
        Route::Refs(RefsRoute::Heads) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_heads(tera, repo, &output).await;
        }
        Route::Refs(RefsRoute::Tags) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_tags(tera, repo, &output).await;
        }
        Route::Refs(RefsRoute::Tag(tag)) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            console_log(&tag);
            render_tag(tera, repo, tag, &output).await;
        }
        Route::Refs(RefsRoute::All) => {
            hide_path_bar(doc);
            set_active_tab(doc, "#!/refs");
            render_refs_all(tera, repo, &output).await;
        }
        Route::Tree(path) => {
            update_path_bar(doc, &path);
            show(doc, "path-bar");
            set_active_tab(doc, "#!/tree");

            if let Some(subtree) = walk_to_tree(root_tree, &path, repo).await {
                render_tree(tera, &subtree, &path, &output);
                return;
            }

            output.set_inner_html("<p class=\"msg\">Loading\u{2026}</p>");
            match walk_to_blob(root_tree, &path, repo).await {
                Some((id, data)) => render_blob(tera, id, &data, &output),
                None => output.set_inner_html(&format!(
                    "<p class=\"msg error\">Not found: <code>{}</code></p>",
                    path
                )),
            }
        }
    }
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
        let Ok(btn) = node.dyn_into::<web_sys::Element>() else { continue };
        let Some(target_str) = btn.get_attribute("data-target") else { continue };
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
                render_about(&tera, &repo, &clone_url, &output).await;
                attach_about_handlers(&doc, &output, &repo, &tera, &clone_url);
            });
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .ok();
        cb.forget();
    }
}
