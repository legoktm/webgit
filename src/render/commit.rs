use crate::error::GitContext;
use crate::{cache::CachingRepo, render::render_template};
use git_async::diff::{DiffEntry, TreeDiff};
use git_async::error::Error as GitError;
use git_async::object::{Object, ObjectId};
use serde::Serialize;
use similar::TextDiffConfig;
use tera::Tera;

#[derive(Serialize)]
struct ParentRef {
    hash: String,
    short: String,
}

#[derive(Serialize)]
struct DiffLine {
    kind: String,
    content: String,
}

#[derive(Serialize)]
struct FileDiff {
    path: String,
    additions: usize,
    deletions: usize,
    bar_add: usize,
    bar_del: usize,
}

#[derive(Serialize)]
struct CommitTemplate {
    hash: String,
    short_hash: String,
    author_name: String,
    author_email: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_date: String,
    parents: Vec<ParentRef>,
    tree_hash: String,
    message_html: String,
    total_additions: usize,
    total_deletions: usize,
    files: Vec<FileDiff>,
    diff_lines: Vec<DiffLine>,
}

async fn build_commit(repo: &CachingRepo, sha: &str) -> anyhow::Result<CommitTemplate> {
    let oid =
        ObjectId::from_hex(sha.as_bytes()).ok_or_else(|| anyhow::anyhow!("invalid SHA: {sha}"))?;
    let commit = repo
        .lookup_object(oid)
        .await
        .context(format!("lookup {sha}"))?
        .commit()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let (parent_commits, commit_tree_obj) = futures::join!(
        async { repo.lookup_parents(&commit).await.unwrap_or_default() },
        repo.lookup_object(commit.tree()),
    );
    let commit_tree = commit_tree_obj
        .context("lookup commit tree")?
        .tree()
        .map_err(GitError::from)
        .context("unexpected object type")?;

    let parents: Vec<ParentRef> = parent_commits
        .iter()
        .map(|p| {
            let h = format!("{}", p.id());
            ParentRef {
                short: h[..8].to_string(),
                hash: h,
            }
        })
        .collect();

    let (files, diff_lines) = if let Some(parent) = parent_commits.first() {
        let parent_tree = repo
            .lookup_object(parent.tree())
            .await
            .context("lookup parent tree")?
            .tree()
            .map_err(GitError::from)
            .context("unexpected object type")?;
        let td = repo
            .tree_diff(&parent_tree, &commit_tree)
            .await
            .context("tree diff")?;
        build_diff(repo, &td).await
    } else {
        (Vec::new(), Vec::new())
    };

    let total_additions: usize = files.iter().map(|f| f.additions).sum();
    let total_deletions: usize = files.iter().map(|f| f.deletions).sum();

    let hash = format!("{}", commit.id());
    Ok(CommitTemplate {
        short_hash: hash[..8].to_string(),
        hash,
        author_name: String::from_utf8_lossy(commit.author_name()).into_owned(),
        author_email: String::from_utf8_lossy(commit.author_email()).into_owned(),
        author_date: commit.author_date().to_string(),
        committer_name: String::from_utf8_lossy(commit.committer_name()).into_owned(),
        committer_email: String::from_utf8_lossy(commit.committer_email()).into_owned(),
        committer_date: commit.commit_date().to_string(),
        parents,
        tree_hash: format!("{}", commit.tree()),
        message_html: linkify_message(&String::from_utf8_lossy(commit.message())),
        total_additions,
        total_deletions,
        files,
        diff_lines,
    })
}

fn escape_char(c: char, out: &mut String) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#x27;"),
        _ => out.push(c),
    }
}

