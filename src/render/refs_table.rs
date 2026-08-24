//! The branch and tag listings: one table shape, filled either from a ref
//! walk that has finished or from one still streaming rows in.

use super::time::Age;
use crate::route::encode_component;
use yew::{Html, html};

#[derive(PartialEq, Clone)]
pub(crate) struct RefRow {
    pub(super) name: String,
    /// The ref's commit metadata; `None` while the commit is still being
    /// fetched. The summary lists the (name-sorted) ref names immediately and
    /// backfills these columns as each section's commits resolve.
    pub(super) meta: Option<RefMeta>,
}

#[derive(PartialEq, Clone)]
pub(super) struct RefMeta {
    pub(super) message: String,
    pub(super) author: String,
    pub(super) age: Age,
}

impl RefRow {
    /// A name-only row whose commit metadata hasn't loaded yet.
    pub(crate) fn pending(name: String) -> Self {
        RefRow { name, meta: None }
    }

    /// Recency sort key (used by the age-sorted refs pages, which never hold
    /// pending rows); a pending row sorts as most-recent.
    pub(crate) fn age_secs(&self) -> u64 {
        self.meta.as_ref().map_or(0, |m| m.age.secs())
    }
}

/// The "Branches" section (the old `refs_heads.html`): a heading plus a table
/// of branch rows, with an optional "more" link to the full branch list. Lives
/// here, next to [`RefRow`], so the refs and summary views can share it.
pub(crate) fn branches_section(branches: &[RefRow], more: bool) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Branches" }</h3>
            { refs_table("Branch", None, html! {
                <>
                    { for branches.iter().map(|b| refs_table_row(format!("#!/tree?h={}", encode_component(&b.name)), b, None)) }
                    if more {
                        <tr><td>{ "[" }<a href="#!/refs/heads">{ "..." }</a>{ "]" }</td></tr>
                    }
                </>
            }) }
        </>
    }
}

/// The "Tags" section (the old `refs_tags.html`): a heading plus either a table
/// of tag rows (with an optional "more" link) or a "No tags." note.
pub(crate) fn tags_section(tags: &[RefRow], more: bool, repo_name: &str) -> Html {
    html! {
        <>
            <h3 class="summary-heading">{ "Tags" }</h3>
            if tags.is_empty() {
                <p class="msg">{ "No tags." }</p>
            } else {
                { refs_table("Tag", Some("Download"), html! {
                    <>
                        { for tags.iter().map(|t| refs_table_row(
                            format!("#!/refs/tags/{}", encode_component(&t.name)),
                            t,
                            Some(snapshot_cell(repo_name, &t.name)),
                        )) }
                        if more {
                            <tr><td>{ "[" }<a href="#!/refs/tags">{ "..." }</a>{ "]" }</td></tr>
                        }
                    </>
                }) }
            }
        </>
    }
}

/// The shared ref-table shell; `first_col` is the leading column header
/// ("Branch" or "Tag"), `extra_col` an optional header sitting just right of the
/// commit message (the tag tables' snapshot links), and `rows` the
/// already-rendered `<tbody>`.
fn refs_table(first_col: &'static str, extra_col: Option<&'static str>, rows: Html) -> Html {
    html! {
        <table class="summary-table">
            <thead>
                <tr>
                    <th>{ first_col }</th>
                    <th>{ "Commit message" }</th>
                    if let Some(extra) = extra_col {
                        <th>{ extra }</th>
                    }
                    <th>{ "Author" }</th>
                    <th>{ "Age" }</th>
                </tr>
            </thead>
            <tbody>{ rows }</tbody>
        </table>
    }
}

/// A minimalist, CSS-animated loading ellipsis shown in place of a value that
/// is still being fetched. The dots cycle via the `.loading-dots` stylesheet
/// rule, so no inline style or script is needed (CSP-safe).
pub(crate) fn loading_dots() -> Html {
    html! { <span class="loading-dots" aria-label="Loading"></span> }
}

/// One ref row. The snapshot cell sits between the message and the author, and
/// is rendered whether or not the commit metadata has arrived — the archive
/// link depends only on the ref name, so there is nothing to wait for.
fn refs_table_row(href: String, r: &RefRow, extra: Option<Html>) -> Html {
    html! {
        <tr key={r.name.clone()}>
            <td class="name"><a href={href}>{ r.name.clone() }</a></td>
            <td class="msg">
                { match &r.meta {
                    Some(m) => html! { m.message.clone() },
                    None => loading_dots(),
                } }
            </td>
            if let Some(extra) = extra {
                <td class="snapshot">{ extra }</td>
            }
            <td class="author">
                { r.meta.as_ref().map(|m| m.author.clone()).unwrap_or_default() }
            </td>
            <td class="age">
                { r.meta.as_ref().map(|m| m.age.display()).unwrap_or_default() }
            </td>
        </tr>
    }
}

/// The archive link for a tag, labelled with the file it downloads.
fn snapshot_cell(repo_name: &str, tag: &str) -> Html {
    html! {
        <a class="snapshot-link" href={crate::route::snapshot_url(tag)}>
            { crate::render::snapshot::snapshot_file_name(repo_name, tag) }
        </a>
    }
}
