//! Unit tests for the FFI boundary: that hunks come back with the coordinates
//! we claim, and that the emitter's output is shaped the way a patch writer
//! expects. Agreement with git itself is `differential.rs`'s job.

use crate::{hunks, unified};

fn text(lines: &[&str]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
        .collect()
}

#[test]
fn identical_files_have_no_hunks() {
    let a = text(&["one", "two"]);
    assert_eq!(hunks(&a, &a).unwrap(), vec![]);
    assert!(unified(&a, &a, 3).unwrap().is_empty());
}

#[test]
fn empty_files_have_no_hunks() {
    assert_eq!(hunks(b"", b"").unwrap(), vec![]);
}

#[test]
fn a_replacement_covers_both_sides() {
    let before = text(&["context", "old", "trailer"]);
    let after = text(&["context", "new", "trailer"]);
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, 1..2);
    assert_eq!(got[0].after, 1..2);
}

#[test]
fn an_insertion_has_an_empty_before_range_at_its_position() {
    let before = text(&["one", "two"]);
    let after = text(&["one", "inserted", "two"]);
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 1);
    // Empty, but positioned: the insert sits between before-lines 1 and 2.
    assert_eq!(got[0].before, 1..1);
    assert_eq!(got[0].after, 1..2);
}

#[test]
fn a_deletion_has_an_empty_after_range_at_its_position() {
    let before = text(&["one", "doomed", "two"]);
    let after = text(&["one", "two"]);
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, 1..2);
    assert_eq!(got[0].after, 1..1);
}

#[test]
fn creating_a_file_is_one_hunk_covering_it() {
    let after = text(&["a", "b", "c"]);
    let got = hunks(b"", &after).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, 0..0);
    assert_eq!(got[0].after, 0..3);
}

#[test]
fn separate_edits_are_separate_hunks() {
    let before = text(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
    let after = text(&["A", "b", "c", "d", "e", "f", "g", "h", "i", "J"]);
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].before, 0..1);
    assert_eq!(got[1].before, 9..10);
}

#[test]
fn the_change_is_slid_the_way_git_slides_it() {
    // Deleting one of two identical lines is ambiguous: the diff is equally
    // minimal either way. xdl_change_compact slides the run down, so it is the
    // *second* "b" that goes. Getting this wrong is invisible in a patch and
    // very visible in a blame annotation, which is the whole reason this crate
    // exists.
    let before = text(&["a", "b", "b", "c"]);
    let after = text(&["a", "b", "c"]);
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, 2..3);
}

#[test]
fn unified_output_carries_hunk_headers_and_markers() {
    let before = text(&["one", "two", "three"]);
    let after = text(&["one", "TWO", "three"]);
    let out = String::from_utf8(unified(&before, &after, 1).unwrap()).unwrap();
    assert_eq!(
        out,
        "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n",
        "got:\n{out}"
    );
}

#[test]
fn unified_context_width_is_honoured() {
    let before = text(&["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    let after = text(&["1", "2", "3", "4", "X", "6", "7", "8", "9"]);
    let tight = String::from_utf8(unified(&before, &after, 1).unwrap()).unwrap();
    assert!(tight.starts_with("@@ -4,3 +4,3 @@"), "got:\n{tight}");

    let wide = String::from_utf8(unified(&before, &after, 3).unwrap()).unwrap();
    assert!(wide.starts_with("@@ -2,7 +2,7 @@"), "got:\n{wide}");
}

#[test]
fn unified_emits_the_enclosing_function_suffix() {
    // xemit's funcname heuristic: the nearest preceding line that starts with
    // an alphabetic character in column zero. This is the piece a hand-rolled
    // unified-diff writer tends not to have.
    let before = text(&["fn outer() {", "    let x = 1;", "    let y = 2;", "}"]);
    let after = text(&["fn outer() {", "    let x = 1;", "    let y = 3;", "}"]);
    let out = String::from_utf8(unified(&before, &after, 1).unwrap()).unwrap();
    assert!(
        out.starts_with("@@ -2,3 +2,3 @@ fn outer() {"),
        "expected a function-context suffix, got:\n{out}"
    );
}

#[test]
fn a_missing_trailing_newline_is_marked() {
    // xdiff appends the marker itself (xutils.c), so a caller assembling a
    // patch gets git's exact wording without having to detect the case.
    let out = String::from_utf8(unified(b"a\n", b"a", 3).unwrap()).unwrap();
    assert_eq!(
        out, "@@ -1 +1 @@\n-a\n+a\n\\ No newline at end of file\n",
        "got:\n{out}"
    );
}

#[test]
fn a_large_input_round_trips() {
    // Exercises the allocator shim's realloc path, which only shows up once
    // xdiff grows its internal tables.
    let before: Vec<u8> = (0..5000)
        .flat_map(|i| format!("line {i}\n").into_bytes())
        .collect();
    let after: Vec<u8> = (0..5000)
        .flat_map(|i| {
            if i % 500 == 0 {
                format!("changed {i}\n").into_bytes()
            } else {
                format!("line {i}\n").into_bytes()
            }
        })
        .collect();
    let got = hunks(&before, &after).unwrap();
    assert_eq!(got.len(), 10);
}