/// A token is treated as a commit reference if it is a run of 7-40 lowercase
/// hex digits, i.e. a full SHA-1 or one of git's abbreviated forms.
fn is_sha1(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Turn SHA-1 references in a commit message into links to the referenced
/// commit, HTML-escaping everything else. The result is trusted HTML, so it
/// must be rendered with Tera's `safe` filter.
fn linkify_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut token = String::new();

    let flush = |token: &str, out: &mut String| {
        if is_sha1(token) {
            // `token` is pure lowercase hex, so it is safe both as an attribute
            // value and as text without further escaping.
            out.push_str("<a href=\"#!/commit/");
            out.push_str(token);
            out.push_str("\">");
            out.push_str(token);
            out.push_str("</a>");
        } else {
            for c in token.chars() {
                escape_char(c, out);
            }
        }
    };

    for c in message.chars() {
        // Word boundaries are ASCII alphanumerics; anything else ends the
        // current token so e.g. a hash inside "word_abc1234" is not matched.
        if c.is_ascii_alphanumeric() {
            token.push(c);
        } else {
            flush(&token, &mut out);
            token.clear();
            escape_char(c, &mut out);
        }
    }
    flush(&token, &mut out);
    out
}

/// Heuristic matching git's: a blob is treated as binary if a NUL byte appears
/// in its leading bytes. git scans the first 8000 bytes, so do the same.
fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

async fn load_blob(repo: &CachingRepo, id: ObjectId) -> Vec<u8> {
    if id.bytes() == &[0u8; 20] {
        return Vec::new();
    }
    match repo.lookup_object(id).await {
        Ok(Object::Blob(b)) => b.data_owned(),
        Ok(_) => format!("{id}").into_bytes(),
        Err(_) => Vec::new(),
    }
}

/// Compute the rendered diff lines and (additions, deletions) for a single
/// changed file. The returned lines always begin with the `diff --git` header;
/// a file detected as binary yields just that header plus a "Binary files
/// differ" line and zero counts. The `+++`/`---` unified-diff headers are
/// emitted but excluded from the counts, matching `git`'s diffstat.
fn diff_file(path: &str, old_data: Vec<u8>, new_data: Vec<u8>) -> (Vec<DiffLine>, usize, usize) {
    let mut lines = vec![DiffLine {
        kind: "hunk".to_string(),
        content: format!("diff --git a/{path} b/{path}"),
    }];

    // Git treats a blob as binary if it contains a NUL byte; in that case it
    // shows "Binary files differ" rather than a line-by-line diff, which would
    // be meaningless (and potentially huge). Mirror that here.
    if is_binary(&old_data) || is_binary(&new_data) {
        lines.push(DiffLine {
            kind: "ctx".to_string(),
            content: format!("Binary files a/{path} and b/{path} differ"),
        });
        return (lines, 0, 0);
    }

    let text_diff = TextDiffConfig::default().diff_lines(old_data, new_data);
    let udiff = text_diff
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();

    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in udiff.lines() {
        let lkind = match line.chars().next() {
            Some('+') => {
                if !line.starts_with("+++") {
                    additions += 1;
                }
                "add"
            }
            Some('-') => {
                if !line.starts_with("---") {
                    deletions += 1;
                }
                "del"
            }
            Some('@') => "hunk",
            _ => "ctx",
        };
        lines.push(DiffLine {
            kind: lkind.to_string(),
            content: line.to_string(),
        });
    }

    (lines, additions, deletions)
}

async fn build_diff(repo: &CachingRepo, td: &TreeDiff) -> (Vec<FileDiff>, Vec<DiffLine>) {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut diff_lines: Vec<DiffLine> = Vec::new();

    // Phase 1: load every changed file's blobs concurrently, so the per-object
    // IndexedDB/network round-trips overlap across files instead of being
    // serialised one file at a time. The diffing itself (phase 2) is CPU-bound
    // and stays sequential to preserve output order.
    let loaded = futures::future::join_all(td.entries().iter().map(|entry| async move {
        let path = String::from_utf8_lossy(entry.path().as_slice()).into_owned();
        let (old_data, new_data) = match entry {
            DiffEntry::LeftOnly {
                content: (old_id, _),
                ..
            } => (load_blob(repo, *old_id).await, Vec::new()),
            DiffEntry::RightOnly {
                content: (_, new_id),
                ..
            } => (Vec::new(), load_blob(repo, *new_id).await),
            DiffEntry::Both {
                content: (old_id, new_id),
                ..
            } => {
                futures::join!(load_blob(repo, *old_id), load_blob(repo, *new_id))
            }
        };
        (path, old_data, new_data)
    }))
    .await;

    for (path, old_data, new_data) in loaded {
        let (lines, additions, deletions) = diff_file(&path, old_data, new_data);
        diff_lines.extend(lines);
        files.push(FileDiff {
            path,
            additions,
            deletions,
            bar_add: 0,
            bar_del: 0,
        });
    }

    let max_changes = files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(1)
        .max(1);

    for f in &mut files {
        let total = f.additions + f.deletions;
        let bar_total = total * 40 / max_changes;
        f.bar_add = f
            .additions
            .checked_mul(bar_total)
            .and_then(|n| n.checked_div(total))
            .unwrap_or(0);
        f.bar_del = bar_total - f.bar_add;
    }

    (files, diff_lines)
}

