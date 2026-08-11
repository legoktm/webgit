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
use gib::object::{ObjectId, Tree, TreeEntryType};
use gib::reference::RefName;
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
        /// Show a markdown blob rendered rather than as source (`?render=1`).
        /// Ignored when the path resolves to anything else.
        render: bool,
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

/// Strip `prefix` off `hash`, but only when the prefix ends where a route name
/// is allowed to end: at one of `seps`, or at the end of the hash.
///
/// A plain `strip_prefix` matches mid-word, so `#!/logout` would parse as the
/// log of a path named `out` and `#!/treex` as the tree of `x`, each rendering
/// an empty page for a route nobody asked for. Requiring the boundary is what
/// lets an unrecognised route reach the summary fallback instead.
fn strip_route_prefix<'a>(hash: &'a str, prefix: &str, seps: &[char]) -> Option<&'a str> {
    let rest = hash.strip_prefix(prefix)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if seps.contains(&c) => Some(rest),
        _ => None,
    }
}

/// Parse `location.hash` into the route it names.
///
/// The grammar, where `<…>` is percent-encoded ([`encode_component`]) and every
/// route name must be followed by `/`, `?` or the end of the hash:
///
/// ```text
/// (empty) | #  | #!/summary        the summary
/// #!/about                         the about page
/// #!/readme                        the README at HEAD
/// #!/log[/<path>][?…]              the log; query: h=<rev>, offset=<n>
/// #!/commit[/]                     HEAD's commit
/// #!/commit/<sha>                  one commit
/// #!/refs[/]                       all refs
/// #!/refs/heads[/]                 the branch list
/// #!/refs/tags[/]                  the tag list
/// #!/refs/tags/<tag>               one tag
/// #!/tree[/<path>][?…]             the tree, or a blob; query: h=<rev>, render=1
/// #!/snapshot[/…][?h=<ref>]        a .tar.gz of a revision's tree (path ignored)
/// ```
///
/// `h=<rev>` is a branch, a tag, or a full 40-character commit hash; see
/// [`resolve_revision`], which is where the distinction is drawn. To the grammar
/// it is one opaque string either way.
///
/// Anything else falls back to the summary, so a hand-edited or stale URL lands
/// on a real page rather than an error.
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

    if let Some(rest) = strip_route_prefix(hash, "#!/log", &['/', '?']) {
        // rest is one of: "", "?query", "/path", or "/path?query".
        let (path_part, query_string) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let path = decode_path(path_part.trim_start_matches('/'));
        let (offset, head) = parse_log_query(query_string);
        return Route::Log { offset, head, path };
    }

    // No query on this route, so the whole remainder is the id; an empty one
    // (`#!/commit` or `#!/commit/`) means HEAD's commit.
    if let Some(rest) = strip_route_prefix(hash, "#!/commit", &['/']) {
        let sha = rest.trim_start_matches('/');
        return if sha.is_empty() {
            Route::CommitHead
        } else {
            Route::Commit(sha.to_string())
        };
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/tree", &['/', '?']) {
        let (path, head, render) = parse_tree_rest(rest);
        return Route::Tree { path, head, render };
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/snapshot", &['/', '?']) {
        // Only the ref matters here: a snapshot is always of a whole tree, so
        // anything in the path position is ignored rather than 404'd, and so is
        // a `render=1` that came along with it.
        let (_, head, _) = parse_tree_rest(rest);
        return Route::Snapshot { head };
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/refs", &['/']) {
        // A listing prefix with nothing left after it names the listing, with
        // or without the trailing slash a browser or a hand-typed URL may leave
        // behind: `#!/refs/tags/` is the tag list, not a tag with no name.
        let subroute = match rest {
            "" | "/" => RefsRoute::All,
            "/heads" | "/heads/" => RefsRoute::Heads,
            "/tags" | "/tags/" => RefsRoute::Tags,
            // A tag name may contain '/', so the whole remainder is the name;
            // it's decoded as one component, not split into path segments.
            _ => match rest.strip_prefix("/tags/") {
                Some(tag) => RefsRoute::Tag(decode_component(tag)),
                None => RefsRoute::All,
            },
        };
        return Route::Refs(subroute);
    }

    // fallback to summary on invalid routes
    Route::Summary
}

fn parse_tree_rest(rest: &str) -> (String, Option<String>, bool) {
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
    // A flag, so only the spelling [`tree_url`] writes counts as asking for it.
    let render = query_string.is_some_and(|qs| qs.split('&').any(|part| part == "render=1"));
    (decode_path(path_part), head, render)
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
    /// A commit named directly by its hash in `?h=`, belonging to no branch or
    /// tag we resolved it through.
    Commit,
}

