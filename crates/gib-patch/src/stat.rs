//! The diffstat block: one row per file, the totals, and the summary lines
//! calling out creations, deletions and mode changes.
//!
//! The column arithmetic is git's, from `show_stats` in `diff.c` — a diffstat
//! that lays its bars out differently is immediately visible as not being
//! git's.

use crate::diff::FileDiff;

/// The width `git format-patch` lays its diffstat out in — the mail wrap
/// width, not the 80 columns `git diff --stat` uses on a terminal.
const STAT_WIDTH: usize = 72;
/// The columns the fixed punctuation around the three stat columns takes: the
/// leading space, ` | `, and the space before the bar.
const STAT_PADDING: usize = 6;

/// Render the diffstat block: one row per file, the totals, and the summary
/// lines that call out creations, deletions and mode changes.
///
/// The column arithmetic is git's, from `show_stats` in `diff.c`, because a
/// diffstat that lays its bars out differently is immediately visible as not
/// being git's. The name column takes what it needs until the line no longer
/// fits, at which point the bar is cut to three eighths of the width and the
/// name to whatever is left — and a name still too long has its front replaced
/// by `...`, trimmed back to a path separator.
pub(crate) fn diffstat(files: &[&FileDiff]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::new();

    let max_len = files
        .iter()
        .map(|f| f.path.chars().count())
        .max()
        .unwrap_or(0);
    // Only text files have a change count, so only they scale the bars; a
    // binary row prints its two sizes where the bar would go.
    let max_change = files
        .iter()
        .filter(|f| f.binary_sizes.is_none())
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(0);
    // The room "Bin XXX -> YYY bytes" needs, and the three columns "Bin"
    // itself takes, which is a floor for the number column.
    let bin_width = files
        .iter()
        .filter_map(|f| f.binary_sizes)
        .map(|(old, new)| 14 + decimal_width(old) + decimal_width(new))
        .max()
        .unwrap_or(0);
    let number_width = decimal_width(max_change).max(if bin_width > 0 { 3 } else { 0 });

    let mut name_width = max_len;
    let mut graph_width = if max_change + 4 > bin_width {
        max_change
    } else {
        bin_width - 4
    };
    if name_width + number_width + STAT_PADDING + graph_width > STAT_WIDTH {
        let graph_max = (STAT_WIDTH * 3 / 8).saturating_sub(number_width + STAT_PADDING);
        if graph_width > graph_max {
            graph_width = graph_max.max(6);
        }
        let name_max = STAT_WIDTH.saturating_sub(number_width + STAT_PADDING + graph_width);
        if name_width > name_max {
            name_width = name_max;
        } else {
            graph_width = STAT_WIDTH.saturating_sub(number_width + STAT_PADDING + name_width);
        }
    }

    let mut additions = 0;
    let mut deletions = 0;
    for file in files {
        let (name, padding) = fit_name(&file.path, name_width);
        if let Some((old_size, new_size)) = file.binary_sizes {
            out.push_str(&format!(
                " {name}{padding} | {:>number_width$} {old_size} -> {new_size} bytes\n",
                "Bin"
            ));
            continue;
        }
        additions += file.additions;
        deletions += file.deletions;
        let change = file.additions + file.deletions;
        let (adds, dels) = scale_bar(file.additions, file.deletions, graph_width, max_change);
        // A file with no counted changes — one that only moved mode — has no
        // bar, and git leaves off the space that would have preceded it.
        let separator = if change > 0 { " " } else { "" };
        out.push_str(&format!(
            " {name}{padding} | {change:>number_width$}{separator}{}{}\n",
            "+".repeat(adds),
            "-".repeat(dels)
        ));
    }

    // git names both counts when either is zero, so that a patch that changes
    // no lines still says so in full.
    out.push_str(&format!(" {} changed", plural(files.len(), "file")));
    if additions > 0 || deletions == 0 {
        out.push_str(&format!(", {}(+)", plural(additions, "insertion")));
    }
    if deletions > 0 || additions == 0 {
        out.push_str(&format!(", {}(-)", plural(deletions, "deletion")));
    }
    out.push('\n');

    for file in files {
        match (file.old, file.new) {
            (None, Some(new)) => {
                out.push_str(&format!(" create mode {} {}\n", new.mode(), file.path));
            }
            (Some(old), None) => {
                out.push_str(&format!(" delete mode {} {}\n", old.mode(), file.path));
            }
            (Some(old), Some(new)) if old.mode() != new.mode() => {
                out.push_str(&format!(
                    " mode change {} => {} {}\n",
                    old.mode(),
                    new.mode(),
                    file.path
                ));
            }
            _ => {}
        }
    }

    out.push('\n');
    out
}

/// A path fitted to the name column: the name to print, and the padding that
/// follows it. A path too long has its front replaced by `...`, cut back to a
/// path separator so what remains starts at a directory boundary.
fn fit_name(path: &str, name_width: usize) -> (String, String) {
    let len = path.chars().count();
    if len <= name_width {
        return (path.to_string(), " ".repeat(name_width - len));
    }
    let room = name_width.saturating_sub(3);
    let mut tail: String = path.chars().skip(len - room).collect();
    if let Some(slash) = tail.find('/') {
        tail = tail[slash..].to_string();
    }
    let padding = " ".repeat(room.saturating_sub(tail.chars().count()));
    (format!("...{tail}"), padding)
}

/// Scale a file's two counts into the bar's width, git's way: the total is
/// scaled first and split between the sides, so that a file with both an
/// addition and a deletion always shows one of each.
fn scale_bar(
    additions: usize,
    deletions: usize,
    graph_width: usize,
    max_change: usize,
) -> (usize, usize) {
    if graph_width > max_change {
        return (additions, deletions);
    }
    let mut total = scale_linear(additions + deletions, graph_width, max_change);
    if total < 2 && additions > 0 && deletions > 0 {
        total = 2;
    }
    if additions < deletions {
        let adds = scale_linear(additions, graph_width, max_change);
        (adds, total - adds)
    } else {
        let dels = scale_linear(deletions, graph_width, max_change);
        (total - dels, dels)
    }
}

/// Scale one count into the space available. A file with any change at all
/// keeps at least one column, so a one-line change never disappears next to a
/// thousand-line one.
fn scale_linear(count: usize, graph_width: usize, max_change: usize) -> usize {
    if count == 0 {
        return 0;
    }
    1 + (count * (graph_width - 1) / max_change)
}

/// How many columns a number takes when written out.
fn decimal_width(value: usize) -> usize {
    value.to_string().len()
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}
