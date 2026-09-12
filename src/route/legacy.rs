//! cgit's and Forgejo's URLs, which name the route in the path rather than the
//! fragment. They are translated into a hash before the router reads them.

use super::{
    DiffView, PAGE_SIZE, commit_url, decode_component, decode_path, log_url, strip_route_prefix,
    tree_url,
};
use crate::render::blob::has_rendered_form;

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
    let path = path.trim_matches('/');
    for (name, route) in [
        ("commit", commit_route as fn(&str, &str) -> Option<String>),
        ("commits", commits_route),
        ("log", log_route),
        ("src", src_route),
        ("tree", tree_route),
    ] {
        if let Some(rest) = strip_route_prefix(path, name, &['/']) {
            return route(rest.trim_start_matches('/'), query);
        }
    }
    None
}

fn commit_route(rest: &str, query: &str) -> Option<String> {
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

/// cgit's log: `log/<path>`, with the rest in the query — `h=` the ref the page
/// is on, `id=` where the walk starts, `ofs=` the offset, `showmsg=1`.
fn log_route(path: &str, query: &str) -> Option<String> {
    let (mut head, mut id) = (None, None);
    let mut offset: usize = 0;
    let mut showmsg = false;
    for part in query.split('&') {
        if let Some(v) = part.strip_prefix("h=")
            && !v.is_empty()
        {
            head = Some(v);
        } else if let Some(v) = part.strip_prefix("id=")
            && !v.is_empty()
        {
            id = Some(v);
        } else if let Some(v) = part.strip_prefix("ofs=") {
            offset = v.parse().unwrap_or(0);
        } else if part == "showmsg=1" {
            showmsg = true;
        } else if let Some(v) = part.strip_prefix("q=")
            && !v.is_empty()
        {
            // cgit's log search (`qt=` picks grep, author, committer or range),
            // a view this app doesn't have.
            return None;
        } else if part == "follow=1" && !path.is_empty() {
            return None;
        }
    }
    // `id=` is where cgit starts the walk, and overrides the ref the page is on.
    let head = id.or(head).map(decode_component);
    Some(log_url(
        &decode_path(path),
        offset,
        head.as_deref(),
        showmsg,
    ))
}

/// Forgejo's log: `commits/<branch|tag|commit>/<rev>[/<path>]`, paged by
/// `?page=`. Only the forms whose ref reads without its ref list are taken.
fn commits_route(rest: &str, query: &str) -> Option<String> {
    let (rev, path) = forgejo_ref(rest)?;
    // Forgejo reads this one path as commit search rather than a file, and
    // there is no such view here.
    if path == "search" {
        return None;
    }
    let mut page: usize = 1;
    for part in query.split('&') {
        if let Some(v) = part.strip_prefix("page=") {
            page = v.parse().unwrap_or(1).max(1);
        } else if let Some(v) = part.strip_prefix("limit=")
            && matches!(v.parse::<usize>(), Ok(n) if n > 0 && n != PAGE_SIZE)
        {
            // A page size this app has no way to render: its log pages by fifty.
            return None;
        }
    }
    let offset = (page - 1).saturating_mul(PAGE_SIZE);
    Some(log_url(
        &decode_path(path),
        offset,
        Some(&decode_component(rev)),
        false,
    ))
}

/// The revision and the path in Forgejo's `<branch|tag|commit>/<rev>[/<path>]`,
/// or `None` where telling the two apart would need its own ref list.
fn forgejo_ref(rest: &str) -> Option<(&str, &str)> {
    let (kind, rest) = rest.split_once('/')?;
    match kind {
        // Forgejo takes the first segment as the object id and whatever follows
        // as the path, so there is nothing here to be unsure about.
        "commit" => {
            let (rev, path) = rest.split_once('/').unwrap_or((rest, ""));
            // The lengths an id may have, as Forgejo bounds them: four to a
            // full sha256. Whether it names an object is resolved later.
            (4..=64).contains(&rev.len()).then_some((rev, path))
        }
        // A name may contain '/', so past one segment the name and the path
        // cannot be told apart without the ref list. One segment is the name.
        "branch" | "tag" if !rest.is_empty() && !rest.contains('/') => Some((rest, "")),
        _ => None,
    }
}

/// cgit's tree: `tree/<path>`, with `h=` the ref the page is on and `id=` the
/// revision to read it at, which wins as it does in the log.
fn tree_route(path: &str, query: &str) -> Option<String> {
    let (mut head, mut id) = (None, None);
    for part in query.split('&') {
        if let Some(v) = part.strip_prefix("h=")
            && !v.is_empty()
        {
            head = Some(v);
        } else if let Some(v) = part.strip_prefix("id=")
            && !v.is_empty()
        {
            id = Some(v);
        }
    }
    let head = id.or(head).map(decode_component);
    // cgit shows a file as its source, which is this app's own default.
    Some(tree_url(&decode_path(path), head.as_deref(), false))
}

/// Forgejo's tree: `src/<branch|tag|commit>/<rev>[/<path>]`. A file it has a
/// rendered form of is shown rendered unless `?display=source` asks otherwise.
fn src_route(rest: &str, query: &str) -> Option<String> {
    let (rev, path) = forgejo_ref(rest)?;
    let path = decode_path(path);
    let source = query.split('&').any(|part| part == "display=source");
    let render = !source && has_rendered_form(&path);
    Some(tree_url(&path, Some(&decode_component(rev)), render))
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
            path_route("/repos/basic.git/plain/src/main.rs", ""),
            Some(PathRoute::Unknown {
                repo: repo.clone(),
                path: "plain/src/main.rs".to_string(),
            })
        );
        assert_eq!(
            path_route("/repos/basic.git/log/", "?qt=grep&q=fix"),
            Some(PathRoute::Unknown {
                repo,
                path: "log/?qt=grep&q=fix".to_string(),
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

    /// cgit's log: the path in the path, the ref and the paging in the query.
    #[test]
    fn test_path_route_hash_log() {
        assert_eq!(path_route_hash("log").as_deref(), Some("#!/log"));
        assert_eq!(path_route_hash("log/").as_deref(), Some("#!/log"));
        assert_eq!(
            path_route_hash("log/src/lib.rs").as_deref(),
            Some("#!/log/src/lib.rs")
        );
        // `h=` is the ref the page is on; cgit leaves it out on the default
        // branch, exactly as this app does.
        assert_eq!(
            path_route_hash("log/?h=next").as_deref(),
            Some("#!/log?h=next")
        );
        // `ofs=` is an offset in commits, which is what this app pages by too.
        assert_eq!(
            path_route_hash("log/src?h=next&ofs=50&showmsg=1").as_deref(),
            Some("#!/log/src?h=next&offset=50&showmsg=1")
        );
        // `id=` is where the walk starts, and beats the ref the page is on.
        assert_eq!(
            path_route_hash("log/?h=next&id=abc123").as_deref(),
            Some("#!/log?h=abc123")
        );
        // An offset that is not a number is no offset, not an error page.
        assert_eq!(path_route_hash("log/?ofs=x").as_deref(), Some("#!/log"));
    }

    /// cgit's log searches, and `--follow`: views this app doesn't have.
    #[test]
    fn test_path_route_hash_refuses_a_log_search() {
        assert_eq!(path_route_hash("log/?qt=grep&q=fix"), None);
        assert_eq!(path_route_hash("log/?qt=range&q=v1.0..v2.0"), None);
        assert_eq!(path_route_hash("log/src/lib.rs?follow=1"), None);
        // cgit ignores `follow=1` without a path, so it asks for nothing extra.
        assert_eq!(path_route_hash("log/?follow=1").as_deref(), Some("#!/log"));
        // An empty pattern is no search, which is cgit's reading of it too.
        assert_eq!(
            path_route_hash("log/?qt=grep&q=").as_deref(),
            Some("#!/log")
        );
    }

    /// Forgejo's log: the ref in the path, under the segment naming its kind.
    #[test]
    fn test_path_route_hash_commits() {
        assert_eq!(
            path_route_hash("commits/branch/main").as_deref(),
            Some("#!/log?h=main")
        );
        assert_eq!(
            path_route_hash("commits/tag/v1.0").as_deref(),
            Some("#!/log?h=v1.0")
        );
        assert_eq!(
            path_route_hash("commits/commit/abc123").as_deref(),
            Some("#!/log?h=abc123")
        );
        // Under `commit/`, whatever trails the id is the file whose history is
        // shown: an id is one segment, so the two cannot be confused.
        assert_eq!(
            path_route_hash("commits/commit/abc123/src/lib.rs").as_deref(),
            Some("#!/log/src/lib.rs?h=abc123")
        );
        // `page=` is 1-based, and a page is fifty commits in both.
        assert_eq!(
            path_route_hash("commits/branch/main?page=3").as_deref(),
            Some("#!/log?h=main&offset=100")
        );
        // Forgejo reads a page below one, or no number at all, as the first.
        assert_eq!(
            path_route_hash("commits/branch/main?page=0").as_deref(),
            Some("#!/log?h=main")
        );
        // A page size it would render the same way is no obstacle.
        assert_eq!(
            path_route_hash("commits/branch/main?limit=50").as_deref(),
            Some("#!/log?h=main")
        );
    }

    /// A ref this app cannot read the way Forgejo does. Guessing would render
    /// the wrong revision's log, or the right one scoped to the wrong file.
    #[test]
    fn test_path_route_hash_refuses_an_ambiguous_ref() {
        // `feature/x` is either a branch of that name or branch `feature` with
        // a file called `x`, and only the ref list says which.
        assert_eq!(path_route_hash("commits/branch/feature/x"), None);
        assert_eq!(path_route_hash("commits/tag/v1.0/src/lib.rs"), None);
        // The deprecated untyped form, which needs that list to read at all.
        assert_eq!(path_route_hash("commits/main"), None);
        assert_eq!(path_route_hash("commits"), None);
        assert_eq!(path_route_hash("commits/branch/"), None);
        assert_eq!(path_route_hash("commits/branch"), None);
        // An id outside the lengths Forgejo reads as one.
        assert_eq!(path_route_hash("commits/commit/ab"), None);
        assert_eq!(path_route_hash("commits/commit/"), None);
    }

    /// The parts of Forgejo's log grammar that name something else entirely.
    #[test]
    fn test_path_route_hash_refuses_other_commits_urls() {
        // Commit search, which this app has no equivalent of.
        assert_eq!(path_route_hash("commits/commit/abc123/search?q=fix"), None);
        // A page size its log cannot render.
        assert_eq!(path_route_hash("commits/branch/main?limit=10"), None);
    }

    /// cgit's tree: the path in the path, the revision in the query.
    #[test]
    fn test_path_route_hash_tree() {
        assert_eq!(path_route_hash("tree").as_deref(), Some("#!/tree"));
        assert_eq!(path_route_hash("tree/").as_deref(), Some("#!/tree"));
        assert_eq!(
            path_route_hash("tree/src/render").as_deref(),
            Some("#!/tree/src/render")
        );
        assert_eq!(
            path_route_hash("tree/src?h=next").as_deref(),
            Some("#!/tree/src?h=next")
        );
        // `id=` is the revision the tree is read at, and beats `h=`.
        assert_eq!(
            path_route_hash("tree/?h=next&id=abc123").as_deref(),
            Some("#!/tree?h=abc123")
        );
        // cgit shows a file as source, which is this app's default: no flag.
        assert_eq!(
            path_route_hash("tree/README.md?h=next").as_deref(),
            Some("#!/tree/README.md?h=next")
        );
    }

    /// Forgejo's tree, which is its file view too: the same URL either way, and
    /// a file it can render is rendered unless the URL says source.
    #[test]
    fn test_path_route_hash_src() {
        assert_eq!(
            path_route_hash("src/branch/main").as_deref(),
            Some("#!/tree?h=main")
        );
        assert_eq!(
            path_route_hash("src/tag/v1.0").as_deref(),
            Some("#!/tree?h=v1.0")
        );
        assert_eq!(
            path_route_hash("src/commit/abc123/src/render").as_deref(),
            Some("#!/tree/src/render?h=abc123")
        );
        // Forgejo renders markdown and SVG by default, where this app asks.
        assert_eq!(
            path_route_hash("src/commit/abc123/README.md").as_deref(),
            Some("#!/tree/README.md?h=abc123&render=1")
        );
        assert_eq!(
            path_route_hash("src/commit/abc123/logo.svg").as_deref(),
            Some("#!/tree/logo.svg?h=abc123&render=1")
        );
        assert_eq!(
            path_route_hash("src/commit/abc123/README.md?display=source").as_deref(),
            Some("#!/tree/README.md?h=abc123")
        );
        // A file with no rendered form is the same page either way.
        assert_eq!(
            path_route_hash("src/commit/abc123/src/main.rs").as_deref(),
            Some("#!/tree/src/main.rs?h=abc123")
        );
    }

    /// The tree's share of the refs that cannot be read without Forgejo's list.
    #[test]
    fn test_path_route_hash_refuses_an_ambiguous_src_ref() {
        assert_eq!(path_route_hash("src/branch/main/src/lib.rs"), None);
        assert_eq!(path_route_hash("src/tag/v1.0/docs"), None);
        assert_eq!(path_route_hash("src/main/README.md"), None);
        assert_eq!(path_route_hash("src/commit/ab/README.md"), None);
    }

    /// Anything else is left to be read as the repository root.
    #[test]
    fn test_path_route_hash_ignores_everything_else() {
        assert_eq!(path_route_hash(""), None);
        assert_eq!(path_route_hash("/"), None);
        // The route name has to end where a route name may end.
        assert_eq!(path_route_hash("commitment/abc"), None);
        // cgit pages this app has no view for, and a name that merely starts
        // with one of the routes it does have.
        assert_eq!(path_route_hash("plain/src/lib.rs"), None);
        assert_eq!(path_route_hash("about"), None);
        assert_eq!(path_route_hash("logout"), None);
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
