use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::about::{AboutProps, build_about};
use crate::render::blob::{BlobProps, build_blob_props};
use crate::render::commit::{CommitProps, build_commit};
use crate::render::log::{LogProps, build_log};
use crate::render::readme::{ReadmeProps, build_readme};
use crate::render::refs_all::{RefsAllProps, build_refs_all};
use crate::render::refs_heads::{RefsHeadsProps, build_refs_heads};
use crate::render::refs_tags::{RefsTagsProps, build_refs_tags};
use crate::render::snapshot::{SnapshotProps, build_snapshot};
use crate::render::summary::{SummaryProps, build_summary};
use crate::render::tag::{TagProps, build_tag};
use crate::render::tree::{TreeProps, build_tree_props};
use crate::render::{commit_for_entry, head_branch_name};
use git_async::object::{ObjectId, Tree, TreeEntryType};
use git_async::reference::RefName;
use std::rc::Rc;

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

/// Resolve `path` to a blob and hand back its bytes.
///
/// There is deliberately no size cap here. A loose object is a single zlib
/// stream that has to be inflated in full before anything can be read out of
/// it, so the whole blob is already in memory by the time this returns and an
/// early bail would save nothing. The cap lives where the expense actually is:
/// `build_blob_props` decides what to render before copying or splitting.
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
    Readme,
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
    /// A `.tar.gz` of a ref's tree (HEAD's, when there is no `?h=`), built on
    /// arrival. A route rather than a button because building one is exactly
    /// what every other route does — an async walk over the repo that resolves
    /// into props — and this way it gets the loading, error and cancel-on-
    /// navigate handling already wired up around [`build_route`].
    Snapshot {
        head: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Percent-encoding
// ---------------------------------------------------------------------------
//
// Ref names and path components are repository data, not identifiers we choose:
// git allows `?`, `#`, `%` and `&` in both, and a branch called `foo?bar` built
// straight into `#!/tree?h=foo?bar` is parsed back as the ref `foo` — the link
// silently goes somewhere else. So every value interpolated into a route is
// encoded on the way out and decoded on the way back in.
//
// Not `js_sys::encode_uri_component`: it only exists in the browser, and the
// whole routing grammar is covered by native tests.

/// Percent-encode one route component: a ref name, or a single path segment.
///
/// Only what would otherwise read as route syntax is escaped — `%` (the escape
/// itself, so that encoding round-trips), `#` and `?` (which would end the
/// fragment or open a query), `&` (which separates query parameters), `/` (the
/// path separator) and space plus control characters, which a browser would
/// rewrite in the address bar regardless. Everything else, non-ASCII included,
/// stays legible: [`decode_component`] accepts whatever additional escaping the
/// browser applies on its own.
pub(crate) fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '#' | '?' | '&' | '/' | ' ') || c.is_control() {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Decode a percent-encoded route component.
///
/// Escapes are resolved to bytes and the result decoded as UTF-8, so a
/// multi-byte character split across several `%XX` (how a browser encodes
/// non-ASCII in `location.hash`) is reassembled. A `%` that doesn't begin a
/// valid escape is passed through as itself rather than rejected — the hash is
/// user-editable, and a literal `%` in it should still name the obvious thing.
pub(crate) fn decode_component(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode a slash-separated path, one component at a time, so the separators
/// survive as separators while anything inside a component that looks like one
/// is escaped.
pub(crate) fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Reverse of [`encode_path`].
fn decode_path(path: &str) -> String {
    path.split('/')
        .map(decode_component)
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn parse_hash(hash: &str) -> Route {
    // most likely scenario
    if hash == "#!/summary" || hash.is_empty() || hash == "#" {
        return Route::Summary;
    }
    if hash == "#!/about" {
        return Route::About;
    }
    if hash == "#!/readme" {
        return Route::Readme;
    }

    if let Some(rest) = hash.strip_prefix("#!/log") {
        // rest is one of: "", "?query", "/path", or "/path?query".
        let (path_part, query_string) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let path = decode_path(path_part.trim_start_matches('/'));
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

    if let Some(rest) = hash.strip_prefix("#!/snapshot") {
        // Only the ref matters here: a snapshot is always of a whole tree, so
        // anything in the path position is ignored rather than 404'd.
        let (_, head) = parse_tree_rest(rest);
        return Route::Snapshot { head };
    }

    if hash.starts_with("#!/refs") {
        let subroute = if hash == "#!/refs/tags" {
            RefsRoute::Tags
        } else if hash == "#!/refs/heads" {
            RefsRoute::Heads
        } else if let Some(tag) = hash.strip_prefix("#!/refs/tags/") {
            // A tag name may contain '/', so the whole remainder is the name;
            // it's decoded as one component, not split into path segments.
            RefsRoute::Tag(decode_component(tag))
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
            .map(decode_component)
    });
    (decode_path(path_part), head)
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
            head = Some(decode_component(v));
        }
    }
    (offset, head)
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RefKind {
    Tag,
    Branch,
}

/// The nav tab a route lives under, used for the `active` highlight.
pub(crate) fn active_tab(route: &Route) -> &'static str {
    match route {
        Route::About => "#!/about",
        Route::Readme => "#!/readme",
        Route::Summary => "#!/summary",
        Route::Log { .. } => "#!/log",
        Route::CommitHead | Route::Commit(_) => "#!/commit",
        Route::Refs(_) => "#!/refs",
        // A snapshot is an action on the tree being browsed, so the tree tab
        // stays lit while one is being built.
        Route::Tree { .. } | Route::Snapshot { .. } => "#!/tree",
    }
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

/// The ref name + kind shown in the path bar / log header: the explicit `?h=`
/// ref (with its resolved kind), or the implicit HEAD branch. `None` if it
/// can't be resolved — the content view reports the real error.
pub(crate) async fn resolve_display_head(
    repo: &CachingRepo,
    head: Option<&str>,
) -> Option<(String, RefKind)> {
    match head {
        Some(name) => resolve_ref_to_commit(repo, name)
            .await
            .ok()
            .map(|(_, kind)| (name.to_string(), kind)),
        None => head_branch_name(repo).await.map(|n| (n, RefKind::Branch)),
    }
}

/// A route's resolved content, ready to render. The chrome (nav, path bar) is
/// handled separately by `RouteView`/`NavBar` in `lib.rs`.
pub(crate) enum LoadedView {
    About(AboutProps),
    Readme(ReadmeProps),
    Summary(SummaryProps),
    Log(LogProps),
    Commit(CommitProps),
    RefsHeads(RefsHeadsProps),
    RefsTags(RefsTagsProps),
    RefsAll(RefsAllProps),
    Tag(TagProps),
    Tree(TreeProps),
    Blob(BlobProps),
    Snapshot(SnapshotProps),
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
    repo_name: &Rc<String>,
    on_partial: &dyn Fn(LoadedView),
) -> anyhow::Result<LoadedView> {
    match parse_hash(hash) {
        Route::About => Ok(LoadedView::About(build_about(repo, clone_url).await)),
        // The README always comes from HEAD's tree, never a `?h=` ref.
        Route::Readme => Ok(LoadedView::Readme(build_readme(root_tree, repo).await)),
        Route::Summary => Ok(LoadedView::Summary(
            build_summary(head_commit, repo, clone_url, repo_name, |p| {
                on_partial(LoadedView::Summary(p))
            })
            .await,
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
                build_log(log_commit, repo, &path, offset, head.as_deref(), |p| {
                    on_partial(LoadedView::Log(p))
                })
                .await,
            ))
        }
        Route::CommitHead => Ok(LoadedView::Commit(
            build_commit(repo, &format!("{}", head_commit.id()), |p| {
                on_partial(LoadedView::Commit(p))
            })
            .await?,
        )),
        Route::Commit(sha) => Ok(LoadedView::Commit(
            build_commit(repo, &sha, |p| on_partial(LoadedView::Commit(p))).await?,
        )),
        Route::Refs(RefsRoute::Heads) => Ok(LoadedView::RefsHeads(build_refs_heads(repo).await)),
        Route::Refs(RefsRoute::Tags) => {
            Ok(LoadedView::RefsTags(build_refs_tags(repo, repo_name).await))
        }
        Route::Refs(RefsRoute::All) => {
            Ok(LoadedView::RefsAll(build_refs_all(repo, repo_name).await))
        }
        Route::Refs(RefsRoute::Tag(tag)) => {
            Ok(LoadedView::Tag(build_tag(repo, tag, repo_name).await?))
        }
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
                Ok(LoadedView::Blob(build_blob_props(id, &path, data)))
            } else {
                Ok(LoadedView::NotFound(path))
            }
        }
        Route::Snapshot { head } => {
            // Both the commit and its tree, where the tree route needs only the
            // tree: the commit's id and date go into the archive itself.
            let resolved_commit;
            let commit: &git_async::object::Commit = match &head {
                Some(name) => {
                    resolved_commit = resolve_ref_to_commit(repo, name).await?.0;
                    &resolved_commit
                }
                None => head_commit,
            };
            let resolved_tree;
            let tree: &Tree = if head.is_some() {
                resolved_tree = repo
                    .lookup_object(commit.tree())
                    .await
                    .context("lookup tree to archive")?
                    .tree()
                    .map_err(git_async::error::Error::from)
                    .context("expected a tree to archive")?;
                &resolved_tree
            } else {
                root_tree
            };

            // What the archive is named after: the ref asked for, the branch
            // HEAD is on, or — if HEAD is detached — the commit itself.
            let ref_label = match &head {
                Some(name) => name.clone(),
                None => match head_branch_name(repo).await {
                    Some(name) => name,
                    None => commit.id().to_string()[..8].to_string(),
                },
            };
            Ok(LoadedView::Snapshot(
                build_snapshot(repo, tree, commit, &ref_label, repo_name, &|p| {
                    on_partial(LoadedView::Snapshot(p))
                })
                .await?,
            ))
        }
    }
}

