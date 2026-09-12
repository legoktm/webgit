//! Line selection, shared by the two views that show a file line by line.

use crate::route::LineRange;
use wasm_bindgen::JsCast;
use yew::prelude::*;

/// Bring the selected lines into view once they are on screen.
#[hook]
pub(crate) fn use_selection_scroll(start: Option<usize>) {
    use_effect_with(start, |start| {
        if let Some(start) = *start
            && let Some(document) = web_sys::window().and_then(|window| window.document())
            && let Some(target) = document.get_element_by_id(&format!("n{start}"))
        {
            target.scroll_into_view();
        }
        || ()
    });
}

/// The click handler shared by every line number in a gutter.
pub(crate) fn line_click_handler(self_url: &str, lines: Option<LineRange>) -> Callback<MouseEvent> {
    let self_url = self_url.to_string();
    Callback::from(move |event: MouseEvent| {
        if !event.shift_key() {
            return;
        }
        let Some(n) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.get_attribute("data-n"))
            .and_then(|n| n.parse::<usize>().ok())
        else {
            return;
        };
        // Extend from the anchor the reader last set, not from whichever end of
        // the range is nearer: shift-clicking twice should be able to shrink a
        // range as well as grow it.
        let range = match lines {
            Some(lines) => LineRange::spanning(lines.start, n),
            None => LineRange::single(n),
        };
        event.prevent_default();
        if let Some(window) = web_sys::window() {
            let _ = window
                .location()
                .set_hash(&format!("{self_url}{}", range.anchor()));
        }
    })
}

/// `url` with the current selection's anchor on the end, for the link that
/// crosses between a file's two views.
pub(crate) fn anchored(url: &str, lines: Option<LineRange>) -> String {
    match lines {
        Some(lines) => format!("{url}{}", lines.anchor()),
        None => url.to_string(),
    }
}
