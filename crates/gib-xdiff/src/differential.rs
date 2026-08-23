//! Differential tests for line diffing, against `git diff --no-index`.
//!
//! The corpus is *generated*, deliberately. The hand-written fixtures used
//! elsewhere in this workspace are files of three distinct lines edited one
//! line at a time, and every diff of such a file is unambiguous — which is
//! exactly the case where any reasonable diff implementation agrees with git.
//! The disagreements live where a change could be slid up or down without
//! becoming less minimal, so the generators below deal in files that are mostly
//! repeated blank lines, closing braces and other filler.
//!
//! git is invoked with `--no-indent-heuristic` because that is what blame does:
//! `blame.c` passes no flags at all, and the indent heuristic is a display
//! nicety git applies to diffs a human will read, not to the hunks it
//! attributes lines from.

use crate::{hunks, unified};
use std::process::Command;
use tempfile::TempDir;

/// A small deterministic PRNG, so a failure reproduces from its seed alone.
/// xorshift64*, which is more than enough to shuffle a corpus around.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// Lines that repeat all over a real source file. Repetition is what makes a
/// change slidable, and slidable changes are where implementations diverge.
const FILLER: &[&str] = &["}", "", "    }", "    ", ")", "};", "#endif", "*/"];

/// A file of `n` lines, roughly a third of them filler.
fn source_file(rng: &mut Rng, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            if rng.below(100) < 35 {
                (*rng.pick(FILLER)).to_string()
            } else {
                format!("code_{i}")
            }
        })
        .collect()
}

/// Apply a handful of insertions, deletions and rewrites, the way a commit does.
fn edit(rng: &mut Rng, lines: &[String]) -> Vec<String> {
    let mut out = lines.to_vec();
    for _ in 0..1 + rng.below(4) {
        if out.is_empty() {
            break;
        }
        let at = rng.below(out.len());
        match rng.below(10) {
            0..=3 => {
                let run: Vec<String> = (0..1 + rng.below(4))
                    .map(|_| (*rng.pick(FILLER)).to_string())
                    .collect();
                out.splice(at..at, run);
            }
            4..=6 => {
                let end = (at + 1 + rng.below(3)).min(out.len());
                out.drain(at..end);
            }
            _ => out[at] = format!("changed_{}", rng.below(1000)),
        }
    }
    out
}

/// A file drawn from a three-letter alphabet: every line matches many others,
/// so nearly every diff is ambiguous. Pathological, and the strongest signal
/// that we are running git's own tie-breaking rather than something that merely
/// agrees with it most of the time.
fn ambiguous_file(rng: &mut Rng, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| (*rng.pick(&["a", "b", "c"])).to_string())
        .collect()
}

fn render(lines: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Run `git diff --no-index` over two files and hand back its stdout.
///
/// `--no-index` exits 1 when the files differ, which is the normal case here;
/// anything above that is a real failure.
fn git_diff(dir: &TempDir, before: &[u8], after: &[u8], context: usize) -> String {
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::write(&a, before).unwrap();
    std::fs::write(&b, after).unwrap();

    let out = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--no-indent-heuristic",
            &format!("-U{context}"),
            "--",
        ])
        .arg(&a)
        .arg(&b)
        .output()
        .expect("git is required to run the differential tests");

    let code = out.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "git diff failed ({code}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git diff emitted invalid UTF-8")
}

/// The changed ranges git reports, read back out of a `-U0` diff.
///
/// With no context, each `@@` header *is* a hunk. A side with a zero count
/// carries the line it sits after rather than a first changed line, which is
/// already the zero-based position; a non-empty side is one-based and needs
/// adjusting.
fn git_hunks(diff: &str) -> Vec<(usize, usize, usize, usize)> {
    diff.lines()
        .filter(|l| l.starts_with("@@"))
        .map(|l| {
            let body = l.trim_start_matches("@@ ");
            let (minus, rest) = body.split_once(' ').expect("malformed hunk header");
            let plus = rest.split(' ').next().expect("malformed hunk header");

            let parse = |field: &str| -> (usize, usize) {
                let field = field.trim_start_matches(['-', '+']);
                match field.split_once(',') {
                    Some((start, count)) => (start.parse().unwrap(), count.parse().unwrap()),
                    None => (field.parse().unwrap(), 1),
                }
            };

            let (s1, c1) = parse(minus);
            let (s2, c2) = parse(plus);
            let start1 = if c1 == 0 { s1 } else { s1 - 1 };
            let start2 = if c2 == 0 { s2 } else { s2 - 1 };
            (start1, c1, start2, c2)
        })
        .collect()
}

