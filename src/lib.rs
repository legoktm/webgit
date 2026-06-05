mod error;
mod fetch;
mod fs;

use error::fmt_git_err;
use fs::{HttpDirectory, HttpFilesystem};
use git_async::Repo;
use git_async::error::{Error as GitError, GResult};
use git_async::object::{Tree, TreeEntryType};
use wasm_bindgen::prelude::*;
use web_sys::{Document, Event, HtmlInputElement};

/// Recursively collect all file paths under `tree` into `out`.
/// `prefix` is the slash-separated path built so far (empty at the root).
async fn list_tree(
    tree: &Tree,
    prefix: &str,
    repo: &Repo<HttpFilesystem>,
    out: &mut Vec<String>,
) -> GResult<()> {
    for entry in tree.entries() {
        let name = String::from_utf8_lossy(entry.name()).into_owned();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };

        match entry.entry_type() {
            TreeEntryType::Tree => {
                let obj = repo.lookup_object(entry.id()).await?;
                let subtree = obj.tree().map_err(GitError::from)?;
                Box::pin(list_tree(&subtree, &path, repo, out)).await?;
            }
            // Files, executables, symlinks -- all leaf paths.
            _ => out.push(path),
        }
    }
    Ok(())
}

async fn load_repo(url: String, output: web_sys::Element) {
    output.set_inner_html(&format!("<p>Opening repo at <code>{}</code>...</p>", url));

    let dir = HttpDirectory::new(url);
    let repo = match Repo::<HttpFilesystem>::open(dir).await {
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"error\">Failed to open repo: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
        Ok(repo) => repo,
    };

    let head = match repo.head().await {
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"error\">Failed to read HEAD: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
        Ok(head) => head,
    };

    use git_async::reference::{RefName, RefTarget};
    let target_str = match head.target() {
        RefTarget::Symbolic(RefName::Head) => "HEAD (symbolic)".to_string(),
        RefTarget::Symbolic(RefName::Ref(name)) => {
            format!("refs/{}", String::from_utf8_lossy(name))
        }
        RefTarget::Direct(oid) => format!("{}", oid),
    };

    // Update the DOM while the (potentially slow) tree walk runs.
    output.set_inner_html(&format!(
        "<p>HEAD &rarr; <strong>{}</strong></p><p>Loading file tree...</p>",
        target_str
    ));

    let commit = match head.peel_to_commit(&repo).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            output.set_inner_html("<p class=\"error\">HEAD does not point to a commit</p>");
            return;
        }
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"error\">Failed to peel HEAD to commit: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
    };

    let root_tree = match repo.lookup_object(commit.tree()).await {
        Ok(obj) => match obj.tree() {
            Ok(t) => t,
            Err(e) => {
                output.set_inner_html(&format!(
                    "<p class=\"error\">Root object is not a tree: {:?}</p>",
                    e
                ));
                return;
            }
        },
        Err(e) => {
            output.set_inner_html(&format!(
                "<p class=\"error\">Failed to load root tree: {}</p>",
                fmt_git_err(&e)
            ));
            return;
        }
    };

    let mut files = Vec::new();
    if let Err(e) = list_tree(&root_tree, "", &repo, &mut files).await {
        output.set_inner_html(&format!(
            "<p class=\"error\">Failed to walk tree: {}</p>",
            fmt_git_err(&e)
        ));
        return;
    }

    let items: String = files
        .iter()
        .map(|f| format!("<li><code>{}</code></li>", f))
        .collect();
    output.set_inner_html(&format!(
        "<p>HEAD &rarr; <strong>{}</strong> &mdash; {} files</p><ul class=\"file-list\">{}</ul>",
        target_str,
        files.len(),
        items
    ));
}

#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();

    let window = web_sys::window().expect("no window");
    let document: Document = window.document().expect("no document");

    let input: HtmlInputElement = document
        .get_element_by_id("repo-url")
        .expect("no #repo-url element")
        .dyn_into()
        .expect("#repo-url is not an input");
    let output = document
        .get_element_by_id("output")
        .expect("no #output element");

    // Wire up the form submit event.
    let cb = Closure::<dyn Fn(Event)>::new(move |e: Event| {
        e.prevent_default();
        let url = input.value().trim().to_string();
        if url.is_empty() {
            return;
        }
        let output = output.clone();
        wasm_bindgen_futures::spawn_local(async move {
            load_repo(url, output).await;
        });
    });

    document
        .get_element_by_id("repo-form")
        .expect("no #repo-form element")
        .add_event_listener_with_callback("submit", cb.as_ref().unchecked_ref())
        .expect("failed to add submit listener");

    // Leak the closure so it stays alive for the lifetime of the page.
    cb.forget();
}