/// The abbreviated form of a hash, as shown wherever a full one would crowd out
/// what surrounds it. Eight characters, matching the commit view's parent links
/// and the snapshot of a detached HEAD.
///
/// Truncation is by character, not by byte slice: every hash reaching this is 40
/// hex digits, but a helper that panics on a short input is a trap for the next
/// caller.
fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
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

/// Resolve a `?h=` value to the commit it names: a tag, a branch, or a commit
/// given as a full 40-character hash.
///
/// Refs are consulted first, and cost nothing to consult — the ref snapshot is
/// fetched once per session and this is a lookup in it — so every link the app
/// writes for itself resolves without a request of its own. Only a value naming
/// no ref is read as an object id.
///
/// Full hashes only. Expanding an abbreviation means searching every pack index
/// and can come back ambiguous ([`CachingRepo::resolve_prefix`]), which is a
/// different failure to report and a different label for the path bar to carry;
/// `#!/commit/<sha>` takes that on because commit messages quote abbreviations,
/// and `?h=` has no such source of them.
///
/// The ref-before-hash order is the reverse of `git rev-parse`, which reads a
/// full hash as an object before it consults refs. It matters only for a ref
/// whose own name is 40 hex digits, and this way every `?h=` URL that resolved
/// before still resolves to exactly what it did.
async fn resolve_revision(
    repo: &CachingRepo,
    name: &str,
) -> anyhow::Result<(gib::object::Commit, RefKind)> {
    let refs = repo.all_refs().await.context("list refs")?;
    let tags_ref = RefName::Ref(format!("tags/{name}").into_bytes());
    if let Some(entry) = refs.get(&tags_ref)
        && let Some(commit) = commit_for_entry(entry, repo).await
    {
        return Ok((commit, RefKind::Tag));
    }
    let heads_ref = RefName::Ref(format!("heads/{name}").into_bytes());
    if let Some(entry) = refs.get(&heads_ref) {
        let commit = commit_for_entry(entry, repo)
            .await
            .ok_or_else(|| anyhow::anyhow!("ref {name} does not point to a commit"))?;
        return Ok((commit, RefKind::Branch));
    }

    let oid = ObjectId::from_hex(name.as_bytes()).ok_or_else(|| {
        anyhow::anyhow!("not a branch, a tag, or a full 40-character commit hash: {name}")
    })?;
    let object = repo
        .lookup_object(oid)
        .await
        .context(format!("lookup {name}"))?;
    // A hash may name an annotated tag object as readily as a commit — the tag
    // pages link to tags by name, but a hash copied out of the refs listing is
    // whatever that row pointed at — so peel before deciding it isn't a commit.
    let commit = repo
        .peel_to_commit(&object)
        .await
        .context(format!("peel {name}"))?
        .ok_or_else(|| anyhow::anyhow!("{name} is not a commit"))?;
    Ok((commit, RefKind::Commit))
}

