//! cgit's "diff options" panel, as links rather than a form.
//!
//! cgit submits a `<select>` on change; the CSP here forbids that outright
//! (`form-action 'none'`), so every setting is instead an anchor to the URL
//! that view has. That is the better shape for a hash-routed app anyway: each
//! state is addressable, the back button walks the settings, and a reader can
//! copy the link to the view they are looking at.

use crate::route::{CONTEXT_CHOICES, DiffMode, DiffView, commit_url};
use yew::prelude::*;

/// cgit's "diff options" panel, as links rather than a form.
///
/// cgit submits a `<select>` on change; the CSP here forbids that outright
/// (`form-action 'none'`), so every setting is instead an anchor to the URL
/// that view has. That is the better shape for a hash-routed app anyway: each
/// state is addressable, the back button walks the settings, and a reader can
/// copy the link to the view they are looking at.
pub(super) fn diff_controls(url_sha: &str, view: DiffView) -> Html {
    let url = |v: DiffView| commit_url(url_sha, v);
    let context = view.context_lines();

    // The ladder is cgit's (1..10, then fives); stepping walks it rather than
    // adding one, so the wide end is a few clicks away instead of thirty.
    let step = |delta: isize| -> Option<DiffView> {
        let at = CONTEXT_CHOICES.iter().position(|&n| n >= context)?;
        let to = CONTEXT_CHOICES.get(at.checked_add_signed(delta)?)?;
        Some(DiffView {
            context: Some(*to),
            ..view
        })
    };

    html! {
        <div class="diff-opts">
            <b class="diff-opts-title">{ "diff options" }</b>

            <div class="diff-opts-row">
                <span class="diff-opts-label">{ "context" }</span>
                <span class="seg">
                    { seg_step("−", step(-1).map(url)) }
                    <span class="seg-num">{ context }</span>
                    { seg_step("+", step(1).map(url)) }
                </span>
            </div>

            <div class="diff-opts-row">
                <span class="diff-opts-label">{ "space" }</span>
                <span class="seg">
                    { seg("include", !view.ignore_whitespace, Some(url(DiffView { ignore_whitespace: false, ..view }))) }
                    { seg("ignore", view.ignore_whitespace, Some(url(DiffView { ignore_whitespace: true, ..view }))) }
                </span>
            </div>

            <div class="diff-opts-row">
                <span class="diff-opts-label">{ "mode" }</span>
                <span class="seg">
                    { seg("unified", view.shows_diff(), Some(url(DiffView { mode: DiffMode::Unified, ..view }))) }
                    { seg("stat only", !view.shows_diff(), Some(url(DiffView { mode: DiffMode::StatOnly, ..view }))) }
                </span>
            </div>

            <div class="diff-opts-row">
                <span class="diff-opts-label">{ "layout" }</span>
                <span class="seg">
                    // With the diff hidden there is no layout to choose, so
                    // both sides go dead rather than offering a setting that
                    // would change nothing on the page.
                    { seg("inline", view.shows_diff() && !view.side_by_side,
                          view.shows_diff().then(|| url(DiffView { side_by_side: false, ..view }))) }
                    { seg("side by side", view.shows_diff() && view.side_by_side,
                          view.shows_diff().then(|| url(DiffView { side_by_side: true, ..view }))) }
                </span>
            </div>

            <div class="diff-opts-reset">
                if view == DiffView::default() {
                    <span class="diff-opts-dim">{ "reset to defaults" }</span>
                } else {
                    <a href={url(DiffView::default())}>{ "reset to defaults" }</a>
                }
            </div>
        </div>
    }
}

/// One segment of a control. The current setting is not a link — it is where
/// the reader already is — and neither is one that would do nothing.
fn seg(label: &'static str, active: bool, href: Option<String>) -> Html {
    match (active, href) {
        (true, _) => html! { <span class="seg-btn on">{ label }</span> },
        (false, Some(href)) => html! { <a class="seg-btn" {href}>{ label }</a> },
        (false, None) => html! { <span class="seg-btn off">{ label }</span> },
    }
}

/// A `−`/`+` end of the context stepper, dead at the end of the ladder.
fn seg_step(label: &'static str, href: Option<String>) -> Html {
    match href {
        Some(href) => html! { <a class="seg-btn" {href}>{ label }</a> },
        None => html! { <span class="seg-btn off">{ label }</span> },
    }
}
