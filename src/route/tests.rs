//! The routing grammar's tests: every hash the app writes has to come back
//! out of [`parse_hash`](super::parse_hash) as the route it named.

use super::*;

#[test]
fn test_parse_hash_readme_is_the_default() {
    assert!(matches!(parse_hash(""), Route::Readme));
    assert!(matches!(parse_hash("#"), Route::Readme));
    assert!(matches!(parse_hash("#!/readme"), Route::Readme));
}

#[test]
fn test_parse_hash_summary() {
    assert!(matches!(parse_hash("#!/summary"), Route::Summary));
}

/// The section a listing hash names, or `None` for the about page.
fn index_section(hash: &str) -> Option<String> {
    match parse_index_hash(hash) {
        IndexRoute::Listing { section } => Some(section),
        IndexRoute::About => None,
    }
}

#[test]
fn test_parse_index_hash() {
    assert_eq!(index_section(""), Some(String::new()));
    assert_eq!(index_section("#"), Some(String::new()));
    assert_eq!(index_section("#!/index"), Some(String::new()));
    assert_eq!(index_section("#!/index/"), Some(String::new()));
    assert_eq!(index_section("#!/index/public"), Some("public".to_string()));
    // A section is a prefix, so it may be several directories deep.
    assert_eq!(index_section("#!/index/a/b"), Some("a/b".to_string()));
    assert_eq!(index_section("#!/about"), None);
    // An unknown route — including a bare `#<section>` anchor, which is not
    // part of the grammar — is the listing, unscrolled, not an error.
    assert_eq!(index_section("#!/log"), Some(String::new()));
    assert_eq!(index_section("#public"), Some(String::new()));
}

#[test]
fn test_index_url_round_trips() {
    assert_eq!(index_url(""), "#!/index");
    assert_eq!(index_url("public"), "#!/index/public");
    for section in ["public", "a/b", "with space", "q?x", "100%"] {
        assert_eq!(index_section(&index_url(section)).as_deref(), Some(section));
    }
}

/// A line anchor names lines within a view, never a different view: every
/// route parses the same with one attached.
#[test]
fn test_parse_hash_ignores_line_anchor() {
    assert!(matches!(parse_hash("#!/about#n5"), Route::About));
    assert!(matches!(parse_hash("#!/summary#n5-n10"), Route::Summary));
    let Route::Tree { path, head, .. } = parse_hash("#!/tree/src/lib.rs?h=v1.0#n5-n10") else {
        panic!("not a tree route");
    };
    assert_eq!(path, "src/lib.rs");
    assert_eq!(head.as_deref(), Some("v1.0"));
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
            showmsg: false,
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
            showmsg: false,
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
        ..
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
    assert!(matches!(parse_hash("#!/commit"), Route::CommitHead(_)));
    assert!(matches!(parse_hash("#!/commit/abc123"), Route::Commit(..)));
}

/// An empty id is not a commit to look up, so the bare route's meaning
/// (HEAD's commit) survives a trailing slash.
#[test]
fn test_parse_hash_commit_trailing_slash() {
    assert!(matches!(parse_hash("#!/commit/"), Route::CommitHead(_)));
}

/// The id runs to the query, not to the end of the hash.
#[test]
fn test_parse_hash_commit_query_is_not_part_of_the_id() {
    match parse_hash("#!/commit/abc123?context=8&ignorews=1&ss=1") {
        Route::Commit(sha, view) => {
            assert_eq!(sha, "abc123");
            assert_eq!(view.context, Some(8));
            assert!(view.ignore_whitespace);
            assert!(view.side_by_side);
            assert_eq!(view.mode, DiffMode::Unified);
        }
        _ => panic!("expected a commit route"),
    }
    // …including on the id-less form, which still follows HEAD.
    match parse_hash("#!/commit?dt=2") {
        Route::CommitHead(view) => assert_eq!(view.mode, DiffMode::StatOnly),
        _ => panic!("expected HEAD's commit"),
    }
}