/// The URL of a ref's `.tar.gz` — the link on the tag rows and the tag page.
/// Like [`log_url`], the ref name passed in is the real (decoded) one; the
/// encoding happens here.
pub(crate) fn snapshot_url(head: &str) -> String {
    format!("#!/snapshot?h={}", encode_component(head))
}

/// The URL for a log view. `path` and `head` are the decoded values (a real
/// path, a real ref name); both are encoded here, so callers pass what they
/// have rather than remembering to escape it.
pub(crate) fn log_url(path: &str, offset: usize, head: Option<&str>) -> String {
    let base = if path.is_empty() {
        "#!/log".to_string()
    } else {
        format!("#!/log/{}", encode_path(path))
    };
    let head = head.map(encode_component);
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
    fn test_parse_hash_readme() {
        assert!(matches!(parse_hash("#!/readme"), Route::Readme));
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
    fn test_parse_hash_snapshot() {
        assert!(matches!(
            parse_hash("#!/snapshot"),
            Route::Snapshot { head: None }
        ));
        assert!(matches!(
            parse_hash("#!/snapshot?h=v1.0.0"),
            Route::Snapshot { head: Some(head) } if head == "v1.0.0"
        ));
        // A ref with a '/' in it survives the round trip through the link.
        assert!(matches!(
            parse_hash(&snapshot_url("release/2.0")),
            Route::Snapshot { head: Some(head) } if head == "release/2.0"
        ));
    }

    #[test]
    fn test_snapshot_url() {
        assert_eq!(snapshot_url("v1.0.0"), "#!/snapshot?h=v1.0.0");
        assert_eq!(
            snapshot_url("release/2.0"),
            "#!/snapshot?h=release%2F2.0",
            "a slash in a ref name is encoded, not left to split the route"
        );
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

    // --- Percent-encoding ---------------------------------------------------

    #[test]
    fn test_encode_component_escapes_only_route_syntax() {
        // Ordinary names are left exactly as they are, so URLs stay readable.
        assert_eq!(encode_component("main"), "main");
        assert_eq!(encode_component("v1.0.0"), "v1.0.0");
        assert_eq!(encode_component("foo(1)+bar,baz=qux"), "foo(1)+bar,baz=qux");
        // The characters that carry meaning in the route grammar.
        assert_eq!(encode_component("a%b"), "a%25b");
        assert_eq!(encode_component("a#b"), "a%23b");
        assert_eq!(encode_component("a?b"), "a%3Fb");
        assert_eq!(encode_component("a&b"), "a%26b");
        assert_eq!(encode_component("a/b"), "a%2Fb");
        assert_eq!(encode_component("a b"), "a%20b");
        assert_eq!(encode_component("a\tb"), "a%09b");
        // Non-ASCII stays legible; the browser escapes it if it needs to, and
        // the decoder accepts either form.
        assert_eq!(encode_component("café"), "café");
    }

    #[test]
    fn test_decode_component() {
        assert_eq!(decode_component("main"), "main");
        assert_eq!(decode_component("a%2Fb"), "a/b");
        assert_eq!(decode_component("a%3fb"), "a?b", "lowercase hex too");
        // Multi-byte UTF-8 split across escapes, as a browser writes it.
        assert_eq!(decode_component("caf%C3%A9"), "café");
        // A '%' that doesn't begin a valid escape is itself, not an error.
        assert_eq!(decode_component("100%"), "100%");
        assert_eq!(decode_component("50%off"), "50%off");
        assert_eq!(decode_component("%zz"), "%zz");
        assert_eq!(decode_component("%2"), "%2");
    }

    #[test]
    fn test_encode_path_keeps_separators() {
        // Separators survive; a '/' *inside* a component would not.
        assert_eq!(encode_path("src/render/mod.rs"), "src/render/mod.rs");
        assert_eq!(encode_path("docs/a?b.md"), "docs/a%3Fb.md");
        assert_eq!(decode_path("docs/a%3Fb.md"), "docs/a?b.md");
        assert_eq!(encode_path(""), "");
    }

    #[test]
    fn test_encoding_round_trips_through_the_router() {
        // The point of the exercise: a ref or path containing route syntax has
        // to come back out of `parse_hash` as the name it went in as.
        for name in ["feature/x", "foo?bar", "a&b", "100%", "release #2", "café"] {
            let url = log_url("", 0, Some(name));
            match parse_hash(&url) {
                Route::Log { head: Some(h), .. } => assert_eq!(h, name, "via {url}"),
                _ => panic!("expected a log route with a head from {url}"),
            }
        }
        for path in ["src/a?b.rs", "docs/50%off.md", "a&b/c#d"] {
            let url = log_url(path, 0, None);
            match parse_hash(&url) {
                Route::Log { path: p, .. } => assert_eq!(p, path, "via {url}"),
                _ => panic!("expected a log route from {url}"),
            }
        }
    }

    #[test]
    fn test_tree_and_tag_routes_decode() {
        // `?h=` on a tree route, and a tag name that itself contains a slash
        // (encoded, so it stays one name rather than becoming path segments).
        assert_eq!(
            parse_tree_rest("/src/a%3Fb.rs?h=foo%26bar"),
            ("src/a?b.rs".into(), Some("foo&bar".into()))
        );
        match parse_hash("#!/refs/tags/release%2F2.0") {
            Route::Refs(RefsRoute::Tag(name)) => assert_eq!(name, "release/2.0"),
            _ => panic!("expected a tag route"),
        }
    }

    #[test]
    fn test_offset_is_not_confused_by_an_encoded_ref() {
        // A ref named "x&offset=999" must not be able to inject a second
        // parameter: encoding hides the '&' from the query splitter.
        let url = log_url("", 10, Some("x&offset=999"));
        match parse_hash(&url) {
            Route::Log { offset, head, .. } => {
                assert_eq!(offset, 10);
                assert_eq!(head.as_deref(), Some("x&offset=999"));
            }
            _ => panic!("expected a log route"),
        }
    }
}
