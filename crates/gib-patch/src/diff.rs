//! Diffing one file's two sides into classified [`PatchLine`]s.
//!
//! The caller loads each side's blob — only it knows how — and hands the bytes
//! to [`diff_file`], which wraps `gib_xdiff`'s unified output in the `diff
//! --git` header block git writes around it.

use gib_hash::ObjectId;
use gib_object::TreeEntryType;
use gib_xdiff::Whitespace;
use gib_xdiff::unified;

/// How to read the two sides of a file.
///
/// [`Default`] is git's default, and the only setting a downloadable `.patch`
/// may be built with — see [`format_patch`], which does not take these at all.
/// They exist for the on-screen diff, where a reader may want wider context or
/// a reindentation hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    /// Lines of context around each hunk. git's `-U`.
    pub context: usize,
    /// Whether a line that differs only in whitespace is a change. git's `-w`.
    pub whitespace: Whitespace,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: 3,
            whitespace: Whitespace::Significant,
        }
    }
}

/// How to read a line of a patch: which of the four things it is that a diff
/// viewer colours differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A `diff --git` header, one of the lines under it, or a `@@` hunk marker.
    Meta,
    /// An added line, or the `+++` header naming the file it was added to.
    Insert,
    /// A removed line, or the `---` header naming the file it came from.
    Delete,
    /// An unchanged line, and the two notes git writes in the same column: the
    /// missing-newline marker and "Binary files ... differ".
    Context,
}

/// One line of a patch, without its terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    /// What the line is, for a caller that renders rather than writes it.
    pub kind: LineKind,
    /// The line as it appears in the patch, leading `+`/`-`/space included.
    pub text: String,
}

impl PatchLine {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// One side of a changed file: the blob recorded for it, and the mode the tree
/// gives it. A side is absent when the file was created or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Side {
    /// The blob the tree points at on this side.
    pub id: ObjectId,
    /// The entry's type, which decides the `100644`-style mode git records.
    pub entry_type: TreeEntryType,
}

impl Side {
    /// The file mode git writes for this side, e.g. `100755`.
    pub(crate) fn mode(&self) -> &'static str {
        match self.entry_type {
            TreeEntryType::File => "100644",
            TreeEntryType::Executable => "100755",
            TreeEntryType::Symlink => "120000",
            TreeEntryType::Tree => "040000",
            TreeEntryType::Commit => "160000",
        }
    }
}

/// The diff of a single changed file: its `diff --git` block and hunks, plus
/// the counts the diffstat is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The file's path, as it appears in the header lines.
    pub path: String,
    /// The lines of the diff, starting with the `diff --git` header.
    pub lines: Vec<PatchLine>,
    /// Lines added, excluding the `+++` header — what a diffstat counts.
    pub additions: usize,
    /// Lines removed, excluding the `---` header.
    pub deletions: usize,
    /// The two sides' byte counts, for a file reported as binary. A binary
    /// file has no line counts, and its diffstat row reports sizes instead.
    // Read by the diffstat and by the patch's header block, both of which
    // live in sibling modules; still crate-private to anyone outside.
    pub(crate) binary_sizes: Option<(usize, usize)>,
    pub(crate) old: Option<Side>,
    pub(crate) new: Option<Side>,
}

/// One file's `diff --git` block, before it is folded into a [`FileDiff`].
/// A typechange is written as two of these.
struct Block {
    lines: Vec<PatchLine>,
    additions: usize,
    deletions: usize,
    binary_sizes: Option<(usize, usize)>,
}

/// A blob is treated as binary if a NUL byte appears in its leading bytes.
/// git scans the first 8000, so do the same.
#[must_use]
pub fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

/// The seven-digit abbreviation git writes in an `index` line. The absent side
/// of a creation or a deletion is all zeroes, which is what git prints for it.
fn abbrev(side: Option<Side>) -> String {
    match side {
        Some(side) => format!("{}", side.id)[..7].to_string(),
        None => "0000000".to_string(),
    }
}