/// cgit spells side-by-side as a third `dt`, so its links have to land on
/// the same view here.
#[test]
fn test_parse_hash_commit_accepts_cgits_ssdiff_difftype() {
    match parse_hash("#!/commit/abc?dt=1") {
        Route::Commit(_, view) => {
            assert!(view.side_by_side);
            assert_eq!(view.mode, DiffMode::Unified);
        }
        _ => panic!("expected a commit route"),
    }
}

/// A hand-edited width should not produce an error page, and should not be
/// able to ask for a diff wide enough to hang the tab.
#[test]
fn test_parse_hash_commit_rejects_an_impossible_context() {
    for hash in [
        "#!/commit/abc?context=0",
        "#!/commit/abc?context=9999",
        "#!/commit/abc?context=x",
    ] {
        match parse_hash(hash) {
            Route::Commit(_, view) => {
                assert_eq!(view.context, None, "{hash}");
                assert_eq!(view.context_lines(), 3, "{hash}");
            }
            _ => panic!("expected a commit route"),
        }
    }
}

#[test]
fn test_commit_url() {
    // Every setting at its default leaves the URL alone.
    assert_eq!(commit_url("abc", DiffView::default()), "#!/commit/abc");
    assert_eq!(commit_url("", DiffView::default()), "#!/commit");
    // …and an explicit `context=3` is a default too, so it is not written.
    assert_eq!(
        commit_url(
            "abc",
            DiffView {
                context: Some(3),
                ..DiffView::default()
            }
        ),
        "#!/commit/abc"
    );
    // Order is cgit's: dt, context, ignorews, then ss.
    assert_eq!(
        commit_url(
            "abc",
            DiffView {
                context: Some(10),
                ignore_whitespace: true,
                mode: DiffMode::Unified,
                side_by_side: true,
            }
        ),
        "#!/commit/abc?context=10&ignorews=1&ss=1"
    );
}

/// Stat-only hides the diff, so there is no layout for `ss` to name and it
/// drops out rather than lingering as a setting with no visible effect.
#[test]
fn test_commit_url_drops_side_by_side_when_the_diff_is_hidden() {
    assert_eq!(
        commit_url(
            "abc",
            DiffView {
                mode: DiffMode::StatOnly,
                side_by_side: true,
                ..DiffView::default()
            }
        ),
        "#!/commit/abc?dt=2"
    );
}