/// The label + kind shown in the path bar / log header: the explicit `?h=`
/// revision, or the implicit HEAD branch. `None` if it can't be resolved — the
/// content view reports the real error.
///
/// A ref is labelled by the name that was asked for. A commit is labelled by the
/// short hash of what it resolved *to*, which is the same thing for a hash that
/// named a commit and the more useful one for a hash that named a tag object.
/// The URL keeps all 40 characters either way: they are what makes the link
/// stable, but spelled out in the path bar they crowd out the breadcrumb.
pub(crate) async fn resolve_display_head(
    repo: &CachingRepo,
    head: Option<&str>,
) -> Option<(String, RefKind)> {
    match head {
        Some(name) => {
            let (commit, kind) = resolve_revision(repo, name).await.ok()?;
            let label = match kind {
                RefKind::Tag | RefKind::Branch => name.to_string(),
                RefKind::Commit => short_hash(&format!("{}", commit.id())),
            };
            Some((label, kind))
        }
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
    head_commit: &gib::object::Commit,
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
            let log_commit: &gib::object::Commit = match &head {
                Some(name) => {
                    resolved = resolve_revision(repo, name).await?.0;
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
        Route::Tree { path, head, render } => {
            let resolved_tree;
            let tree: &Tree = if let Some(ref ref_name) = head {
                let (commit, _kind) = resolve_revision(repo, ref_name).await?;
                resolved_tree = repo
                    .lookup_object(commit.tree())
                    .await
                    .context(format!("lookup tree for {ref_name}"))?
                    .tree()
                    .map_err(gib::error::Error::from)
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
                Ok(LoadedView::Blob(build_blob_props(
                    id,
                    &path,
                    data,
                    head.as_deref(),
                    render,
                )))
            } else {
                Ok(LoadedView::NotFound(path))
            }
        }
        Route::Snapshot { head } => {
            // Both the commit and its tree, where the tree route needs only the
            // tree: the commit's id and date go into the archive itself.
            let resolved_commit;
            let mut resolved_kind = None;
            let commit: &gib::object::Commit = match &head {
                Some(name) => {
                    let (commit, kind) = resolve_revision(repo, name).await?;
                    resolved_commit = commit;
                    resolved_kind = Some(kind);
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
                    .map_err(gib::error::Error::from)
                    .context("expected a tree to archive")?;
                &resolved_tree
            } else {
                root_tree
            };

            // What the archive is named after: the ref asked for, the branch
            // HEAD is on, or — for a `?h=` that named a commit outright, and for
            // a detached HEAD — the commit itself, abbreviated. All 40 digits in
            // a filename tell the reader nothing the first eight don't.
            let ref_label = match (&head, resolved_kind) {
                (Some(_), Some(RefKind::Commit)) => short_hash(&format!("{}", commit.id())),
                (Some(name), _) => name.clone(),
                (None, _) => match head_branch_name(repo).await {
                    Some(name) => name,
                    None => short_hash(&format!("{}", commit.id())),
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

/// The URL for a tree view — a directory listing, or a blob. `path` and `head`
/// are the decoded values (a real path, a real ref name); both are encoded
/// here. `render` asks for a markdown blob's rendered form.
pub(crate) fn tree_url(path: &str, head: Option<&str>, render: bool) -> String {
    let base = if path.is_empty() {
        "#!/tree".to_string()
    } else {
        format!("#!/tree/{}", encode_path(path))
    };
    let head = head.map(encode_component);
    match (head, render) {
        (None, false) => base,
        (None, true) => format!("{base}?render=1"),
        (Some(head), false) => format!("{base}?h={head}"),
        (Some(head), true) => format!("{base}?h={head}&render=1"),
    }
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

    /// An empty id is not a commit to look up, so the bare route's meaning
    /// (HEAD's commit) survives a trailing slash.
    #[test]
    fn test_parse_hash_commit_trailing_slash() {
        assert!(matches!(parse_hash("#!/commit/"), Route::CommitHead));
    }

    #[test]
    fn test_parse_hash_tree() {
        assert!(matches!(
            parse_hash("#!/tree"),
            Route::Tree { path, head: None, render: false } if path.is_empty()
        ));
        assert!(matches!(
            parse_hash("#!/tree/src/main.rs"),
            Route::Tree { path, head: None, render: false } if path == "src/main.rs"
        ));
    }

    #[test]
    fn test_parse_hash_tree_render() {
        assert!(matches!(
            parse_hash("#!/tree/docs/setup.md?render=1"),
            Route::Tree { path, head: None, render: true } if path == "docs/setup.md"
        ));
        assert!(matches!(
            parse_hash("#!/tree/docs/setup.md?h=v1&render=1"),
            Route::Tree { path, head: Some(head), render: true }
                if path == "docs/setup.md" && head == "v1"
        ));
        // Only the flag as written by `tree_url` asks for it.
        for hash in [
            "#!/tree/a.md",
            "#!/tree/a.md?render=0",
            "#!/tree/a.md?render",
            "#!/tree/a.md?h=render=1",
        ] {
            assert!(
                matches!(parse_hash(hash), Route::Tree { render: false, .. }),
                "{hash}"
            );
        }
    }

    /// A snapshot is of a whole tree, so the flag means nothing there and must
    /// not stop the ref from being read.
    #[test]
    fn test_parse_hash_snapshot_ignores_render() {
        assert!(matches!(
            parse_hash("#!/snapshot?h=v1&render=1"),
            Route::Snapshot { head: Some(head) } if head == "v1"
        ));
    }

    #[test]
    fn test_tree_url() {
        assert_eq!(tree_url("", None, false), "#!/tree");
        assert_eq!(tree_url("docs/a.md", None, false), "#!/tree/docs/a.md");
        assert_eq!(
            tree_url("docs/a.md", None, true),
            "#!/tree/docs/a.md?render=1"
        );
        assert_eq!(
            tree_url("docs/a.md", Some("main"), false),
            "#!/tree/docs/a.md?h=main"
        );
        assert_eq!(
            tree_url("docs/a.md", Some("release/2.0"), true),
            "#!/tree/docs/a.md?h=release%2F2.0&render=1"
        );
    }

    /// The round trip that matters for the rendered view: a path and a ref that
    /// contain route syntax come back out of the router unchanged, flag intact.
    #[test]
    fn test_tree_url_round_trips_through_the_router() {
        let url = tree_url("docs/a?b.md", Some("x&render=1"), true);
        match parse_hash(&url) {
            Route::Tree { path, head, render } => {
                assert_eq!(path, "docs/a?b.md");
                assert_eq!(head.as_deref(), Some("x&render=1"));
                assert!(render);
            }
            _ => panic!("expected a tree route from {url}"),
        }
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
        assert_eq!(parse_tree_rest(""), ("".into(), None, false));
        assert_eq!(parse_tree_rest("/src"), ("src".into(), None, false));
        assert_eq!(
            parse_tree_rest("?h=main"),
            ("".into(), Some("main".into()), false)
        );
        assert_eq!(
            parse_tree_rest("/src?h=stable"),
            ("src".into(), Some("stable".into()), false)
        );
        assert_eq!(parse_tree_rest("?h="), ("".into(), None, false));
        assert_eq!(
            parse_tree_rest("/a.md?render=1"),
            ("a.md".into(), None, true)
        );
    }

    #[test]
    fn test_short_hash() {
        assert_eq!(
            short_hash("6121d0b97779278fcc32cc8a02754e7c588d9c18"),
            "6121d0b9"
        );
        // Shorter than the abbreviation: itself, not a panic.
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }

    /// A full hash in `?h=` is 40 hex digits, none of which the encoder touches,
    /// so it reaches the router as itself and the URL stays readable. The router
    /// draws no distinction between a hash and a ref name — `resolve_revision`
    /// is where that is decided — and this pins that it doesn't have to.
    #[test]
    fn test_h_takes_a_full_hash_verbatim() {
        let sha = "6121d0b97779278fcc32cc8a02754e7c588d9c18";
        assert_eq!(
            tree_url("src", Some(sha), false),
            format!("#!/tree/src?h={sha}")
        );
        assert_eq!(log_url("", 0, Some(sha)), format!("#!/log?h={sha}"));
        assert_eq!(snapshot_url(sha), format!("#!/snapshot?h={sha}"));
        match parse_hash(&tree_url("src", Some(sha), false)) {
            Route::Tree { path, head, .. } => {
                assert_eq!(path, "src");
                assert_eq!(head.as_deref(), Some(sha));
            }
            _ => panic!("expected a tree route"),
        }
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

    /// A listing prefix with an empty remainder is still the listing: the
    /// trailing slash must not turn `#!/refs/tags/` into a tag with no name,
    /// which resolves to nothing and renders an error page.
    #[test]
    fn test_parse_hash_refs_listings_tolerate_a_trailing_slash() {
        assert!(
            matches!(parse_hash("#!/refs/tags/"), Route::Refs(RefsRoute::Tags)),
            "#!/refs/tags/"
        );
        assert!(
            matches!(parse_hash("#!/refs/heads/"), Route::Refs(RefsRoute::Heads)),
            "#!/refs/heads/"
        );
        assert!(
            matches!(parse_hash("#!/refs/"), Route::Refs(RefsRoute::All)),
            "#!/refs/"
        );
    }

    /// Everything under `#!/refs` that names no listing we have — including a
    /// branch, which has no page of its own — is the combined listing.
    #[test]
    fn test_parse_hash_refs_unknown_subroute_is_the_all_listing() {
        for hash in ["#!/refs/bogus", "#!/refs/heads/main", "#!/refs/tagsy"] {
            assert!(
                matches!(parse_hash(hash), Route::Refs(RefsRoute::All)),
                "{hash}"
            );
        }
    }

    /// A route name only matches when it ends at a separator or at the end of
    /// the hash. Without that check `#!/logout` is the log of a path named
    /// `out` and `#!/treex` the tree of `x`, both empty pages for a route that
    /// was never requested; the grammar says they are unknown routes.
    #[test]
    fn test_parse_hash_prefix_needs_a_separator() {
        for hash in [
            "#!/logout",
            "#!/logs",
            "#!/treex",
            "#!/trees",
            "#!/snapshots",
            "#!/commits",
            "#!/commitment",
            "#!/refsall",
            "#!/summaryx",
            "#!/aboutus",
            "#!/readmes",
            "#!/nonsense",
        ] {
            assert!(matches!(parse_hash(hash), Route::Summary), "{hash}");
        }
    }

    /// The boundary check must not cost the routes that legitimately continue
    /// with a path or a query.
    #[test]
    fn test_parse_hash_prefix_matches_at_a_separator() {
        assert!(matches!(parse_hash("#!/log/src"), Route::Log { .. }));
        assert!(matches!(parse_hash("#!/log?h=main"), Route::Log { .. }));
        assert!(matches!(parse_hash("#!/tree/src"), Route::Tree { .. }));
        assert!(matches!(parse_hash("#!/tree?h=main"), Route::Tree { .. }));
        assert!(matches!(
            parse_hash("#!/snapshot?h=v1"),
            Route::Snapshot { head: Some(_) }
        ));
        assert!(matches!(parse_hash("#!/commit/abc"), Route::Commit(_)));
        assert!(matches!(parse_hash("#!/refs/tags"), Route::Refs(_)));
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
            ("src/a?b.rs".into(), Some("foo&bar".into()), false)
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