/// Diff one changed file. `old`/`new` are the tree's entries for it, absent on
/// the side where the file did not exist; `old_data`/`new_data` are those
/// blobs' bytes, empty for an absent side.
///
/// The returned lines always begin with the `diff --git` header and the block
/// under it — the mode lines a creation, deletion or `chmod` calls for, and the
/// `index` line whenever the two sides are different objects. A file detected
/// as binary gets that header and git's "Binary files ... differ" note in place
/// of hunks, and no counts.
///
/// `options` shapes the hunks and so the counts with them: a diff read with
/// [`Whitespace::Ignore`] reports fewer changed lines than the same file read
/// with [`DiffOptions::default`]. Pass the default for anything that has to be
/// a patch rather than a view of one.
#[must_use]
pub fn diff_file(
    path: &str,
    old: Option<Side>,
    new: Option<Side>,
    old_data: &[u8],
    new_data: &[u8],
    options: DiffOptions,
) -> FileDiff {
    let mut diff = FileDiff {
        path: path.to_string(),
        lines: Vec::new(),
        additions: 0,
        deletions: 0,
        binary_sizes: None,
        old,
        new,
    };

    // A file that becomes a symlink (or the other way round) cannot be diffed
    // against its own replacement — the two are not the same kind of thing — so
    // git writes the change as a deletion followed by a creation, and still
    // reports it as the one file it is. Do the same, and the diffstat row and
    // its "mode change" summary line fall out of the entry's two real sides.
    if let (Some(old_side), Some(new_side)) = (old, new)
        && kind(old_side.entry_type) != kind(new_side.entry_type)
    {
        let removal = file_block(path, old, None, old_data, &[], options);
        let addition = file_block(path, None, new, &[], new_data, options);
        diff.deletions = removal.deletions;
        diff.additions = addition.additions;
        diff.binary_sizes = match (removal.binary_sizes, addition.binary_sizes) {
            (None, None) => None,
            _ => Some((old_data.len(), new_data.len())),
        };
        diff.lines = removal.lines;
        diff.lines.extend(addition.lines);
        return diff;
    }

    let block = file_block(path, old, new, old_data, new_data, options);
    diff.lines = block.lines;
    diff.additions = block.additions;
    diff.deletions = block.deletions;
    diff.binary_sizes = block.binary_sizes;
    diff
}

/// The kind of thing a tree entry names, in the sense that matters to a diff:
/// switching between two kinds is a typechange, where gaining the executable
/// bit is only a mode change.
fn kind(entry_type: TreeEntryType) -> u8 {
    match entry_type {
        TreeEntryType::File | TreeEntryType::Executable => b'f',
        TreeEntryType::Symlink => b'l',
        TreeEntryType::Tree => b't',
        TreeEntryType::Commit => b'c',
    }
}

