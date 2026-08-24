//! Hand-built rows for the view tests, so a snapshot never depends on a real
//! repository — or on the wall clock, which `Age` would otherwise read.

use super::commits_table::{CommitRow, RefLabel, RefLabelKind};
use super::refs_table::{RefMeta, RefRow};
use super::time::Age;
use gib::object::ObjectId;

/// An [`Age`] that renders as a relative bucket; `secs` must be under the
/// two-week cutoff for the (placeholder) date to stay hidden.
pub(crate) fn relative_age(secs: u64) -> Age {
    Age {
        secs,
        when: ymd("2000-01-01"),
    }
}

/// An [`Age`] old enough to render as the given absolute `YYYY-MM-DD` date.
pub(crate) fn date_age(date: &str) -> Age {
    Age {
        secs: 365 * 86400,
        when: ymd(date),
    }
}

fn ymd(date: &str) -> jiff::civil::Date {
    date.parse().unwrap()
}

pub(crate) fn ref_row(name: &str, message: &str, author: &str, age: Age) -> RefRow {
    RefRow {
        name: name.to_string(),
        meta: Some(RefMeta {
            message: message.to_string(),
            author: author.to_string(),
            age,
        }),
    }
}

pub(crate) fn commit_row(short_hash: &str, message: &str, author: &str, age: Age) -> CommitRow {
    // Zero-pad the abbreviation out to a full id, so the row's link renders
    // the same 40 hex characters a real walk would produce.
    let hex = format!("{short_hash}{}", "0".repeat(40 - short_hash.len()));
    CommitRow {
        id: ObjectId::from_hex(hex.as_bytes()).expect("fixture id must be 40 hex characters"),
        short_hash: short_hash.to_string(),
        message: message.to_string(),
        body: String::new(),
        author: author.to_string(),
        age,
        refs: Vec::new(),
    }
}

/// A row whose commit message has a body, which the expanded log
/// (`?showmsg=1`) renders under the subject.
pub(crate) fn commit_row_with_body(
    short_hash: &str,
    message: &str,
    body: &str,
    author: &str,
    age: Age,
) -> CommitRow {
    let mut row = commit_row(short_hash, message, author, age);
    row.body = body.to_string();
    row
}

pub(crate) fn decorated_commit_row(
    short_hash: &str,
    message: &str,
    author: &str,
    age: Age,
    branches: &[&str],
    tags: &[&str],
) -> CommitRow {
    let mut row = commit_row(short_hash, message, author, age);
    for name in branches {
        row.refs.push(RefLabel {
            name: name.to_string(),
            kind: RefLabelKind::Branch,
        });
    }
    for name in tags {
        row.refs.push(RefLabel {
            name: name.to_string(),
            kind: RefLabelKind::Tag,
        });
    }
    row
}
