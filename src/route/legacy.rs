//! cgit's and Forgejo's URLs, which name the route in the path rather than the
//! fragment. They are translated into a hash before the router reads them.

use super::{DiffView, commit_url, decode_component, strip_route_prefix};

pub(crate) fn split_repo_url(bare: &str) -> Option<(&str, &str)> {
    match bare.find(".git/") {
        Some(i) => Some((&bare[..i + ".git".len()], &bare[i + ".git/".len()..])),
        None => bare.ends_with(".git").then_some((bare, "")),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum PathRoute {
    /// A route this app has, as the hash naming it ([`path_route_hash`]).
    Known { repo: String, hash: String },
    /// A route it doesn't, as the path that was asked for.
    Unknown { repo: String, path: String },
}

pub(crate) fn path_route(path: &str, search: &str) -> Option<PathRoute> {
    let (repo, rest) = split_repo_url(path)?;
    if rest.is_empty() || rest == "index.html" {
        return None;
    }
    let repo = format!("{repo}/");
    let path = format!("{rest}{search}");
    Some(match path_route_hash(&path) {
        Some(hash) => PathRoute::Known { repo, hash },
        None => PathRoute::Unknown { repo, path },
    })
}

/// Translate the path and query trailing the repository
fn path_route_hash(rest: &str) -> Option<String> {
    let (path, query) = match rest.find('?') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let rest = strip_route_prefix(path.trim_matches('/'), "commit", &['/'])?;
    let rest = rest.trim_start_matches('/');
    let id = query
        .split('&')
        .find_map(|part| part.strip_prefix("id="))
        .filter(|v| !v.is_empty());
    let rev = match id {
        // Alongside cgit's `?id=`, the path scopes the diff to one file. This
        // viewer cannot do that, so the URL is refused rather than widened.
        Some(_) if !rest.is_empty() => return None,
        Some(id) => id,
        None => rest,
    };
    Some(commit_url(&decode_component(rev), DiffView::parse(query)))
}

#[cfg(test)]
mod tests {
    use super::super::{Route, parse_hash};
    use super::*;

    #[test]
    fn test_split_repo_url() {
        // The repository itself, with or without the trailing slash.
        assert_eq!(
            split_repo_url("https://example.org/public/foo.git"),
            Some(("https://example.org/public/foo.git", ""))
        );
        assert_eq!(
            split_repo_url("https://example.org/public/foo.git/"),
            Some(("https://example.org/public/foo.git", ""))
        );
        // A cgit-style route trailing it, which is what is left to parse.
        assert_eq!(
            split_repo_url("https://example.org/public/foo.git/commit/abc"),
            Some(("https://example.org/public/foo.git", "commit/abc"))
        );
        // Paths alone, which is the form `resolve_path_route` splits.
        assert_eq!(
            split_repo_url("/public/foo.git/commit/abc"),
            Some(("/public/foo.git", "commit/abc"))
        );
        // `.git` has to end a component, not merely start one.
        assert_eq!(split_repo_url("https://example.org/public/foo"), None);
        assert_eq!(split_repo_url("https://example.org/.github/foo"), None);
        assert_eq!(split_repo_url(""), None);
    }

    #[test]
    fn test_path_route() {
        let repo = "/repos/basic.git/".to_string();
        // A route this app has, as the hash it is rewritten to.
        assert_eq!(
            path_route("/repos/basic.git/commit/abc", "?dt=2"),
            Some(PathRoute::Known {
                repo: repo.clone(),
                hash: "#!/commit/abc?dt=2".to_string(),
            })
        );
        // One it doesn't: reported at the address that was asked for, query
        // included, rather than resolved to some other page.
        assert_eq!(
            path_route("/repos/basic.git/tree/src/main.rs", ""),
            Some(PathRoute::Unknown {
                repo: repo.clone(),
                path: "tree/src/main.rs".to_string(),
            })
        );
        assert_eq!(
            path_route("/repos/basic.git/log/", "?h=main"),
            Some(PathRoute::Unknown {
                repo,
                path: "log/?h=main".to_string(),
            })
        );
        // The repository itself, and the app shell's own URL under it.
        assert_eq!(path_route("/repos/basic.git/", ""), None);
        assert_eq!(path_route("/repos/basic.git", ""), None);
        assert_eq!(path_route("/repos/basic.git/index.html", ""), None);
        // No repository in the path at all.
        assert_eq!(path_route("/", ""), None);
        assert_eq!(path_route("/repos/", ""), None);
    }

    /// The commit route in the path, the way cgit and Forgejo write it, becomes
    /// the hash naming the same commit.
    #[test]
    fn test_path_route_hash_commit() {
        // Forgejo puts the revision in the path, with or without a trailing
        // slash — and with or without a leading one, should a caller keep it.
        assert_eq!(
            path_route_hash("commit/abc123").as_deref(),
            Some("#!/commit/abc123")
        );
        assert_eq!(
            path_route_hash("commit/abc123/").as_deref(),
            Some("#!/commit/abc123")
        );
        assert_eq!(
            path_route_hash("/commit/abc123").as_deref(),
            Some("#!/commit/abc123")
        );
        // cgit puts it in `?id=`, with nothing in the path position.
        assert_eq!(
            path_route_hash("commit/?id=abc123").as_deref(),
            Some("#!/commit/abc123")
        );
        // An empty `id=` is no revision at all, so the path still has its say.
        assert_eq!(
            path_route_hash("commit/abc123?id=").as_deref(),
            Some("#!/commit/abc123")
        );
        // Nothing naming a revision either way: HEAD's commit.
        assert_eq!(path_route_hash("commit").as_deref(), Some("#!/commit"));
        assert_eq!(path_route_hash("commit/").as_deref(), Some("#!/commit"));
        assert_eq!(path_route_hash("commit/?id=").as_deref(), Some("#!/commit"));
        // A ref name is a revision like any other, encoded on the way out.
        assert_eq!(
            path_route_hash("commit/feature/a b").as_deref(),
            Some("#!/commit/feature%2Fa%20b")
        );
        assert_eq!(
            path_route_hash("commit/feature%2Fa%20b").as_deref(),
            Some("#!/commit/feature%2Fa%20b")
        );
    }

    /// The diff options ride along, so a link into a side-by-side or stat-only
    /// view lands on that view here.
    #[test]
    fn test_path_route_hash_carries_the_diff_view() {
        assert_eq!(
            path_route_hash("commit/abc?dt=2&context=10").as_deref(),
            Some("#!/commit/abc?dt=2&context=10")
        );
        // cgit spells side-by-side as a third diff type; the hash spells it as
        // the flag this app uses, naming the same view.
        assert_eq!(
            path_route_hash("commit/?id=abc&dt=1&ignorews=1").as_deref(),
            Some("#!/commit/abc?ignorews=1&ss=1")
        );
        // Defaults stay out of the URL, and `id=` is consumed, not copied.
        assert_eq!(
            path_route_hash("commit/?id=abc&context=3").as_deref(),
            Some("#!/commit/abc")
        );
    }

    /// cgit's path-scoped commit, which shows one file's diff. There is no
    /// such view here, so the URL is refused rather than answered with more.
    #[test]
    fn test_path_route_hash_refuses_a_file_scoped_commit() {
        assert_eq!(path_route_hash("commit/src/lib.rs?id=abc123"), None);
        assert_eq!(path_route_hash("commit/src?id=abc123&dt=2"), None);
        // Only the pairing is refused: either half alone still names a commit.
        assert!(path_route_hash("commit/?id=abc123").is_some());
        assert!(path_route_hash("commit/src/lib.rs?id=").is_some());
    }

    /// Anything else is left to be read as the repository root.
    #[test]
    fn test_path_route_hash_ignores_everything_else() {
        assert_eq!(path_route_hash(""), None);
        assert_eq!(path_route_hash("/"), None);
        // The route name has to end where a route name may end.
        assert_eq!(path_route_hash("commitment/abc"), None);
        // Routes that only exist in the fragment grammar are not path routes.
        assert_eq!(path_route_hash("log"), None);
        assert_eq!(path_route_hash("tree/src/lib.rs"), None);
        assert_eq!(path_route_hash("about"), None);
    }

    /// Whatever a path URL becomes has to parse back out as the commit it
    /// named: the hash is the only form the rest of the app ever sees.
    #[test]
    fn test_path_route_hash_round_trips_through_parse_hash() {
        for (path, want) in [
            ("commit/abc123", "abc123"),
            ("commit/?id=abc123", "abc123"),
            ("commit/abc123?dt=2", "abc123"),
        ] {
            let hash = path_route_hash(path).expect("a commit route");
            match parse_hash(&hash) {
                Route::Commit(sha, _) => assert_eq!(sha, want, "for {path}"),
                _ => panic!("expected Commit for {path}"),
            }
        }
        let hash = path_route_hash("commit/").expect("a commit route");
        assert!(matches!(parse_hash(&hash), Route::CommitHead(_)));
    }
}