/// One `diff --git` block: the header lines and either hunks or the binary
/// note. Always has a side on both ends of an edit, or exactly one side for a
/// creation or a deletion.
fn file_block(
    path: &str,
    old: Option<Side>,
    new: Option<Side>,
    old_data: &[u8],
    new_data: &[u8],
    options: DiffOptions,
) -> Block {
    let mut lines = vec![PatchLine::new(
        LineKind::Meta,
        format!("diff --git a/{path} b/{path}"),
    )];

    match (old, new) {
        (None, Some(new)) => lines.push(PatchLine::new(
            LineKind::Meta,
            format!("new file mode {}", new.mode()),
        )),
        (Some(old), None) => lines.push(PatchLine::new(
            LineKind::Meta,
            format!("deleted file mode {}", old.mode()),
        )),
        // A file that only changed mode says so in two lines of its own, and
        // git then leaves the `index` line off entirely — the blob is the same
        // one, so there is nothing for it to report.
        (Some(old), Some(new)) if old.mode() != new.mode() => {
            lines.push(PatchLine::new(
                LineKind::Meta,
                format!("old mode {}", old.mode()),
            ));
            lines.push(PatchLine::new(
                LineKind::Meta,
                format!("new mode {}", new.mode()),
            ));
        }
        _ => {}
    }

    let same_object = matches!((old, new), (Some(o), Some(n)) if o.id == n.id);
    if !same_object {
        // The mode is repeated on the index line only when it is not already
        // spelled out above, i.e. when the file exists on both sides with the
        // same mode.
        let mode = match (old, new) {
            (Some(old), Some(new)) if old.mode() == new.mode() => format!(" {}", old.mode()),
            _ => String::new(),
        };
        lines.push(PatchLine::new(
            LineKind::Meta,
            format!("index {}..{}{mode}", abbrev(old), abbrev(new)),
        ));
    }

    // The file headers name /dev/null on the side where the file is absent,
    // which is how `git apply` tells a creation or deletion from an edit.
    let left = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_string()
    };
    let right = if new.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".to_string()
    };

    let mut diff = Block {
        lines,
        additions: 0,
        deletions: 0,
        binary_sizes: None,
    };

    // git shows "Binary files differ" rather than a line-by-line diff, which
    // would be meaningless (and potentially huge).
    if is_binary(old_data) || is_binary(new_data) {
        diff.binary_sizes = Some((old_data.len(), new_data.len()));
        diff.lines.push(PatchLine::new(
            LineKind::Context,
            format!("Binary files {left} and {right} differ"),
        ));
        return diff;
    }

    // Render the diff with xdiff
    let body = match unified(old_data, new_data, options.context, options.whitespace) {
        Ok(body) => body,
        // xdiff only fails when an allocation does, at which point the tab has
        // larger problems than one unrendered diff.
        Err(_) => return diff,
    };

    // Two sides with the same content produce no hunks, and then no file
    // headers either — a mode-only change is a header block and nothing else.
    if body.is_empty() {
        return diff;
    }

    // The file headers precede the first hunk.
    diff.lines
        .push(PatchLine::new(LineKind::Delete, format!("--- {left}")));
    diff.lines
        .push(PatchLine::new(LineKind::Insert, format!("+++ {right}")));

    // xdiff terminates every line it emits, so the trailing newline would
    // otherwise split into a final empty line that is not part of the diff.
    let body = body.strip_suffix(b"\n").unwrap_or(&body);
    for line in body.split(|&b| b == b'\n') {
        // A line's first byte is its marker, exactly as it will be rendered:
        // `@` for a hunk header, `+`/`-` for a change, a space for context and
        // a backslash for the no-newline note. Counts exclude the `---`/`+++`
        // headers above, which is what git's diffstat does.
        let kind = match line.first() {
            Some(b'@') => LineKind::Meta,
            Some(b'+') => {
                diff.additions += 1;
                LineKind::Insert
            }
            Some(b'-') => {
                diff.deletions += 1;
                LineKind::Delete
            }
            _ => LineKind::Context,
        };
        // A blob is not required to be UTF-8, and a patch of one still has to
        // render; git passes the bytes through and so would we, but `PatchLine`
        // holds a `String`.
        diff.lines
            .push(PatchLine::new(kind, String::from_utf8_lossy(line)));
    }

    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(hex: &[u8], entry_type: TreeEntryType) -> Option<Side> {
        Some(Side {
            id: ObjectId::from_hex(hex).unwrap(),
            entry_type,
        })
    }

    fn edit(path: &str, old_data: &[u8], new_data: &[u8]) -> FileDiff {
        diff_file(
            path,
            side(
                b"1111111111111111111111111111111111111111",
                TreeEntryType::File,
            ),
            side(
                b"2222222222222222222222222222222222222222",
                TreeEntryType::File,
            ),
            old_data,
            new_data,
            DiffOptions::default(),
        )
    }

    fn texts(diff: &FileDiff) -> Vec<&str> {
        diff.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn test_diff_file_modification() {
        let diff = edit("foo.txt", b"alpha\nbeta\n", b"alpha\nbeta changed\n");
        assert_eq!((diff.additions, diff.deletions), (1, 1));
        // The header block: what changed, and between which two objects.
        assert_eq!(
            &texts(&diff)[..4],
            &[
                "diff --git a/foo.txt b/foo.txt",
                "index 1111111..2222222 100644",
                "--- a/foo.txt",
                "+++ b/foo.txt",
            ]
        );
        // The changed lines are classified for a renderer to colour.
        assert!(
            diff.lines
                .iter()
                .any(|l| l.kind == LineKind::Meta && l.text.starts_with("@@"))
        );
        assert!(diff.lines.iter().any(|l| l.kind == LineKind::Insert
            && l.text.starts_with('+')
            && !l.text.starts_with("+++")));
        assert!(diff.lines.iter().any(|l| l.kind == LineKind::Delete
            && l.text.starts_with('-')
            && !l.text.starts_with("---")));
    }

    #[test]
    fn test_diff_file_pure_addition_and_deletion() {
        // A brand-new file: every line counts as an addition, none as deletion,
        // and the side it never had is /dev/null.
        let created = diff_file(
            "new.txt",
            None,
            side(
                b"2222222222222222222222222222222222222222",
                TreeEntryType::File,
            ),
            b"",
            b"one\ntwo\nthree\n",
            DiffOptions::default(),
        );
        assert_eq!((created.additions, created.deletions), (3, 0));
        assert_eq!(
            &texts(&created)[..5],
            &[
                "diff --git a/new.txt b/new.txt",
                "new file mode 100644",
                "index 0000000..2222222",
                "--- /dev/null",
                "+++ b/new.txt",
            ]
        );

        // A removed file: the reverse.
        let deleted = diff_file(
            "gone.txt",
            side(
                b"1111111111111111111111111111111111111111",
                TreeEntryType::File,
            ),
            None,
            b"one\ntwo\n",
            b"",
            DiffOptions::default(),
        );
        assert_eq!((deleted.additions, deleted.deletions), (0, 2));
        assert_eq!(
            &texts(&deleted)[..5],
            &[
                "diff --git a/gone.txt b/gone.txt",
                "deleted file mode 100644",
                "index 1111111..0000000",
                "--- a/gone.txt",
                "+++ /dev/null",
            ]
        );
    }

    #[test]
    fn test_diff_file_mode_change_only() {
        // The same blob with the executable bit added: two mode lines, and no
        // index line, because there is no object change to report.
        let diff = diff_file(
            "script.sh",
            side(
                b"1111111111111111111111111111111111111111",
                TreeEntryType::File,
            ),
            side(
                b"1111111111111111111111111111111111111111",
                TreeEntryType::Executable,
            ),
            b"#!/bin/sh\n",
            b"#!/bin/sh\n",
            DiffOptions::default(),
        );
        assert_eq!((diff.additions, diff.deletions), (0, 0));
        assert_eq!(
            texts(&diff),
            &[
                "diff --git a/script.sh b/script.sh",
                "old mode 100644",
                "new mode 100755",
            ]
        );
    }

    #[test]
    fn test_diff_file_typechange_is_a_delete_and_an_add() {
        // A file replaced by a symlink can't be diffed against its replacement,
        // so it is written as two blocks — but stays one changed file.
        let diff = diff_file(
            "becomes-link",
            side(
                b"1111111111111111111111111111111111111111",
                TreeEntryType::File,
            ),
            side(
                b"2222222222222222222222222222222222222222",
                TreeEntryType::Symlink,
            ),
            b"a regular file\n",
            b"target",
            DiffOptions::default(),
        );
        assert_eq!((diff.additions, diff.deletions), (1, 1));
        let texts = texts(&diff);
        assert_eq!(texts[1], "deleted file mode 100644");
        assert_eq!(
            texts.iter().filter(|t| t.starts_with("diff --git")).count(),
            2
        );
        assert!(texts.contains(&"new file mode 120000"));
    }

    #[test]
    fn test_diff_file_binary() {
        // A NUL byte makes the file binary: no line-by-line diff, zero counts.
        let diff = edit("blob.bin", b"\0\x01\x02", b"\0\x03");
        assert_eq!((diff.additions, diff.deletions), (0, 0));
        assert_eq!(
            texts(&diff),
            &[
                "diff --git a/blob.bin b/blob.bin",
                "index 1111111..2222222 100644",
                "Binary files a/blob.bin and b/blob.bin differ",
            ]
        );
        assert_eq!(diff.lines[2].kind, LineKind::Context);
    }

    /// The shapes the line-by-line emission has to get right, now that the
    /// hunks themselves come from xdiff: a plain modification, a file with no
    /// trailing newline on either side, CRLF endings (the CR belongs to the
    /// line and git keeps it), multiple hunks (one file header, one `@@` per
    /// hunk), an identical pair (no hunks, so no header at all), and invalid
    /// UTF-8, which decodes lossily rather than being dropped.
    ///
    /// Agreement with git itself is the differential suite's job; this pins the
    /// framing around what xdiff hands back.
    #[test]
    fn test_diff_file_emission() {
        let modified = edit("foo.txt", b"alpha\nbeta\n", b"alpha\nbeta changed\n");
        let lines = texts(&modified);
        assert_eq!(lines[2], "--- a/foo.txt");
        assert_eq!(lines[3], "+++ b/foo.txt");
        assert_eq!(lines[4], "@@ -1,2 +1,2 @@");
        assert_eq!(lines[5], " alpha");
        assert_eq!(lines[6], "-beta");
        assert_eq!(lines[7], "+beta changed");
        assert_eq!((modified.additions, modified.deletions), (1, 1));

        // The marker is xdiff's, and it is a context line so a viewer does not
        // colour it as a change.
        let no_newline = edit("foo.txt", b"alpha\nbeta\n", b"alpha\nbeta");
        let lines = texts(&no_newline);
        assert!(
            lines.contains(&"\\ No newline at end of file"),
            "expected a no-newline marker in {lines:?}"
        );
        let marker = no_newline
            .lines
            .iter()
            .find(|l| l.text.starts_with('\\'))
            .expect("marker present");
        assert_eq!(marker.kind, LineKind::Context);

        // A CR is part of the line's content, and git leaves it there.
        let crlf = edit(
            "foo.txt",
            b"alpha\r\nbeta\r\n",
            b"alpha\r\nbeta changed\r\n",
        );
        assert!(
            texts(&crlf).contains(&"-beta\r"),
            "expected the CR to survive in {:?}",
            texts(&crlf)
        );

        // Two well-separated edits: one file header, two hunk headers.
        let two_hunks = edit(
            "foo.txt",
            b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n",
            b"1\n2\nx\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\ny\n20\n",
        );
        let lines = texts(&two_hunks);
        assert_eq!(lines.iter().filter(|l| l.starts_with("--- ")).count(), 1);
        assert_eq!(lines.iter().filter(|l| l.starts_with("@@")).count(), 2);
        assert_eq!((two_hunks.additions, two_hunks.deletions), (2, 2));

        // Identical content produces no hunks, and so no file header either.
        let same = edit("foo.txt", b"same\n", b"same\n");
        let lines = texts(&same);
        assert!(
            !lines.iter().any(|l| l.starts_with("--- ")),
            "expected no file header in {lines:?}"
        );
        assert_eq!((same.additions, same.deletions), (0, 0));

        // Creation from nothing: every line is an addition.
        let created = edit("foo.txt", b"", b"only\n");
        assert_eq!((created.additions, created.deletions), (1, 0));

        // Invalid UTF-8 decodes lossily instead of panicking or vanishing.
        let lossy = edit("foo.txt", b"caf\xc3\xa9\n", b"caf\xff\n");
        assert_eq!((lossy.additions, lossy.deletions), (1, 1));
        assert!(
            texts(&lossy)
                .iter()
                .any(|l| l.starts_with('+') && !l.starts_with("+++") && l.contains('\u{fffd}')),
            "expected a replacement character in {:?}",
            texts(&lossy)
        );
    }

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b""));
        assert!(!is_binary(b"hello\nworld\n"));
        // UTF-8 multibyte content has no NUL bytes and must stay textual.
        assert!(!is_binary("café — résumé".as_bytes()));
        assert!(is_binary(b"PK\x03\x04\0\0"));
        assert!(is_binary(b"text then \0 nul"));
        // A NUL past the 8000-byte scan window is not flagged, matching git.
        let mut late_nul = vec![b'a'; 8000];
        late_nul.push(0);
        assert!(!is_binary(&late_nul));
    }
}
