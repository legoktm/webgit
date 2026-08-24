//! The views: one module per page, over a shared set of building blocks.
//!
//! The blocks live here — [`refs_table`] and [`commits_table`] for the two
//! table shapes every listing is made of, [`time`] for how a timestamp is
//! shown, [`query`] for reading the repository into rows, and [`browser`] for
//! the few places the render path touches the DOM directly.

pub(crate) mod about;
pub(crate) mod blame;
pub(crate) mod blob;
pub(crate) mod commit;
pub(crate) mod listing;
pub(crate) mod log;
pub(crate) mod markdown;
pub(crate) mod readme;
pub(crate) mod refs_all;
pub(crate) mod refs_heads;
pub(crate) mod refs_tags;
pub(crate) mod snapshot;
pub(crate) mod summary;
pub(crate) mod tag;
pub(crate) mod tree;

mod browser;
mod commits_table;
mod query;
mod refs_table;
mod time;

#[cfg(test)]
pub(crate) mod fixtures;

pub(crate) use browser::{
    click_download, download_bytes, use_blob_url, use_object_url, yield_to_browser,
};
pub(crate) use commits_table::{CommitRow, ExpandMsg, commits_table, short_hash};
pub(crate) use gib_patch::is_binary;
pub(crate) use query::{
    apply_decorations, collect_refs, commit_for_entry, decoration_map, fetch_ref_rows,
    fetch_ref_rows_each, head_branch_name, mapped_ident, recent_commits, walk_commits_streamed,
};
pub(crate) use refs_table::{RefRow, branches_section, loading_dots, tags_section};
pub(crate) use time::format_datetime;
