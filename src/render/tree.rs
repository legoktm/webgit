use crate::cache::CachingRepo;
use crate::route::{encode_component, encode_path};
use gib::object::{ObjectId, Tree, TreeEntryType};
use yew::prelude::*;

fn mode_string(entry_type: TreeEntryType) -> &'static str {
    match entry_type {
        TreeEntryType::Tree => "d---------",
        TreeEntryType::File => "-rw-r--r--",
        TreeEntryType::Executable => "-rwxr-xr-x",
        TreeEntryType::Symlink => "l---------",
        TreeEntryType::Commit => "m---------",
    }
}

#[derive(PartialEq, Clone)]
pub(crate) enum RowKind {
    Dir,
    File,
    Symlink(Option<SymlinkTarget>),
    Submodule(ObjectId),
}

/// A symlink's target: the text git stored, plus where that text lands in this
/// repository — when it lands in it at all.
#[derive(PartialEq, Clone)]
pub(crate) struct SymlinkTarget {
    /// The blob's contents verbatim, which is what gets displayed. A symlink
    /// may point anywhere, so this is shown whether or not it resolves.
    text: String,
    /// The in-repo path `text` names, resolved against the directory the link
    /// sits in; `None` when it names something this view can't show.
    path: Option<String>,
}

/// A single row in the tree listing. Doubles as the per-entry data the
/// component renders and as the unit-test fixture.
#[derive(PartialEq, Clone)]
pub(crate) struct TreeEntryRow {
    mode: String,
    name: String,
    path: String,
    kind: RowKind,
}

fn row_kind(entry_type: TreeEntryType, id: ObjectId) -> RowKind {
    match entry_type {
        TreeEntryType::Tree => RowKind::Dir,
        TreeEntryType::File | TreeEntryType::Executable => RowKind::File,
        TreeEntryType::Symlink => RowKind::Symlink(None),
        TreeEntryType::Commit => RowKind::Submodule(id),
    }
}

fn tree_rows(tree: &Tree, prefix: &str) -> Vec<TreeEntryRow> {
    tree.entries()
        .map(|e| {
            let name = String::from_utf8_lossy(e.name()).into_owned();
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            TreeEntryRow {
                mode: mode_string(e.entry_type()).to_string(),
                kind: row_kind(e.entry_type(), e.id()),
                name,
                path,
            }
        })
        .collect()
}

/// Where a symlink's target lands inside the repository, given `dir` — the path
/// of the tree the link itself sits in — or `None` when it lands outside it.
fn resolve_symlink(dir: &str, target: &str) -> Option<String> {
    if target.is_empty() || target.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    // A target that resolves to the repository root itself: there is a view for
    // it, but an arrow pointing at an empty name reads as breakage.
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Read a symlink's target out of the blob the entry points at.
async fn read_symlink_target(repo: &CachingRepo, id: ObjectId) -> Option<String> {
    let object = repo.lookup_object(id).await.ok()?;
    let blob = object.blob().ok()?;
    Some(String::from_utf8_lossy(blob.data()).into_owned())
}

/// The view inputs for a tree listing: the rows and the `?h=…` suffix appended
/// to every link so navigation stays pinned to the current ref.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct TreeProps {
    pub entries: Vec<TreeEntryRow>,
    pub head_suffix: String,
}

/// Build the listing, filling in symlink targets once they arrive.
pub(crate) async fn build_tree_props(
    tree: &Tree,
    prefix: &str,
    head: Option<&str>,
    repo: &CachingRepo,
    on_partial: impl Fn(TreeProps),
) -> TreeProps {
    let mut props = TreeProps {
        entries: tree_rows(tree, prefix),
        head_suffix: head.map_or(String::new(), |h| format!("?h={}", encode_component(h))),
    };

    // `tree_rows` maps the entries one for one, so an entry's position in the
    // tree is its row's position in `props.entries`.
    let symlinks: Vec<(usize, ObjectId)> = tree
        .entries()
        .enumerate()
        .filter(|(_, e)| e.entry_type() == TreeEntryType::Symlink)
        .map(|(i, e)| (i, e.id()))
        .collect();
    if symlinks.is_empty() {
        return props;
    }
    on_partial(props.clone());

    let targets = futures::future::join_all(
        symlinks
            .iter()
            .map(|&(i, id)| async move { (i, read_symlink_target(repo, id).await) }),
    )
    .await;
    for (i, text) in targets {
        let Some(text) = text else { continue };
        props.entries[i].kind = RowKind::Symlink(Some(SymlinkTarget {
            path: resolve_symlink(prefix, &text),
            text,
        }));
    }
    props
}

