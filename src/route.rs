//! The URL grammar: parsing `location.hash` into a [`Route`], and building the
//! hashes that link back to one.
//!
//! The pieces a route is made of live alongside: [`encode`] for the
//! percent-encoding every interpolated value goes through, [`anchor`] for the
//! `#n5` line selection, [`diff_view`] for the commit view's query parameters,
//! and [`load`] for turning a parsed route into rendered props.

mod anchor;
mod diff_view;
mod encode;
mod load;

pub(crate) use anchor::{LineRange, split_line_anchor};
pub(crate) use diff_view::{CONTEXT_CHOICES, DiffMode, DiffView};
pub(crate) use encode::{encode_component, encode_path};
pub(crate) use load::{LoadedView, RefKind, build_route, resolve_display_head};

use encode::{decode_component, decode_path};

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
        showmsg: bool,
    },
    CommitHead(DiffView),
    Commit(String, DiffView),
    Refs(RefsRoute),
    Tree {
        path: String,
        head: Option<String>,
        /// Show a blob rendered rather than as source (`?render=1`) — markdown
        /// as a document, SVG as a picture. Ignored when the path resolves to
        /// anything else.
        render: bool,
    },
    /// Per-line blame for one file: which commit last touched each line.
    /// `head` is the revision blamed from, as everywhere else.
    Blame {
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

/// Strip `prefix` off `hash`, but only when the prefix ends where a route name
/// is allowed to end: at one of `seps`, or at the end of the hash.
///
/// A plain `strip_prefix` matches mid-word, so `#!/logout` would parse as the
/// log of a path named `out` and `#!/treex` as the tree of `x`, each rendering
/// an empty page for a route nobody asked for. Requiring the boundary is what
/// lets an unrecognised route reach the readme fallback instead.
fn strip_route_prefix<'a>(hash: &'a str, prefix: &str, seps: &[char]) -> Option<&'a str> {
    let rest = hash.strip_prefix(prefix)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if seps.contains(&c) => Some(rest),
        _ => None,
    }
}

/// A route on the repository index. Its own grammar rather than a corner of
/// [`Route`]: the index and a repository are disjoint modes, chosen from the URL
/// before either hash is read, and neither one's routes exist in the other.
///
/// ```text
/// #!/index[/<section>]             the listing, scrolled to a section
/// #!/about                         the viewer-wide about page
/// ```
pub(crate) enum IndexRoute {
    /// The listing. `section` is the prefix heading to scroll to, empty for the
    /// top of the page.
    Listing {
        section: String,
    },
    About,
}

/// The hash naming a section of the listing (the whole listing, for an empty
/// `section`).
pub(crate) fn index_url(section: &str) -> String {
    if section.is_empty() {
        "#!/index".to_string()
    } else {
        format!("#!/index/{}", encode_path(section))
    }
}

/// Parse `location.hash` on the repository index; see [`IndexRoute`].
pub(crate) fn parse_index_hash(hash: &str) -> IndexRoute {
    if hash == "#!/about" {
        return IndexRoute::About;
    }
    let section = strip_route_prefix(hash, "#!/index", &['/'])
        .map(|rest| decode_path(rest.trim_start_matches('/')))
        .unwrap_or_default();
    IndexRoute::Listing { section }
}

