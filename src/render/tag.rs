use crate::cache::CachingRepo;
use crate::error::GitContext;
use crate::render::format_datetime;
use git_async::reference::RefName;
use yew::prelude::*;

pub(crate) async fn build_tag(repo: &CachingRepo, tag: String) -> anyhow::Result<TagProps> {
    let ref_name = RefName::Ref(format!("tags/{tag}").into_bytes());
    let refs = repo.all_refs().await.context("list refs")?;
    let entry = refs
        .get(&ref_name)
        .ok_or_else(|| anyhow::anyhow!("tag not found: {tag}"))?;
    let object = repo
        .lookup_object(entry.target())
        .await
        .context(format!("lookup tag object {tag}"))?;
    let commit = repo
        .peel_to_commit(&object)
        .await
        .context(format!("peel ref for {tag}"))?
        .ok_or_else(|| anyhow::anyhow!("no commit for {tag}"))?;

    // Annotated (and signed) tags point at a tag object that carries its own
    // tagger and message. Lightweight tags point straight at the commit and
    // have no metadata of their own, so fall back to the commit's details.
    match object.tag() {
        Ok(tag_obj) => Ok(TagProps {
            name: tag.clone(),
            date: format_datetime(
                tag_obj
                    .date()
                    .ok_or_else(|| anyhow::anyhow!("no date on tag {tag}"))?,
            ),
            tagger_name: Some(
                String::from_utf8_lossy(
                    tag_obj
                        .tagger_name()
                        .ok_or_else(|| anyhow::anyhow!("no tagger on {tag}"))?,
                )
                .into_owned(),
            ),
            commit: commit.id().to_string(),
            contents: Some(String::from_utf8_lossy(tag_obj.message()).into_owned()),
        }),
        Err(_) => Ok(TagProps {
            name: tag,
            date: format_datetime(commit.author_date()),
            tagger_name: None,
            commit: commit.id().to_string(),
            contents: None,
        }),
    }
}

/// The view inputs for a single tag. These double as the component's props and
/// as the unit-test fixture, so the data-building (`build_tag`) and the markup
/// (`TagView`) can be exercised independently.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct TagProps {
    pub name: String,
    pub date: String,
    pub tagger_name: Option<String>,
    pub commit: String,
    pub contents: Option<String>,
}

/// The Yew component used to mount the tag view into the DOM. The markup lives
/// in the plain `tag_view` function below so it can be unit-tested without a
/// renderer (see the `VNode`-equality tests).
#[function_component(TagView)]
pub(crate) fn tag_view_component(props: &TagProps) -> Html {
    tag_view(props)
}

pub(crate) fn tag_view(props: &TagProps) -> Html {
    let TagProps {
        name,
        date,
        tagger_name,
        commit,
        contents,
    } = props;

    let commit_href = format!("#!/commit/{commit}");
    let encoded = crate::route::encode_component(name);
    let tree_href = format!("#!/tree?h={encoded}");
    let log_href = format!("#!/log?h={encoded}");

    html! {
        <>
            <table class="tag-table">
                <tbody>
                    <tr>
                        <td class="label">{ "tag name" }</td>
                        <td>{ name.clone() }</td>
                    </tr>
                    <tr>
                        <td class="label">{ "tag date" }</td>
                        <td>{ date.clone() }</td>
                    </tr>
                    if let Some(tagger) = tagger_name {
                        <tr>
                            <td class="label">{ "tagged by" }</td>
                            <td>{ tagger.clone() }</td>
                        </tr>
                    }
                    <tr>
                        <td class="label">{ "tagged object" }</td>
                        <td class="mono"><a href={commit_href}>{ commit.clone() }</a></td>
                    </tr>
                    <tr>
                        <td class="label">{ "browse" }</td>
                        <td>
                            <a href={tree_href}>{ "tree" }</a>
                            { " | " }
                            <a href={log_href}>{ "log" }</a>
                        </td>
                    </tr>
                </tbody>
            </table>
            if let Some(body) = contents {
                <pre class="tag-message">{ body.clone() }</pre>
            }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `TagView` to a static HTML string via SSR. Asserting on the
    /// rendered string (rather than comparing `VNode`s) sidesteps the vdom's
    /// representational quirks — data-derived attributes are stored as
    /// `Dynamic` while string literals are `Static`, so two trees that produce
    /// identical HTML are not `PartialEq`-equal.
    ///
    /// SSR emits everything on one line; we break adjacent tags onto their own
    /// lines purely so the snapshots read as a tree. It's line-breaks only (no
    /// depth indentation) so that `<pre>` bodies stay byte-exact — their text
    /// contains no `><`, so it is left untouched.
    fn render(props: TagProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<TagView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    #[test]
    fn tag_html_annotated() {
        // Annotated tags show the tagger row and the message body.
        insta::assert_snapshot!(render(TagProps {
            name: "v1.0.0".to_string(),
            date: "2026-01-15 12:34:56 +00:00".to_string(),
            tagger_name: Some("Kunal Mehta".to_string()),
            commit: "0123abcd0123abcd0123abcd0123abcd0123abcd".to_string(),
            contents: Some("Release 1.0.0\n\nSigned-off-by: Kunal Mehta".to_string()),
        }));
    }

    #[test]
    fn tag_html_lightweight() {
        // Lightweight tags carry no tagger and no message, so those are omitted.
        insta::assert_snapshot!(render(TagProps {
            name: "v0.9.0".to_string(),
            date: "2025-11-02 08:00:00 +00:00".to_string(),
            tagger_name: None,
            commit: "89abcdef89abcdef89abcdef89abcdef89abcdef".to_string(),
            contents: None,
        }));
    }
}