/// The Yew component used to mount the tree view into the DOM. The markup lives
/// in the plain `tree_view` function below so it can be unit-tested without a
/// renderer.
#[function_component(TreeView)]
pub(crate) fn tree_view_component(props: &TreeProps) -> Html {
    tree_view(props)
}

pub(crate) fn tree_view(props: &TreeProps) -> Html {
    let TreeProps {
        entries,
        head_suffix,
    } = props;

    html! {
        <table class="tree-table">
            <thead>
                <tr>
                    <th>{ "Mode" }</th>
                    <th>{ "Name" }</th>
                </tr>
            </thead>
            <tbody>
                { for entries.iter().map(|e| tree_row(e, head_suffix)) }
            </tbody>
        </table>
    }
}

fn tree_row(entry: &TreeEntryRow, head_suffix: &str) -> Html {
    let href = format!("#!/tree/{}{}", encode_path(&entry.path), head_suffix);
    html! {
        <tr key={entry.path.clone()}>
            <td class="mode">{ &entry.mode }</td>
            <td class="name">
                { match &entry.kind {
                    RowKind::Dir => html! { <a href={href}>{ &entry.name }</a> },
                    RowKind::File => html! { <a class="file" href={href}>{ &entry.name }</a> },
                    RowKind::Symlink(target) => html! {
                        <>
                            <a class="file" href={href}>{ &entry.name }</a>
                            if let Some(target) = target {
                                { symlink_target(target, head_suffix) }
                            }
                        </>
                    },
                    RowKind::Submodule(id) => submodule(&entry.name, *id),
                } }
            </td>
        </tr>
    }
}

/// The `-> target` half of a symlink row.
fn symlink_target(target: &SymlinkTarget, head_suffix: &str) -> Html {
    html! {
        <>
            <span class="symlink-arrow">{ " -> " }</span>
            { match &target.path {
                Some(path) => {
                    let href = format!("#!/tree/{}{}", encode_path(path), head_suffix);
                    html! { <a class="file" href={href}>{ &target.text }</a> }
                }
                None => html! { <span class="symlink-target">{ &target.text }</span> },
            } }
        </>
    }
}

