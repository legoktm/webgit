//! The `(patch)` link, and the `.patch` file behind it — built from the diff
//! the view is already holding rather than by re-reading the repository.

use super::{CommitProps, FileRow};
use crate::render::download_bytes;
use yew::prelude::*;

/// The `(patch)` link beside the commit hash, where cgit's commit view puts it.
pub(super) fn patch_link(props: &CommitProps) -> Html {
    if !props.complete {
        return Html::default();
    }
    let props = props.clone();
    let onclick = Callback::from(move |e: MouseEvent| {
        // There is no patch page to navigate to: the href only makes this a
        // link, and the download takes the place of following it.
        e.prevent_default();
        download_bytes(
            &format!("{}.patch", props.meta.hash),
            "text/plain",
            build_patch(&props).as_bytes(),
        );
    });
    html! {
        <>{ " (" }<a class="patch-link" href="#" {onclick}>{ "patch" }</a>{ ")" }</>
    }
}

/// The commit as a patch file, from the diff the view is already holding.
pub(super) fn build_patch(props: &CommitProps) -> String {
    gib_patch::format_patch(
        &props.meta,
        props.files.iter().filter_map(FileRow::for_patch),
        &format!("webgit {}", crate::render::about::COMMIT),
    )
}