/// Our hunks in the same shape, for comparison.
fn our_hunks(before: &[u8], after: &[u8]) -> Vec<(usize, usize, usize, usize)> {
    hunks(before, after)
        .unwrap()
        .into_iter()
        .map(|h| {
            (
                h.before.start,
                h.before.end - h.before.start,
                h.after.start,
                h.after.end - h.after.start,
            )
        })
        .collect()
}

/// The body of a unified diff: everything from the first `@@` on, dropping the
/// `diff --git`/`index`/`---`/`+++` block that names paths we never see.
fn diff_body(diff: &str) -> String {
    match diff.find("\n@@") {
        Some(i) => diff[i + 1..].to_string(),
        None if diff.starts_with("@@") => diff.to_string(),
        None => String::new(),
    }
}

fn check_hunks(dir: &TempDir, before: &[u8], after: &[u8], case: &str) {
    let expected = git_hunks(&git_diff(dir, before, after, 0));
    let got = our_hunks(before, after);
    assert_eq!(
        got,
        expected,
        "hunks disagree with git for {case}\nbefore:\n{}\nafter:\n{}",
        String::from_utf8_lossy(before),
        String::from_utf8_lossy(after)
    );
}

#[test]
fn hunks_match_git_on_realistic_edits() {
    let dir = TempDir::new().unwrap();
    let mut rng = Rng::new(0x5EED_1234);
    for i in 0..400 {
        let before = source_file(&mut rng, 40);
        let after = edit(&mut rng, &before);
        let (before, after) = (render(&before), render(&after));
        if before == after {
            continue;
        }
        check_hunks(&dir, &before, &after, &format!("realistic case {i}"));
    }
}

#[test]
fn hunks_match_git_on_ambiguous_files() {
    let dir = TempDir::new().unwrap();
    let mut rng = Rng::new(0xC0FF_EE01);
    for i in 0..400 {
        let (n1, n2) = (1 + rng.below(12), 1 + rng.below(12));
        let before = ambiguous_file(&mut rng, n1);
        let after = ambiguous_file(&mut rng, n2);
        let (before, after) = (render(&before), render(&after));
        if before == after {
            continue;
        }
        check_hunks(&dir, &before, &after, &format!("ambiguous case {i}"));
    }
}

#[test]
fn hunks_match_git_when_a_file_is_created_or_emptied() {
    let dir = TempDir::new().unwrap();
    let mut rng = Rng::new(0x0BAD_F00D);
    for i in 0..40 {
        let n = 1 + rng.below(20);
        let lines = render(&source_file(&mut rng, n));
        check_hunks(&dir, b"", &lines, &format!("creation {i}"));
        check_hunks(&dir, &lines, b"", &format!("deletion {i}"));
    }
}

#[test]
fn unified_output_matches_git() {
    let dir = TempDir::new().unwrap();
    let mut rng = Rng::new(0xFEED_BEEF);
    for i in 0..300 {
        let before = source_file(&mut rng, 40);
        let after = edit(&mut rng, &before);
        let (before, after) = (render(&before), render(&after));
        if before == after {
            continue;
        }
        let expected = diff_body(&git_diff(&dir, &before, &after, 3));
        let got = String::from_utf8(unified(&before, &after, 3).unwrap()).unwrap();
        assert_eq!(
            got,
            expected,
            "unified diff disagrees with git for case {i}\nbefore:\n{}\nafter:\n{}",
            String::from_utf8_lossy(&before),
            String::from_utf8_lossy(&after)
        );
    }
}