/// A submodule row: the name, unlinked, and the commit it pins.
/// TODO: support equivalent to cgit's `repo.module-link`
fn submodule(name: &str, id: ObjectId) -> Html {
    html! {
        <>
            <span class="submodule">{ name }</span>
            <span class="submodule-commit">{ format!(" @ {}", super::short_hash(id)) }</span>
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gib::object::{Object, ObjectType, RawObject};

    /// Render `TreeView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines so the snapshot reads as a tree. See the
    /// equivalent helper in `render::tag` for why we go through SSR and why
    /// indentation is omitted.
    fn render(props: TreeProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<TreeView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn test_mode_string() {
        assert_eq!(mode_string(TreeEntryType::Tree), "d---------");
        assert_eq!(mode_string(TreeEntryType::File), "-rw-r--r--");
        assert_eq!(mode_string(TreeEntryType::Executable), "-rwxr-xr-x");
        assert_eq!(mode_string(TreeEntryType::Symlink), "l---------");
        assert_eq!(mode_string(TreeEntryType::Commit), "m---------");
    }

    #[test]
    fn test_resolve_symlink() {
        // Siblings and descents, from the root and from a subdirectory.
        assert_eq!(
            resolve_symlink("", "README.md").as_deref(),
            Some("README.md")
        );
        assert_eq!(
            resolve_symlink("src", "lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            resolve_symlink("src", "render/tree.rs").as_deref(),
            Some("src/render/tree.rs")
        );

        // `.` and `..` fold away, and `..` may climb as far as the root.
        assert_eq!(
            resolve_symlink("src", "./lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(resolve_symlink("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(resolve_symlink("a/b", "../../c/d").as_deref(), Some("c/d"));
        assert_eq!(
            resolve_symlink("a", "../README.md").as_deref(),
            Some("README.md")
        );

        // Nothing this view can show: out of the repository, or the root itself.
        assert_eq!(resolve_symlink("a", "../../escape"), None);
        assert_eq!(resolve_symlink("", "/etc/passwd"), None);
        assert_eq!(resolve_symlink("a/b", "../.."), None);
        assert_eq!(resolve_symlink("src", ""), None);
    }

    /// Assemble a tree object out of `(octal mode, name)` pairs, every entry
    /// pointing at the same made-up id, and parse it back the way a lookup
    /// would. Entry order is git's, i.e. already sorted.
    fn tree_of(entries: &[(&str, &str)]) -> Tree {
        let target = ObjectId::from_hex(b"1a3e64c6c4a6ff9e1d2c3b4a5968776655443322").unwrap();
        let mut body = Vec::new();
        for (mode, name) in entries {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(target.bytes());
        }
        let raw = RawObject {
            object_type: ObjectType::Tree,
            body,
        };
        Object::from_raw(raw.compute_id(), raw)
            .unwrap()
            .tree()
            .unwrap()
    }

    /// Every mode git writes, mapped to the row it produces. The gitlink is the
    /// point: it used to be indistinguishable from a directory here and so got
    /// a link to a page that resolved to nothing.
    #[test]
    fn test_tree_rows_kinds() {
        let tree = tree_of(&[
            ("40000", "dir"),
            ("100644", "file"),
            ("100755", "exe"),
            ("120000", "link"),
            ("160000", "vendor"),
        ]);
        let rows = tree_rows(&tree, "sub");
        let target = ObjectId::from_hex(b"1a3e64c6c4a6ff9e1d2c3b4a5968776655443322").unwrap();

        let kinds: Vec<&RowKind> = rows.iter().map(|r| &r.kind).collect();
        assert!(matches!(kinds[0], RowKind::Dir));
        assert!(matches!(kinds[1], RowKind::File));
        assert!(matches!(kinds[2], RowKind::File));
        // The target is a separate fetch, so it isn't known yet.
        assert!(matches!(kinds[3], RowKind::Symlink(None)));
        assert!(matches!(kinds[4], RowKind::Submodule(id) if *id == target));

        // The prefix is carried into every path, symlinks and gitlinks alike.
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            ["sub/dir", "sub/file", "sub/exe", "sub/link", "sub/vendor"]
        );
    }

    fn row(mode: TreeEntryType, name: &str, path: &str, kind: RowKind) -> TreeEntryRow {
        TreeEntryRow {
            mode: mode_string(mode).to_string(),
            name: name.to_string(),
            path: path.to_string(),
            kind,
        }
    }

    fn fixture_rows() -> Vec<TreeEntryRow> {
        vec![
            row(TreeEntryType::Tree, "src", "src", RowKind::Dir),
            row(TreeEntryType::File, "README.md", "README.md", RowKind::File),
            row(
                TreeEntryType::Executable,
                "build.sh",
                "scripts/build.sh",
                RowKind::File,
            ),
        ]
    }

    /// The rows the two new kinds produce, in each state they can be in: a
    /// submodule, a symlink whose target hasn't arrived yet, one pointing
    /// inside the repository, and one pointing out of it.
    fn special_rows() -> Vec<TreeEntryRow> {
        let submodule_oid =
            ObjectId::from_hex(b"1a3e64c6c4a6ff9e1d2c3b4a5968776655443322").unwrap();
        vec![
            row(
                TreeEntryType::Commit,
                "vendor",
                "vendor",
                RowKind::Submodule(submodule_oid),
            ),
            row(
                TreeEntryType::Symlink,
                "pending",
                "src/pending",
                RowKind::Symlink(None),
            ),
            row(
                TreeEntryType::Symlink,
                "lib.rs",
                "src/lib.rs",
                RowKind::Symlink(Some(SymlinkTarget {
                    text: "../lib.rs".to_string(),
                    path: resolve_symlink("src", "../lib.rs"),
                })),
            ),
            row(
                TreeEntryType::Symlink,
                "outside",
                "src/outside",
                RowKind::Symlink(Some(SymlinkTarget {
                    text: "/etc/passwd".to_string(),
                    path: resolve_symlink("src", "/etc/passwd"),
                })),
            ),
        ]
    }

    #[test]
    fn test_tree_html() {
        insta::assert_snapshot!(render(TreeProps {
            entries: fixture_rows(),
            head_suffix: String::new(),
        }));
    }

    #[test]
    fn test_tree_html_with_head() {
        insta::assert_snapshot!(render(TreeProps {
            entries: fixture_rows(),
            head_suffix: "?h=stable".to_string(),
        }));
    }

    #[test]
    fn test_tree_html_submodule_and_symlinks() {
        insta::assert_snapshot!(render(TreeProps {
            entries: special_rows(),
            head_suffix: String::new(),
        }));
    }

    #[test]
    fn test_tree_html_submodule_and_symlinks_with_head() {
        insta::assert_snapshot!(render(TreeProps {
            entries: special_rows(),
            head_suffix: "?h=stable".to_string(),
        }));
    }
}
