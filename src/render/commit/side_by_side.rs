//! The diff body, inline or in two columns.
//!
//! Two columns means pairing each run of removals with the run of additions
//! that replaced it, and blanking whichever side runs out first.

use super::{FileRow, diff_line};
use gib_patch::{LineKind, PatchLine};
use yew::prelude::*;

/// The diff under the diffstat, in whichever layout the reader asked for.
pub(super) fn diff_body(files: &[FileRow], side_by_side: bool) -> Html {
    let lines = || {
        files
            .iter()
            .filter_map(|f| f.diff.as_ref())
            .flat_map(|d| d.lines.iter())
    };
    if side_by_side {
        html! {
            <table class="diff-ss">
                <tbody>{ for side_rows(lines()).into_iter().map(side_row) }</tbody>
            </table>
        }
    } else {
        html! { <pre class="diff-pre">{ for lines().map(diff_line) }</pre> }
    }
}

/// One row of a two-column diff.
pub(super) enum SideRow<'a> {
    /// A line belonging to neither side on its own — a `diff --git` header, the
    /// block under it, or a `@@` marker — laid across both columns.
    Span(&'a PatchLine),
    /// An unchanged line, which both columns show.
    Both(&'a str),
    /// A change. Either side is absent where the two runs are uneven: three
    /// lines replaced by one leaves two rows with nothing on the right.
    Change {
        del: Option<&'a str>,
        ins: Option<&'a str>,
    },
}

/// Fold a unified diff's lines into two-column rows.
///
/// A hunk arrives as a run of removed lines followed by a run of added ones, so
/// the two runs are collected and then zipped: the *n*th removal sits opposite
/// the *n*th addition, and the longer run's tail gets blank cells. This is the
/// pairing cgit's `ssdiff.c` makes, and it is a presentational guess rather
/// than a claim about the edit — xdiff never said line 3 became line 3.
pub(super) fn side_rows<'a>(lines: impl Iterator<Item = &'a PatchLine>) -> Vec<SideRow<'a>> {
    let mut rows = Vec::new();
    let mut dels: Vec<&str> = Vec::new();
    let mut inss: Vec<&str> = Vec::new();

    // `text` carries the `+`/`-`/space marker a unified diff needs; the column
    // a cell lands in says the same thing, so it comes off here.
    fn body(line: &PatchLine) -> &str {
        line.text.get(1..).unwrap_or("")
    }

    fn flush<'a>(rows: &mut Vec<SideRow<'a>>, dels: &mut Vec<&'a str>, inss: &mut Vec<&'a str>) {
        for i in 0..dels.len().max(inss.len()) {
            rows.push(SideRow::Change {
                del: dels.get(i).copied(),
                ins: inss.get(i).copied(),
            });
        }
        dels.clear();
        inss.clear();
    }

    for line in lines {
        match line.kind {
            LineKind::Delete if line.text.starts_with('-') && !line.text.starts_with("---") => {
                dels.push(body(line));
            }
            LineKind::Insert if line.text.starts_with('+') && !line.text.starts_with("+++") => {
                inss.push(body(line));
            }
            // A context line closes the pairing before it, and so does every
            // header: a run never spans a hunk boundary.
            LineKind::Context if line.text.starts_with(' ') => {
                flush(&mut rows, &mut dels, &mut inss);
                rows.push(SideRow::Both(body(line)));
            }
            _ => {
                flush(&mut rows, &mut dels, &mut inss);
                rows.push(SideRow::Span(line));
            }
        }
    }
    flush(&mut rows, &mut dels, &mut inss);
    rows
}

fn side_row(row: SideRow<'_>) -> Html {
    match row {
        SideRow::Span(line) => {
            let class = match line.kind {
                LineKind::Meta => "diff-hunk",
                LineKind::Insert => "diff-add",
                LineKind::Delete => "diff-del",
                LineKind::Context => "diff-ctx",
            };
            html! {
                <tr><td class={classes!("diff-ss-span", class)} colspan="2">
                    { line.text.clone() }
                </td></tr>
            }
        }
        SideRow::Both(text) => html! {
            <tr>
                <td class="diff-ctx">{ text.to_string() }</td>
                <td class="diff-ctx">{ text.to_string() }</td>
            </tr>
        },
        SideRow::Change { del, ins } => html! {
            <tr>
                { side_cell(del, "diff-del") }
                { side_cell(ins, "diff-add") }
            </tr>
        },
    }
}

/// One half of a changed row. An absent side is a filled-in blank rather than
/// an empty cell, so the eye can see that there is nothing there to compare.
fn side_cell(text: Option<&str>, class: &'static str) -> Html {
    match text {
        Some(text) => html! { <td class={class}>{ text.to_string() }</td> },
        None => html! { <td class="diff-ss-none"></td> },
    }
}
