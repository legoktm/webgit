//! The commit view's tests. The fixtures are shared: `base_fixture` is the
//! commit every snapshot starts from, and `row` builds the diffed file rows
//! the diffstat and the diff body are rendered out of.

use super::patch::build_patch;
use super::side_by_side::{SideRow, side_rows};
use super::stream::recompute_bars;
use super::*;
use crate::route::DiffMode;
use gib_patch::{DiffOptions, Side};

/// Render `CommitView` to a static HTML string via SSR. See `render::tag`
/// for why we go through SSR rather than comparing `VNode`s.
fn render(props: CommitProps) -> String {
    let html = futures::executor::block_on(
        yew::ServerRenderer::<CommitView>::with_props(move || props)
            .hydratable(false)
            .render(),
    );
    prettify(&html)
}

/// Break adjacent tags onto their own lines for readable snapshots, but emit
/// the contents of `<pre>` elements verbatim. Unlike the other ported views,
/// the commit diff renders sibling `<span>`s *inside* `<pre class="diff-pre">`,
/// so the blanket `><` split the others use would inject newlines into
/// whitespace the browser renders literally.
fn prettify(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find("<pre") {
        out.push_str(&rest[..open].replace("><", ">\n<"));
        rest = &rest[open..];
        match rest.find("</pre>") {
            Some(close) => {
                let end = close + "</pre>".len();
                out.push_str(&rest[..end]);
                rest = &rest[end..];
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
    out.push_str(&rest.replace("><", ">\n<"));
    out
}

fn base_fixture() -> CommitProps {
    CommitProps {
        meta: PatchMeta {
            hash: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            author_name: "Kunal Mehta".to_string(),
            author_email: "author@example.org".to_string(),
            date: "Thu, 15 Jan 2026 12:34:56 +0000".to_string(),
            message: MESSAGE.to_string(),
        },
        author_name: "Kunal Mehta".to_string(),
        author_email: "author@example.org".to_string(),
        author_date: "2026-01-15 12:34:56 +00:00".to_string(),
        committer_name: "Committer Person".to_string(),
        committer_email: "committer@example.org".to_string(),
        committer_date: "2026-01-15 13:00:00 +00:00".to_string(),
        parents: vec![],
        tree_hash: "fedcba98fedcba98fedcba98fedcba98fedcba98".to_string(),
        message: linkify_message(MESSAGE),
        notes: None,
        total_additions: 0,
        total_deletions: 0,
        files: vec![],
        complete: true,
        url_sha: "abcdef01abcdef01abcdef01abcdef01abcdef01".to_string(),
        view: DiffView::default(),
    }
}

const MESSAGE: &str =
    "Fix the thing\n\nLonger explanation with <html> & \"chars\".\nSee 0123abcd for context.";

/// A file row with a diff already in it, as the view holds one once its
/// blobs have loaded.
fn row(path: &str, old: &[u8], new: &[u8], bar_add: usize, bar_del: usize) -> FileRow {
    // The two sides carry different (made-up) blob ids, as a real change
    // does, so the `index` header line comes out as it would in the app.
    let blob = |data: &[u8], hex: &[u8]| {
        (!data.is_empty()).then_some(Side {
            id: ObjectId::from_hex(hex).unwrap(),
            entry_type: gib::object::TreeEntryType::File,
        })
    };
    FileRow {
        path: path.to_string(),
        diff: Some(gib_patch::diff_file(
            path,
            blob(old, b"1111111111111111111111111111111111111111"),
            blob(new, b"2222222222222222222222222222222222222222"),
            old,
            new,
            DiffOptions::default(),
        )),
        patch_diff: None,
        bar_add,
        bar_del,
    }
}

/// A file row whose diff has `additions` added lines and `deletions`
/// removed ones, for the bar arithmetic.
fn file(path: &str, additions: usize, deletions: usize) -> FileRow {
    let old = "old\n".repeat(deletions);
    let new = "new\n".repeat(additions);
    row(path, old.as_bytes(), new.as_bytes(), 0, 0)
}

#[test]
fn test_recompute_bars_normalises_to_widest() {
    // The widest file (4 changes) fills the 40-column bar split by its
    // add/del ratio; a smaller file scales proportionally.
    let mut files = vec![file("big.rs", 3, 1), file("small", 1, 0)];
    recompute_bars(&mut files);
    assert_eq!((files[0].bar_add, files[0].bar_del), (30, 10));
    assert_eq!((files[1].bar_add, files[1].bar_del), (10, 0));
}

#[test]
fn test_recompute_bars_rescales_as_files_stream_in() {
    // Mid-stream the first (and only) file owns the full bar...
    let mut files = vec![file("first", 4, 0)];
    recompute_bars(&mut files);
    assert_eq!((files[0].bar_add, files[0].bar_del), (40, 0));
    // ...then a larger file arriving rescales the earlier one down.
    files.push(file("bigger", 0, 8));
    recompute_bars(&mut files);
    assert_eq!((files[0].bar_add, files[0].bar_del), (20, 0));
    assert_eq!((files[1].bar_add, files[1].bar_del), (0, 40));
}

#[test]
fn test_linkify_message() {
    use MessageSegment::{Sha, Text};
    // A 7-40 char hex run becomes a SHA segment; surrounding text stays text.
    assert_eq!(
        linkify_message("see 0123abcd <ok>"),
        vec![
            Text("see ".to_string()),
            Sha("0123abcd".to_string()),
            Text(" <ok>".to_string()),
        ]
    );
    // Full 40-char SHA-1.
    let full = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(linkify_message(full), vec![Sha(full.to_string())]);
    // Too short (<7), too long (>40), uppercase, or embedded in a word: text.
    assert_eq!(linkify_message("abc123"), vec![Text("abc123".to_string())]);
    assert_eq!(
        linkify_message("0123ABCD"),
        vec![Text("0123ABCD".to_string())]
    );
    assert_eq!(
        linkify_message("x0123abcd"),
        vec![Text("x0123abcd".to_string())]
    );
    assert_eq!(linkify_message(&"a".repeat(41)), vec![Text("a".repeat(41))]);
}

#[test]
fn test_linkify_message_links_all_digit_runs() {
    use MessageSegment::Sha;
    // An all-digit run is a valid abbreviation, so it is linkified like any
    // other. Resolution decides whether it names anything — a date or a bug
    // number simply reports "unknown SHA" when followed.
    for token in ["20250101", "1234567", "1234567890", "2025010a"] {
        assert_eq!(
            linkify_message(token),
            vec![Sha(token.to_string())],
            "{token} should be linkified"
        );
    }
}

#[test]
fn test_linkify_message_no_spurious_links() {
    // Markup and quotes are never mistaken for SHAs: the whole string stays
    // one text run, and Yew escapes it at render time (see the snapshots).
    let attack = "<script>alert('xss')</script> & \"quotes\"";
    assert_eq!(
        linkify_message(attack),
        vec![MessageSegment::Text(attack.to_string())]
    );
}

/// A commit view with one modified file, as it looks once its diff has
/// streamed in.
fn patch_fixture() -> CommitProps {
    let mut props = base_fixture();
    props.parents = vec![ParentRef {
        hash: "1111111111111111111111111111111111111111".to_string(),
        short: "11111111".to_string(),
    }];
    props.files = vec![row(
        "src/main.rs",
        b"alpha\nbeta\n",
        b"alpha\nbeta changed\n",
        20,
        20,
    )];
    props.total_additions = 1;
    props.total_deletions = 1;
    props
}

#[test]
fn test_build_patch() {
    // What the "(patch)" link downloads. The format itself is `gib-patch`'s
    // and is checked against `git format-patch` there; what this pins is
    // the wiring — the view's metadata and its streamed diff reaching the
    // patch writer intact, under this build's signature.
    let patch = build_patch(&patch_fixture()).replace(crate::render::about::COMMIT, "<version>");
    insta::assert_snapshot!(patch);
}

#[test]
fn test_the_patch_keeps_the_contact_the_commit_records() {
    // The header shows the mailmapped author; the patch must not, because
    // `git format-patch` is not one of the commands `log.mailmap` applies
    // to — a patch carries the authorship it will be applied under.
    let mut props = patch_fixture();
    props.author_name = "Proper Name".to_string();
    props.author_email = "proper@example.org".to_string();

    let patch = build_patch(&props);
    assert!(
        patch.contains("From: Kunal Mehta <author@example.org>"),
        "{patch}"
    );
    assert!(!patch.contains("proper@example.org"), "{patch}");

    let rendered = render(props);
    assert!(
        rendered.contains("Proper Name &lt;proper@example.org&gt;"),
        "{rendered}"
    );
}

#[test]
fn test_patch_link_waits_for_the_whole_diff() {
    // A patch built from an unfinished view would be missing files, so the
    // link is only offered once the diff is done...
    let props = patch_fixture();
    assert!(render(props.clone()).contains("patch-link"));

    // ...both while a file's blobs are still loading...
    let mut streaming = props.clone();
    streaming.complete = false;
    streaming.files[0].diff = None;
    assert!(!render(streaming).contains("patch-link"));

    // ...and on the metadata-only first frame, whose empty file list would
    // otherwise look exactly like a root commit's and hand over a patch
    // with no diff in it.
    let mut first_frame = props;
    first_frame.complete = false;
    first_frame.files = vec![];
    assert!(!render(first_frame).contains("patch-link"));
}

#[test]
fn test_commit_html_root_commit() {
    // No parents and no diff: the diffstat section should be absent.
    insta::assert_snapshot!(render(base_fixture()));
}

#[test]
fn test_commit_html_root_commit_with_diff() {
    // A root commit is diffed against the empty tree, so it has files like
    // any other commit: the diffstat and the diff itself both render, with
    // no `parent` row above them.
    let mut props = base_fixture();
    props.files = vec![row("src/main.rs", b"", b"alpha\nbeta\n", 40, 0)];
    props.total_additions = 2;
    insta::assert_snapshot!(render(props));
}

#[test]
fn test_commit_html_merge_with_diff() {
    let mut props = base_fixture();
    props.parents = vec![
        ParentRef {
            hash: "1111111111111111111111111111111111111111".to_string(),
            short: "11111111".to_string(),
        },
        ParentRef {
            hash: "2222222222222222222222222222222222222222".to_string(),
            short: "22222222".to_string(),
        },
    ];
    props.files = vec![
        row(
            "src/main.rs",
            b"fn main() {\n    println!(\"old\");\n}\n",
            b"fn main() {\n    println!(\"<new> & escaped\");\n}\n",
            30,
            10,
        ),
        row("README", b"", b"hello\n", 10, 0),
    ];
    props.total_additions = 2;
    props.total_deletions = 1;
    insta::assert_snapshot!(render(props));
}

/// A commit whose diff exercises the side-by-side cases at once: two lines
/// that pair one-for-one, an unchanged line between the changes, and a pure
/// insertion, which leaves the left column with nothing to show.
fn uneven_fixture() -> CommitProps {
    let mut props = base_fixture();
    props.files = vec![row(
        "src/main.rs",
        b"fn main() {\n    a();\n    b();\n    c();\n}\n",
        b"fn main() {\n    A();\n    merged();\n    c();\n    extra();\n}\n",
        30,
        10,
    )];
    props.total_additions = 3;
    props.total_deletions = 3;
    props
}

#[test]
fn test_commit_html_side_by_side() {
    // Two columns, with a hatched cell wherever one side's run is shorter
    // than the other's. The panel shows "side by side" as the setting in
    // force, and every link it offers carries `ss=1` forward.
    let mut props = uneven_fixture();
    props.view = DiffView {
        side_by_side: true,
        ..DiffView::default()
    };
    insta::assert_snapshot!(render(props));
}

#[test]
fn test_commit_html_stat_only() {
    // The diffstat alone. The diff body is gone, both layout segments are
    // dead because there is no layout left to choose, and `ss` has dropped
    // out of every URL the panel builds.
    let mut props = uneven_fixture();
    props.view = DiffView {
        mode: DiffMode::StatOnly,
        side_by_side: true,
        ..DiffView::default()
    };
    insta::assert_snapshot!(render(props));
}

#[test]
fn test_commit_html_wide_context_and_ignored_whitespace() {
    // Non-default settings: the panel shows them as current, the stepper's
    // `+` has run off the end of the ladder, and "reset to defaults" is a
    // live link for the first time.
    let mut props = uneven_fixture();
    props.view = DiffView {
        context: Some(40),
        ignore_whitespace: true,
        ..DiffView::default()
    };
    insta::assert_snapshot!(render(props));
}

#[test]
fn side_by_side_pairs_runs_and_blanks_the_shorter_one() {
    let props = uneven_fixture();
    let lines = &props.files[0].diff.as_ref().unwrap().lines;
    let rows = side_rows(lines.iter());

    let changes: Vec<(Option<&str>, Option<&str>)> = rows
        .iter()
        .filter_map(|r| match r {
            SideRow::Change { del, ins } => Some((*del, *ins)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        vec![
            // A run of two removals against a run of two additions pairs
            // up in order...
            (Some("    a();"), Some("    A();")),
            (Some("    b();"), Some("    merged();")),
            // ...and an addition with no removal opposite it leaves the
            // left column blank.
            (None, Some("    extra();")),
        ]
    );

    // The `---`/`+++` headers are Delete/Insert lines but not part of any
    // run: they span both columns, as the `@@` marker does.
    let spans: Vec<&str> = rows
        .iter()
        .filter_map(|r| match r {
            SideRow::Span(line) => Some(line.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(spans.contains(&"--- a/src/main.rs"), "got {spans:?}");
    assert!(spans.contains(&"+++ b/src/main.rs"), "got {spans:?}");
}

#[test]
fn side_by_side_blanks_the_right_when_lines_are_only_removed() {
    // The mirror of the case above: three lines collapse to one, so two
    // rows have nothing on the right.
    let file = row(
        "src/main.rs",
        b"keep\na\nb\nc\nkeep\n",
        b"keep\nonly\nkeep\n",
        40,
        0,
    );
    let rows = side_rows(file.diff.as_ref().unwrap().lines.iter());
    let changes: Vec<(Option<&str>, Option<&str>)> = rows
        .iter()
        .filter_map(|r| match r {
            SideRow::Change { del, ins } => Some((*del, *ins)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        vec![
            (Some("a"), Some("only")),
            (Some("b"), None),
            (Some("c"), None),
        ]
    );
}

#[test]
fn the_patch_download_stays_at_gits_defaults() {
    // What the reader sees and what `git apply` can consume are two
    // different documents once the controls have moved: a `-w` diff prints
    // one side's bytes for a line both sides have, and would not apply.
    let mut props = base_fixture();
    let options = DiffOptions {
        context: 1,
        whitespace: gib_patch::Whitespace::Ignore,
    };
    let before = b"fn f() {\n    a();\n    b();\n}\n";
    let after = b"fn f() {\n\ta();\n    c();\n}\n";
    let side = |hex: &[u8]| {
        Some(Side {
            id: ObjectId::from_hex(hex).unwrap(),
            entry_type: gib::object::TreeEntryType::File,
        })
    };
    let (old, new) = (
        side(b"1111111111111111111111111111111111111111"),
        side(b"2222222222222222222222222222222222222222"),
    );
    let diff = |options| {
        Some(gib_patch::diff_file(
            "src/main.rs",
            old,
            new,
            before,
            after,
            options,
        ))
    };
    props.files = vec![FileRow {
        path: "src/main.rs".to_string(),
        diff: diff(options),
        patch_diff: diff(DiffOptions::default()),
        bar_add: 40,
        bar_del: 0,
    }];

    // On screen the reindented `a();` is not a change; in the patch it is.
    let shown = render(props.clone());
    assert!(!shown.contains("-    a();"), "got:\n{shown}");
    let patch = build_patch(&props);
    assert!(patch.contains("-    a();"), "got:\n{patch}");
    assert!(patch.contains("+\ta();"), "got:\n{patch}");
}

#[test]
fn test_commit_html_diff_pending() {
    // The streamed first frame: the changed-file list is known (from the
    // tree diff) but no blobs have loaded, so every row is pending — name
    // shown, stats columns blank — the diff body is still empty, and the
    // patch link has nothing to offer yet.
    let mut props = base_fixture();
    props.parents = vec![ParentRef {
        hash: "1111111111111111111111111111111111111111".to_string(),
        short: "11111111".to_string(),
    }];
    props.complete = false;
    props.files = ["src/main.rs", "README"]
        .into_iter()
        .map(|path| FileRow {
            path: path.to_string(),
            diff: None,
            patch_diff: None,
            bar_add: 0,
            bar_del: 0,
        })
        .collect();
    insta::assert_snapshot!(render(props));
}

#[test]
fn test_commit_html_with_notes() {
    // A note is rendered under its own heading, below the message and
    // above the diff, with the same SHA linkification the message gets.
    let mut props = base_fixture();
    props.notes = Some(linkify_message(
        "Cherry-picked from 0123abcd.\nReviewed with <angle> & \"quoted\" text.\n",
    ));
    insta::assert_snapshot!(render(props));
}

#[test]
fn commit_html_without_notes_has_no_notes_markup() {
    // The usual case: no notes ref, or none for this commit. Nothing is
    // drawn for it — not an empty box, and no heading.
    let html = render(base_fixture());
    assert!(!html.contains("notes"));
}
