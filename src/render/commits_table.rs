//! The commit table shared by the log, the summary and the ref pages: one row
//! per commit, with the ref decorations that point at it.

use super::time::Age;
use crate::route::encode_component;
use gib::object::ObjectId;
use yew::{Html, classes, html};

#[derive(PartialEq, Clone)]
pub(crate) struct CommitRow {
    /// The full commit id, kept in its 20-byte form rather than as hex.
    pub(super) id: ObjectId,
    pub(super) short_hash: String,
    pub(super) message: String,
    /// Everything after the subject line, shown under the row when the log is
    /// expanded (`?showmsg=1`); empty for a single-line commit message.
    pub(super) body: String,
    pub(super) author: String,
    pub(super) age: Age,
    pub(super) refs: Vec<RefLabel>,
}

/// The log's Expand/Collapse control, as rendered in the "Message" header
#[derive(PartialEq, Clone)]
pub(crate) struct ExpandMsg {
    expanded: bool,
    toggle_url: String,
}

impl ExpandMsg {
    pub(crate) fn new(expanded: bool, toggle_url: String) -> Self {
        ExpandMsg {
            expanded,
            toggle_url,
        }
    }
}

/// A branch or tag decoration shown next to a commit, cgit-style.
#[derive(PartialEq, Clone)]
pub(crate) struct RefLabel {
    pub(super) name: String,
    pub(super) kind: RefLabelKind,
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum RefLabelKind {
    Branch,
    Tag,
}

/// The commit list shared by the log and summary views (the old
/// `commits.html`). Lives here, next to [`CommitRow`], so both callers can
/// reuse it and reach the row's private fields.
pub(crate) fn commits_table(commits: &[CommitRow], expand: Option<&ExpandMsg>) -> Html {
    let expanded = expand.is_some_and(|e| e.expanded);
    let class = if expanded {
        classes!("summary-table", "log-expanded")
    } else {
        classes!("summary-table")
    };
    html! {
        <table {class}>
            <thead>
                <tr>
                    <th>{ "Age" }</th>
                    <th>{ "Commit" }</th>
                    <th>{ "Message" }{ for expand.map(expand_toggle) }</th>
                    <th>{ "Author" }</th>
                </tr>
            </thead>
            <tbody>
                { for commits.iter().map(|c| commit_table_row(c, expanded)) }
            </tbody>
        </table>
    }
}

/// cgit's `(Expand)`/`(Collapse)` link, which just names the same log with
/// `?showmsg=1` flipped.
fn expand_toggle(e: &ExpandMsg) -> Html {
    let label = if e.expanded { "Collapse" } else { "Expand" };
    html! {
        <>{ " (" }<a href={e.toggle_url.clone()}>{ label }</a>{ ")" }</>
    }
}

fn commit_table_row(c: &CommitRow, expanded: bool) -> Html {
    let href = format!("#!/commit/{}", c.id);
    html! {
        <>
            <tr key={c.id.to_string()} class={classes!(expanded.then_some("logheader"))}>
                <td class="age">{ c.age.display() }</td>
                <td class="name"><a href={href}>{ c.short_hash.clone() }</a></td>
                <td class="msg">{ c.message.clone() }{ for c.refs.iter().map(ref_label) }</td>
                <td class="author">{ c.author.clone() }</td>
            </tr>
            if expanded && !c.body.is_empty() {
                <tr key={format!("{}-body", c.id)}>
                    <td/>
                    <td class="logmsg" colspan="3">{ c.body.clone() }</td>
                </tr>
            }
        </>
    }
}

/// The 8-character abbreviation of `id` displayed in commit tables. Rendered
/// once when the row is built, so the full hex form is never retained.
pub(crate) fn short_hash(id: ObjectId) -> String {
    format!("{id}")[..8].to_string()
}

/// A single decoration after the commit message. Each is preceded by a literal
/// space so consecutive labels (and the message) stay separated.
fn ref_label(r: &RefLabel) -> Html {
    match r.kind {
        RefLabelKind::Tag => {
            let href = format!("#!/refs/tags/{}", encode_component(&r.name));
            html! { <>{ " " }<a class="ref-label tag" href={href}>{ r.name.clone() }</a></> }
        }
        RefLabelKind::Branch => {
            let href = format!("#!/log?h={}", encode_component(&r.name));
            html! { <>{ " " }<a class="ref-label branch" href={href}>{ r.name.clone() }</a></> }
        }
    }
}