/// Parse `location.hash` into the route it names.
///
/// The grammar, where `<…>` is percent-encoded ([`encode_component`]) and every
/// route name must be followed by `/`, `?` or the end of the hash:
///
/// ```text
/// (empty) | #  | #!/readme         the README at HEAD
/// #!/about                         the about page
/// #!/summary                       the summary
/// #!/log[/<path>][?…]              the log; query: h=<rev>, offset=<n>, showmsg=1
/// #!/commit[/][?…]                 HEAD's commit
/// #!/commit/<sha>[?…]              one commit
///                                  query: dt=<0|1|2>, context=<n>,
///                                         ignorews=1, ss=1
/// #!/refs[/]                       all refs
/// #!/refs/heads[/]                 the branch list
/// #!/refs/tags[/]                  the tag list
/// #!/refs/tags/<tag>               one tag
/// #!/tree[/<path>][?…]             the tree, or a blob; query: h=<rev>, render=1
/// #!/blame/<path>[?h=<rev>]        per-line blame for one file
/// #!/snapshot[/…][?h=<ref>]        a .tar.gz of a revision's tree (path ignored)
/// ```
///
/// Any of these may carry a trailing `#n<A>[-n<B>]` line anchor, which
/// [`split_line_anchor`] takes off before the route is read: it selects lines
/// within the blob view and names the same route with or without it.
///
/// `h=<rev>` is a branch, a tag, `HEAD`, or a commit hash whole or abbreviated;
/// see [`effective_head`] and [`resolve_revision`], which are where the
/// distinctions are drawn. To the grammar it is one opaque string whichever it
/// is.
///
/// Anything else falls back to the readme, so a hand-edited or stale URL lands
/// on a real page rather than an error.
pub(crate) fn parse_hash(hash: &str) -> Route {
    // A line anchor selects rows within a view, never a different view, so it
    // comes off before anything below reads the string.
    let (hash, _) = split_line_anchor(hash);
    // most likely scenario
    if hash == "#!/readme" || hash.is_empty() || hash == "#" {
        return Route::Readme;
    }
    if hash == "#!/about" {
        return Route::About;
    }
    if hash == "#!/summary" {
        return Route::Summary;
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/log", &['/', '?']) {
        // rest is one of: "", "?query", "/path", or "/path?query".
        let (path_part, query_string) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let path = decode_path(path_part.trim_start_matches('/'));
        let (offset, head, showmsg) = parse_log_query(query_string);
        return Route::Log {
            offset,
            head,
            path,
            showmsg,
        };
    }

    // The id runs to the query, if there is one; an empty id (`#!/commit`,
    // `#!/commit/`, or either carrying only diff options) means HEAD's commit.
    if let Some(rest) = strip_route_prefix(hash, "#!/commit", &['/', '?']) {
        let (sha_part, query_string) = match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let sha = sha_part.trim_start_matches('/');
        let view = DiffView::parse(query_string);
        return if sha.is_empty() {
            Route::CommitHead(view)
        } else {
            Route::Commit(sha.to_string(), view)
        };
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/tree", &['/', '?']) {
        let (path, head, render) = parse_tree_rest(rest);
        return Route::Tree { path, head, render };
    }

    if let Some(rest) = strip_route_prefix(hash, "#!/blame", &['/', '?']) {
        // Blame is always of one file, so `render=1` has nothing to mean here.
        let (path, head, _) = parse_tree_rest(rest);
        return Route::Blame { path, head };
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

    // fallback to the readme on invalid routes
    Route::Readme
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

fn parse_log_query(query_string: &str) -> (usize, Option<String>, bool) {
    let mut offset = 0usize;
    let mut head = None;
    let mut showmsg = false;
    for part in query_string.split('&') {
        if let Some(v) = part.strip_prefix("offset=") {
            offset = v.parse().unwrap_or(0);
        } else if let Some(v) = part.strip_prefix("h=")
            && !v.is_empty()
        {
            head = Some(decode_component(v));
        } else if part == "showmsg=1" {
            showmsg = true;
        }
    }
    (offset, head, showmsg)
}

/// The nav tab a route lives under, used for the `active` highlight.
pub(crate) fn active_tab(route: &Route) -> &'static str {
    match route {
        Route::About => "#!/about",
        Route::Readme => "#!/readme",
        Route::Summary => "#!/summary",
        Route::Log { .. } => "#!/log",
        Route::CommitHead(_) | Route::Commit(..) => "#!/commit",
        Route::Refs(_) => "#!/refs",
        // A snapshot is an action on the tree being browsed, and blame is a
        // way of reading one of its files, so the tree tab stays lit for both.
        Route::Tree { .. } | Route::Snapshot { .. } | Route::Blame { .. } => "#!/tree",
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
/// here. `render` asks for a blob's rendered form.
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

/// The URL for a blame view. `path` and `head` are the decoded values, encoded
/// here as [`tree_url`] encodes them.
pub(crate) fn blame_url(path: &str, head: Option<&str>) -> String {
    let base = format!("#!/blame/{}", encode_path(path));
    match head.map(encode_component) {
        None => base,
        Some(head) => format!("{base}?h={head}"),
    }
}

/// The URL for a log view. `path` and `head` are the decoded values (a real
/// path, a real ref name); both are encoded here, so callers pass what they
/// have rather than remembering to escape it.
pub(crate) fn log_url(path: &str, offset: usize, head: Option<&str>, showmsg: bool) -> String {
    let base = if path.is_empty() {
        "#!/log".to_string()
    } else {
        format!("#!/log/{}", encode_path(path))
    };
    let mut params: Vec<String> = Vec::new();
    if let Some(head) = head {
        params.push(format!("h={}", encode_component(head)));
    }
    if offset != 0 {
        params.push(format!("offset={offset}"));
    }
    if showmsg {
        params.push("showmsg=1".to_string());
    }
    if params.is_empty() {
        base
    } else {
        format!("{base}?{}", params.join("&"))
    }
}

/// The URL for a commit, viewed with `view`. An empty `sha` names HEAD's
/// commit, which is the one URL the diff controls have to keep working on:
/// they rebuild the current URL with one setting changed, and the reader may
/// well have arrived at `#!/commit` without a hash in it.
pub(crate) fn commit_url(sha: &str, view: DiffView) -> String {
    let base = if sha.is_empty() {
        "#!/commit".to_string()
    } else {
        format!("#!/commit/{}", encode_component(sha))
    };
    format!("{base}{}", view.query())
}

#[cfg(test)]
mod tests;
