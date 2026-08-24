//! Unit tests for the pieces that have no repository in them: the chunk walk
//! that moves line runs between a commit and its parent, and the list merge
//! that puts two batches of runs back in order.
//!
//! The walk over real history is covered by `differential.rs`, against
//! `git blame` itself.

use super::{Entry, OriginId, blame_chunk, blame_merge, count_lines};
use std::collections::VecDeque;

/// A run of `num_lines` lines sitting at the same place in both files.
fn entry(lno: usize, num_lines: usize, suspect: OriginId) -> Entry {
    Entry {
        lno,
        num_lines,
        s_lno: lno,
        suspect,
    }
}

/// `(lno, num_lines, s_lno, suspect)` for each entry, which is the whole of
/// what a chunk walk is allowed to change.
fn shape(entries: &[Entry]) -> Vec<(usize, usize, usize, OriginId)> {
    entries
        .iter()
        .map(|e| (e.lno, e.num_lines, e.s_lno, e.suspect))
        .collect()
}

/// Drive one diff's worth of chunks the way `pass_blame_to_parent` does:
/// a chunk per hunk, then the closing chunk over the common tail.
fn walk(
    suspects: Vec<Entry>,
    hunks: &[(usize, usize, usize, usize)],
    parent: OriginId,
) -> (Vec<Entry>, Vec<Entry>) {
    let mut src: VecDeque<Entry> = suspects.into();
    let (mut passed, mut kept) = (Vec::new(), Vec::new());
    let mut offset: isize = 0;
    for &(start_a, count_a, start_b, count_b) in hunks {
        blame_chunk(
            &mut passed,
            &mut src,
            &mut kept,
            start_b,
            start_a as isize - start_b as isize,
            start_b + count_b,
            parent,
        );
        offset = (start_a + count_a) as isize - (start_b + count_b) as isize;
    }
    blame_chunk(
        &mut passed,
        &mut src,
        &mut kept,
        usize::MAX,
        offset,
        usize::MAX,
        parent,
    );
    (passed, kept)
}

/// A file the commit did not touch at all: no hunks, so every line goes to the
/// parent unmoved.
#[test]
fn test_unchanged_file_passes_everything() {
    let (passed, kept) = walk(vec![entry(0, 10, 0)], &[], 1);
    assert_eq!(shape(&passed), vec![(0, 10, 0, 1)]);
    assert!(kept.is_empty());
}

/// One line replaced in the middle: the line itself is this commit's, and the
/// runs on either side of it are the parent's, still at their own positions.
#[test]
fn test_replacement_splits_around_the_hunk() {
    // Parent line 4 became target line 4; everything else is common.
    let (passed, kept) = walk(vec![entry(0, 10, 0)], &[(4, 1, 4, 1)], 1);
    assert_eq!(shape(&passed), vec![(0, 4, 0, 1), (5, 5, 5, 1)]);
    assert_eq!(shape(&kept), vec![(4, 1, 4, 0)]);
}

/// An insertion pushes everything after it down, so the lines below the hunk
/// sit two lines earlier in the parent than they do here.
#[test]
fn test_insertion_rebases_the_lines_below_it() {
    // Two lines inserted at target line 3; the parent has none there.
    let (passed, kept) = walk(vec![entry(0, 10, 0)], &[(3, 0, 3, 2)], 1);
    assert_eq!(shape(&passed), vec![(0, 3, 0, 1), (5, 5, 3, 1)]);
    assert_eq!(shape(&kept), vec![(3, 2, 3, 0)]);
}

/// A deletion has no lines on this side to blame anyone for, and moves the
/// lines below it *up* in this file relative to the parent.
#[test]
fn test_deletion_keeps_nothing_and_shifts_the_rest() {
    // Parent lines 3..5 are gone; target line 3 is parent line 5.
    let (passed, kept) = walk(vec![entry(0, 8, 0)], &[(3, 2, 3, 0)], 1);
    assert_eq!(shape(&passed), vec![(0, 3, 0, 1), (3, 5, 5, 1)]);
    assert!(kept.is_empty());
}

/// Several hunks in one diff, with the offset between the two files changing
/// as the walk crosses each of them.
#[test]
fn test_offset_accumulates_across_hunks() {
    // Insert one line at 2, then delete two parent lines at 8 (target 9).
    let (passed, kept) = walk(vec![entry(0, 12, 0)], &[(2, 0, 2, 1), (8, 2, 9, 0)], 1);
    assert_eq!(
        shape(&passed),
        vec![(0, 2, 0, 1), (3, 6, 2, 1), (9, 3, 10, 1)]
    );
    assert_eq!(shape(&kept), vec![(2, 1, 2, 0)]);
}

/// A hunk that falls inside a run splits it in three, and the outer two parts
/// keep the line numbers they had — this is the case a blame gets visibly
/// wrong if the split arithmetic is off by one.
#[test]
fn test_run_straddling_both_hunk_boundaries() {
    let (passed, kept) = walk(vec![entry(10, 6, 0)], &[(12, 1, 12, 1)], 1);
    assert_eq!(shape(&passed), vec![(10, 2, 10, 1), (13, 3, 13, 1)]);
    assert_eq!(shape(&kept), vec![(12, 1, 12, 0)]);
}

/// Runs already blamed on different commits keep their own suspects; only the
/// ones handed to the parent change hands.
#[test]
fn test_several_runs_keep_their_own_suspects() {
    let suspects = vec![entry(0, 3, 7), entry(3, 3, 8)];
    let (passed, kept) = walk(suspects, &[(4, 1, 4, 1)], 1);
    assert_eq!(
        shape(&passed),
        vec![(0, 3, 0, 1), (3, 1, 3, 1), (5, 1, 5, 1)]
    );
    assert_eq!(shape(&kept), vec![(4, 1, 4, 8)]);
}

/// The merge is by `s_lno`, and where two runs start on the same line the
/// first list's comes first — which is what keeps a parent's existing lines
/// ahead of a batch that just arrived.
#[test]
fn test_blame_merge_orders_by_s_lno() {
    let a = vec![entry(0, 1, 1), entry(4, 1, 2)];
    let b = vec![entry(0, 1, 3), entry(2, 1, 4), entry(9, 1, 5)];
    let merged = blame_merge(a, b);
    assert_eq!(
        merged.iter().map(|e| e.suspect).collect::<Vec<_>>(),
        vec![1, 3, 4, 2, 5]
    );
}

#[test]
fn test_blame_merge_with_an_empty_side() {
    let a = vec![entry(0, 1, 1)];
    assert_eq!(shape(&blame_merge(a.clone(), Vec::new())), shape(&a));
    assert_eq!(shape(&blame_merge(Vec::new(), a.clone())), shape(&a));
}

/// git counts a final line with no newline after it, and does not count an
/// empty one after a trailing newline.
#[test]
fn test_count_lines_matches_gits_counting() {
    assert_eq!(count_lines(b""), 0);
    assert_eq!(count_lines(b"a\n"), 1);
    assert_eq!(count_lines(b"a"), 1);
    assert_eq!(count_lines(b"a\nb"), 2);
    assert_eq!(count_lines(b"a\nb\n"), 2);
    assert_eq!(count_lines(b"\n"), 1);
}