#[test]
fn test_commit_url_round_trips_through_the_router() {
    let view = DiffView {
        context: Some(15),
        ignore_whitespace: true,
        mode: DiffMode::StatOnly,
        side_by_side: false,
    };
    match parse_hash(&commit_url("abc", view)) {
        Route::Commit(sha, got) => {
            assert_eq!(sha, "abc");
            assert_eq!(got, view);
        }
        _ => panic!("expected a commit route"),
    }
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

#[test]
fn test_parse_hash_blame() {
    assert!(matches!(
        parse_hash("#!/blame/src/main.rs"),
        Route::Blame { path, head: None } if path == "src/main.rs"
    ));
    assert!(matches!(
        parse_hash("#!/blame/src/main.rs?h=v1.0"),
        Route::Blame { path, head: Some(head) } if path == "src/main.rs" && head == "v1.0"
    ));
    // A line anchor selects lines within the blame, not another route.
    assert!(matches!(
        parse_hash("#!/blame/src/main.rs#n5"),
        Route::Blame { path, .. } if path == "src/main.rs"
    ));
}

#[test]
fn test_blame_url() {
    assert_eq!(blame_url("src/main.rs", None), "#!/blame/src/main.rs");
    assert_eq!(
        blame_url("src/main.rs", Some("main")),
        "#!/blame/src/main.rs?h=main"
    );
    assert_eq!(
        blame_url("docs/a b.md", Some("release/2.0")),
        "#!/blame/docs/a%20b.md?h=release%2F2.0",
        "a slash in a ref name is encoded, not left to split the route"
    );
}

/// The round trip that matters: a path and a ref carrying route syntax come
/// back out of the router as themselves.
#[test]
fn test_blame_url_round_trips_through_the_router() {
    let url = blame_url("docs/a?b.md", Some("x&h=y"));
    match parse_hash(&url) {
        Route::Blame { path, head } => {
            assert_eq!(path, "docs/a?b.md");
            assert_eq!(head.as_deref(), Some("x&h=y"));
        }
        _ => panic!("expected a blame route from {url}"),
    }
}

/// Blame is a way of reading a file in the tree, so the tree tab stays lit.
#[test]
fn test_blame_lives_under_the_tree_tab() {
    assert_eq!(active_tab(&parse_hash("#!/blame/src/main.rs")), "#!/tree");
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
    assert_eq!(log_url("", 0, Some(sha), false), format!("#!/log?h={sha}"));
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
        assert!(matches!(parse_hash(hash), Route::Readme), "{hash}");
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
    assert!(matches!(parse_hash("#!/commit/abc"), Route::Commit(..)));
    assert!(matches!(parse_hash("#!/refs/tags"), Route::Refs(_)));
}

#[test]
fn test_log_url() {
    assert_eq!(log_url("", 0, None, false), "#!/log");
    assert_eq!(log_url("", 50, None, false), "#!/log?offset=50");
    assert_eq!(log_url("", 0, Some("main"), false), "#!/log?h=main");
    assert_eq!(
        log_url("", 100, Some("stable"), false),
        "#!/log?h=stable&offset=100"
    );
    assert_eq!(
        log_url("src/route.rs", 0, None, false),
        "#!/log/src/route.rs"
    );
    assert_eq!(
        log_url("src", 50, Some("main"), false),
        "#!/log/src?h=main&offset=50"
    );
}

#[test]
fn test_log_url_showmsg() {
    assert_eq!(log_url("", 0, None, true), "#!/log?showmsg=1");
    assert_eq!(log_url("", 50, None, true), "#!/log?offset=50&showmsg=1");
    assert_eq!(
        log_url("src", 50, Some("main"), true),
        "#!/log/src?h=main&offset=50&showmsg=1"
    );
}

#[test]
fn test_parse_hash_log_showmsg() {
    // Only the spelling `log_url` writes turns the bodies on; anything else
    // in that position leaves the log collapsed.
    for (url, want) in [
        ("#!/log?showmsg=1", true),
        ("#!/log/src?h=main&offset=50&showmsg=1", true),
        ("#!/log", false),
        ("#!/log?showmsg=0", false),
        ("#!/log?showmsg", false),
        ("#!/log?showmsg=2", false),
    ] {
        match parse_hash(url) {
            Route::Log { showmsg, .. } => assert_eq!(showmsg, want, "via {url}"),
            _ => panic!("expected Log for {url}"),
        }
    }
}

/// A ref may contain `&`, so an expanded log's own parameters must survive
/// a branch name that spells one of them.
#[test]
fn test_showmsg_is_not_confused_by_an_encoded_ref() {
    let url = log_url("", 0, Some("x&showmsg=1"), false);
    match parse_hash(&url) {
        Route::Log { showmsg, head, .. } => {
            assert!(!showmsg);
            assert_eq!(head.as_deref(), Some("x&showmsg=1"));
        }
        _ => panic!("expected Log"),
    }
}

#[test]
fn test_encoding_round_trips_through_the_router() {
    // The point of the exercise: a ref or path containing route syntax has
    // to come back out of `parse_hash` as the name it went in as.
    for name in ["feature/x", "foo?bar", "a&b", "100%", "release #2", "café"] {
        let url = log_url("", 0, Some(name), false);
        match parse_hash(&url) {
            Route::Log { head: Some(h), .. } => assert_eq!(h, name, "via {url}"),
            _ => panic!("expected a log route with a head from {url}"),
        }
    }
    for path in ["src/a?b.rs", "docs/50%off.md", "a&b/c#d"] {
        let url = log_url(path, 0, None, false);
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
    let url = log_url("", 10, Some("x&offset=999"), false);
    match parse_hash(&url) {
        Route::Log { offset, head, .. } => {
            assert_eq!(offset, 10);
            assert_eq!(head.as_deref(), Some("x&offset=999"));
        }
        _ => panic!("expected a log route"),
    }
}