pub(crate) async fn render_commit(
    tera: &Tera,
    repo: &CachingRepo,
    sha: String,
    output: &web_sys::Element,
) -> anyhow::Result<()> {
    let template = build_commit(repo, &sha).await?;
    render_template(tera, "commit.html", &template, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{init_tera, render_to_string};

    fn base_fixture() -> CommitTemplate {
        CommitTemplate {
            hash: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            short_hash: "0123abcd".to_string(),
            author_name: "Kunal Mehta".to_string(),
            author_email: "author@example.org".to_string(),
            author_date: "2026-01-15 12:34:56 +00:00".to_string(),
            committer_name: "Committer Person".to_string(),
            committer_email: "committer@example.org".to_string(),
            committer_date: "2026-01-15 13:00:00 +00:00".to_string(),
            parents: vec![],
            tree_hash: "fedcba98fedcba98fedcba98fedcba98fedcba98".to_string(),
            message_html: linkify_message(
                "Fix the thing\n\nLonger explanation with <html> & \"chars\".\nSee 0123abcd for context.",
            ),
            total_additions: 0,
            total_deletions: 0,
            files: vec![],
            diff_lines: vec![],
        }
    }

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b""));
        assert!(!is_binary(b"hello\nworld\n"));
        // UTF-8 multibyte content has no NUL bytes and must stay textual.
        assert!(!is_binary("café — résumé".as_bytes()));
        assert!(is_binary(b"PK\x03\x04\x00\x00"));
        assert!(is_binary(b"text then \0 nul"));
        // A NUL past the 8000-byte scan window is not flagged, matching git.
        let mut late_nul = vec![b'a'; 8000];
        late_nul.push(0);
        assert!(!is_binary(&late_nul));
    }

    #[test]
    fn test_diff_file_modification() {
        let (lines, additions, deletions) = diff_file(
            "foo.txt",
            b"alpha\nbeta\n".to_vec(),
            b"alpha\nbeta changed\n".to_vec(),
        );
        assert_eq!(additions, 1);
        assert_eq!(deletions, 1);
        // The first line is always the git-style header.
        assert_eq!(lines[0].kind, "hunk");
        assert_eq!(lines[0].content, "diff --git a/foo.txt b/foo.txt");
        // A hunk marker and the changed lines are present and classified.
        assert!(lines.iter().any(|l| l.kind == "hunk" && l.content.starts_with("@@")));
        assert!(
            lines
                .iter()
                .any(|l| l.kind == "add" && l.content.starts_with('+') && !l.content.starts_with("+++"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.kind == "del" && l.content.starts_with('-') && !l.content.starts_with("---"))
        );
    }

    #[test]
    fn test_diff_file_pure_addition_and_deletion() {
        // A brand-new file: every line counts as an addition, none as deletion.
        let (_lines, additions, deletions) =
            diff_file("new.txt", Vec::new(), b"one\ntwo\nthree\n".to_vec());
        assert_eq!((additions, deletions), (3, 0));

        // A removed file: the reverse.
        let (_lines, additions, deletions) =
            diff_file("gone.txt", b"one\ntwo\n".to_vec(), Vec::new());
        assert_eq!((additions, deletions), (0, 2));
    }

    #[test]
    fn test_diff_file_binary() {
        // A NUL byte makes the file binary: no line-by-line diff, zero counts.
        let (lines, additions, deletions) =
            diff_file("blob.bin", b"\0\x01\x02".to_vec(), b"\0\x03".to_vec());
        assert_eq!((additions, deletions), (0, 0));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "diff --git a/blob.bin b/blob.bin");
        assert_eq!(lines[1].kind, "ctx");
        assert_eq!(
            lines[1].content,
            "Binary files a/blob.bin and b/blob.bin differ"
        );
    }

    #[test]
    fn test_linkify_message() {
        // A 7-40 char hex run becomes a link; surrounding text is escaped.
        assert_eq!(
            linkify_message("see 0123abcd <ok>"),
            "see <a href=\"#!/commit/0123abcd\">0123abcd</a> &lt;ok&gt;"
        );
        // Full 40-char SHA-1.
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            linkify_message(full),
            format!("<a href=\"#!/commit/{full}\">{full}</a>")
        );
        // Too short (<7), too long (>40), uppercase, or embedded in a word: no link.
        assert_eq!(linkify_message("abc123"), "abc123");
        assert_eq!(linkify_message("0123ABCD"), "0123ABCD");
        assert_eq!(linkify_message("x0123abcd"), "x0123abcd");
        assert_eq!(linkify_message(&"a".repeat(41)), "a".repeat(41));
    }

    #[test]
    fn test_linkify_message_escapes_xss() {
        // A script-injection attempt must be fully neutralised: no raw angle
        // brackets, quotes, or ampersands survive into the trusted HTML.
        let attack = "<script>alert('xss')</script> & \"quotes\"";
        let html = linkify_message(attack);
        assert_eq!(
            html,
            "&lt;script&gt;alert(&#x27;xss&#x27;)&lt;/script&gt; &amp; &quot;quotes&quot;"
        );
        assert!(!html.contains('<'));
        assert!(!html.contains('>'));
        // Escaping still happens around a linkified hash on the same line.
        let mixed = linkify_message("<b>0123abcd</b>");
        assert_eq!(
            mixed,
            "&lt;b&gt;<a href=\"#!/commit/0123abcd\">0123abcd</a>&lt;/b&gt;"
        );
    }

    #[test]
    fn test_commit_html_root_commit() {
        // No parents and no diff: the diffstat section should be absent.
        insta::assert_snapshot!(
            render_to_string(&init_tera(), "commit.html", &base_fixture()).unwrap()
        );
    }

    #[test]
    fn test_commit_html_merge_with_diff() {
        let mut template = base_fixture();
        template.parents = vec![
            ParentRef {
                hash: "1111111111111111111111111111111111111111".to_string(),
                short: "11111111".to_string(),
            },
            ParentRef {
                hash: "2222222222222222222222222222222222222222".to_string(),
                short: "22222222".to_string(),
            },
        ];
        template.files = vec![
            FileDiff {
                path: "src/main.rs".to_string(),
                additions: 3,
                deletions: 1,
                bar_add: 30,
                bar_del: 10,
            },
            FileDiff {
                path: "README".to_string(),
                additions: 1,
                deletions: 0,
                bar_add: 10,
                bar_del: 0,
            },
        ];
        template.total_additions = 4;
        template.total_deletions = 1;
        template.diff_lines = vec![
            DiffLine {
                kind: "hunk".to_string(),
                content: "diff --git a/src/main.rs b/src/main.rs".to_string(),
            },
            DiffLine {
                kind: "hunk".to_string(),
                content: "@@ -1,3 +1,5 @@".to_string(),
            },
            DiffLine {
                kind: "ctx".to_string(),
                content: " fn main() {".to_string(),
            },
            DiffLine {
                kind: "del".to_string(),
                content: "-    println!(\"old\");".to_string(),
            },
            DiffLine {
                kind: "add".to_string(),
                content: "+    println!(\"<new> & escaped\");".to_string(),
            },
        ];
        insta::assert_snapshot!(render_to_string(&init_tera(), "commit.html", &template).unwrap());
    }
}
