use crate::cache::CachingRepo;
use crate::render::blob::{MAX_BLOB_BYTES, MAX_BLOB_LINES, count_lines, split_lines};
use crate::render::{
    anchored, commit_first_line, format_datetime, is_binary, line_click_handler, mapped_ident,
    short_hash, use_selection_scroll, yield_to_browser,
};
use crate::route::{LineRange, blame_url, tree_url};
use gib::object::{Commit, ObjectId};
use gib_blame::{BlameError, BlameGroup};
use gib_mailmap::Mailmap;
use std::collections::{BTreeMap, BTreeSet};
use yew::prelude::*;

/// One run of lines the walk has attributed, as the gutter shows it.
///
/// Built once when the run is settled, so the per-line markup below stays a
/// lookup rather than a format call per row: a blame of a 20 000-line file
/// renders every line, and the runs are what few of them there are.
#[derive(PartialEq, Clone)]
pub(crate) struct BlameRun {
    /// First line of the run, zero-based, matching the index into the file's
    /// lines.
    pub start: usize,
    pub num_lines: usize,
    /// The run's commit, kept in its 20-byte form rather than as hex: it is
    /// what [`fill_details`] fetches and keys its results by.
    pub commit: ObjectId,
    pub short_hash: String,
    /// The full hash, shown on hover — the gutter only has room for eight
    /// characters, and this is the one a reader copies out.
    pub full_hash: String,
    pub commit_url: String,
    /// Blame the same file one revision further back, cgit's `^`. Absent on a
    /// root commit, which has no previous revision to look at.
    pub prev_url: Option<String>,
    /// The gutter's tooltip, cgit's `emit_suspect_detail`. `None` until
    /// [`fill_details`] reads the commit; until then the full hash shows.
    pub detail: Option<String>,
}

/// What the blame view has to show for the file.
#[derive(PartialEq, Clone)]
pub(crate) enum BlameContent {
    /// The file's lines, with whatever has been attributed so far. A line's
    /// 1-based number is its index here plus one.
    Lines(Vec<String>),
    /// A file git's heuristic calls binary, with its size in bytes.
    Binary { bytes: usize },
    /// A file over [`MAX_BLOB_BYTES`], with its size in bytes.
    TooManyBytes { bytes: usize },
    /// A file over [`MAX_BLOB_LINES`], with its line count.
    TooManyLines { lines: usize },
}

/// The view inputs for a blame. Doubles as the component's props and the test
/// fixture.
#[derive(Properties, PartialEq, Clone)]
pub(crate) struct BlameProps {
    pub blob_id: String,
    pub content: BlameContent,
    /// The runs settled so far, in file order. Grows as the walk reports more;
    /// a line no run covers yet renders with an empty gutter rather than
    /// waiting, which is what lets the file itself paint immediately.
    pub runs: Vec<BlameRun>,
    /// Whether the walk is still going, so the view can say so.
    pub pending: bool,
    /// This blame's own URL, which every line number's anchor is built on.
    pub self_url: String,
    /// The plain blob view of the same file.
    pub source_url: String,
    /// The lines the URL's `#n…` anchor selected, highlighted in the gutter and
    /// scrolled to on arrival, exactly as in the blob view.
    pub lines: Option<LineRange>,
}

pub(crate) struct BlameTarget<'a> {
    /// The revision being blamed from: where the walk starts.
    pub commit: &'a Commit,
    /// The file's path within that revision's tree.
    pub path: &'a str,
    /// The `?h=` the file was reached by. Together with `path` it addresses
    /// every link the gutter writes.
    pub head: Option<&'a str>,
    pub blob_id: ObjectId,
    /// The file's bytes. Resolved by the caller rather than by the engine,
    /// which is what lets the file paint before the walk starts.
    pub data: Vec<u8>,
}

/// Build the blame view's props, streaming partial results through
/// `on_partial` as the walk settles more of the file and then as the hover
/// details arrive behind it.
///
/// The file's own bytes are rendered first and the gutter fills in behind
/// them. Blame is the most object-hungry view in the app — every commit that
/// touched the file costs a blob, fetched one revision after another — so
/// waiting for the oldest commit before showing anything would leave the
/// reader looking at nothing for as long as the file's history is deep.
pub(crate) async fn build_blame(
    repo: &CachingRepo,
    mailmap: &Mailmap,
    target: BlameTarget<'_>,
    on_partial: impl Fn(BlameProps),
) -> Result<BlameProps, BlameError> {
    let BlameTarget {
        commit,
        path,
        head,
        blob_id,
        data,
    } = target;
    let mut props = BlameProps {
        blob_id: blob_id.to_string(),
        content: blame_content(&data),
        runs: Vec::new(),
        pending: false,
        self_url: blame_url(path, head),
        source_url: tree_url(path, head, false),
        lines: None,
    };
    // A file with no lines to attribute — binary, or past the caps — is the
    // whole answer already; there is nothing for the walk to do.
    if !matches!(props.content, BlameContent::Lines(_)) {
        return Ok(props);
    }

    props.pending = true;
    on_partial(props.clone());
    let on_partial = &on_partial;
    let blame = gib_blame::blame(commit, repo, path, |groups| {
        let mut partial = props.clone();
        partial.runs = runs(groups);
        async move {
            on_partial(partial);
            // Blame is a serial walk over the file's whole history, and a
            // cached object resolves without ever handing the event loop back.
            // Without this the gutter would land in one go at the end, which is
            // the wait this streaming exists to avoid.
            yield_to_browser().await;
        }
    })
    .await?;

    crate::console_log(&format!(
        "webgit: blame [{path}]: {}, {} lines in {} runs",
        blame.stats,
        blame.num_lines,
        blame.groups.len(),
    ));

    props.runs = runs(&blame.groups);
    props.pending = false;
    on_partial(props.clone());

    fill_details(repo, mailmap, &mut props, on_partial).await;
    Ok(props)
}

/// How many commit objects to read (concurrently) before showing the tooltips
/// that have landed. Mirrors [`gib_log`]'s streamed walk, for the same reason.
const DETAIL_BATCH: usize = 10;

/// Fill in each run's hover detail, streaming partials as the batches land.
async fn fill_details(
    repo: &CachingRepo,
    mailmap: &Mailmap,
    props: &mut BlameProps,
    on_partial: impl Fn(BlameProps),
) {
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let wanted: Vec<ObjectId> = props
        .runs
        .iter()
        .map(|run| run.commit)
        .filter(|&id| seen.insert(id))
        .collect();

    let mut details: BTreeMap<ObjectId, String> = BTreeMap::new();
    for chunk in wanted.chunks(DETAIL_BATCH) {
        let objects =
            futures::future::join_all(chunk.iter().map(|&id| repo.lookup_object(id))).await;
        for (&id, object) in chunk.iter().zip(objects) {
            // A commit that can't be read leaves its runs on the hash-only
            // tooltip; a partial repository shouldn't fail over a tooltip.
            if let Some(commit) = object.ok().and_then(|object| object.commit().ok()) {
                details.insert(id, detail(&commit, mailmap));
            }
        }
        for run in &mut props.runs {
            if run.detail.is_none()
                && let Some(text) = details.get(&run.commit)
            {
                run.detail = Some(text.clone());
            }
        }
        on_partial(props.clone());
        // As in the walk above: cached objects resolve without handing the
        // event loop back, so the tooltips would all land at once at the end.
        yield_to_browser().await;
    }
}

/// One commit's tooltip: cgit's `emit_suspect_detail`, kept behind the full
/// hash because that is what this tooltip showed before.
fn detail(commit: &Commit, mailmap: &Mailmap) -> String {
    let (author_name, author_email) =
        mapped_ident(commit.author_name(), commit.author_email(), mailmap);
    let (committer_name, committer_email) =
        mapped_ident(commit.committer_name(), commit.committer_email(), mailmap);
    format!(
        "{}\nauthor  {author_name} <{author_email}> {}\ncommitter  {committer_name} \
         <{committer_email}> {}\n\n{}",
        commit.id(),
        format_datetime(commit.author_date()),
        format_datetime(commit.commit_date()),
        commit_first_line(commit.message()),
    )
}

/// Classify the file the same way the blob view classifies one, minus the
/// forms blame has nothing to say about: an image or a rendered markdown
/// document has no lines to attribute, so both arrive here as what they are
/// underneath — bytes, either textual or binary.
fn blame_content(data: &[u8]) -> BlameContent {
    if is_binary(data) {
        return BlameContent::Binary { bytes: data.len() };
    }
    if data.len() > MAX_BLOB_BYTES {
        return BlameContent::TooManyBytes { bytes: data.len() };
    }
    let lines = count_lines(data);
    if lines > MAX_BLOB_LINES {
        return BlameContent::TooManyLines { lines };
    }
    BlameContent::Lines(split_lines(&String::from_utf8_lossy(data)))
}

/// Turn the engine's groups into the gutter's runs.
fn runs(groups: &[BlameGroup]) -> Vec<BlameRun> {
    groups
        .iter()
        .map(|group| BlameRun {
            start: group.start,
            num_lines: group.num_lines,
            commit: group.commit,
            short_hash: short_hash(group.commit),
            full_hash: group.commit.to_string(),
            commit_url: format!("#!/commit/{}", group.commit),
            // The previous revision is blamed at the path the run's own commit
            // had for the file, named by hash so the link is stable whatever
            // ref the reader arrived by.
            prev_url: group
                .parent
                .map(|parent| blame_url(&group.path, Some(&parent.to_string()))),
            detail: None,
        })
        .collect()
}

/// The Yew component used to mount the blame view into the DOM.
#[function_component(BlameView)]
pub(crate) fn blame_view_component(props: &BlameProps) -> Html {
    use_selection_scroll(props.lines.map(|lines| lines.start));
    blame_view(props)
}

/// The blame view's markup: cgit's blame table, one row per line, with each
/// run's commit spanning its lines in the gutter.
///
/// cgit builds four parallel columns of `<div>`s — hashes, line numbers,
/// shading bars, and the file as one `<pre>` — which keeps the bars aligned
/// only as long as every column agrees about line height. A table row per line
/// with the gutter cell spanning its run says the same thing structurally, and
/// the alternating shading follows the runs rather than being a column of its
/// own.
pub(crate) fn blame_view(props: &BlameProps) -> Html {
    let BlameProps {
        blob_id,
        content,
        runs,
        pending,
        self_url,
        source_url,
        lines: selected,
    } = props;
    let on_line_click = line_click_handler(self_url, *selected);

    html! {
        <>
            <div class="blob-info">
                { "blob: " }{ blob_id }
                { " · " }
                <a class="blob-alt-view" href={anchored(source_url, *selected)}>{ "source" }</a>
                if *pending {
                    { " · " }<span class="blame-pending">{ "blaming\u{2026}" }</span>
                }
            </div>
            { match content {
                BlameContent::Lines(lines) => blame_table(BlameTable {
                    lines,
                    runs,
                    self_url,
                    selected: *selected,
                    on_click: &on_line_click,
                }),
                BlameContent::Binary { bytes } => html! {
                    <p class="msg">{ format!("Binary file ({bytes} bytes).") }</p>
                },
                BlameContent::TooManyBytes { bytes } => html! {
                    <p class="msg">{
                        format!("File too large to blame ({bytes} bytes, limit {MAX_BLOB_BYTES}).")
                    }</p>
                },
                BlameContent::TooManyLines { lines } => html! {
                    <p class="msg">{
                        format!("File too large to blame ({lines} lines, limit {MAX_BLOB_LINES}).")
                    }</p>
                },
            } }
        </>
    }
}

struct BlameTable<'a> {
    lines: &'a [String],
    runs: &'a [BlameRun],
    /// The blame's own URL, which every line anchor is appended to.
    self_url: &'a str,
    /// The lines the URL selected, highlighted as they are in the blob view.
    selected: Option<LineRange>,
    /// The gutter's shared shift-click handler.
    on_click: &'a Callback<MouseEvent>,
}

fn blame_table(table: BlameTable<'_>) -> Html {
    let BlameTable {
        lines,
        runs,
        self_url,
        selected,
        on_click,
    } = table;
    // Runs arrive in file order and cover a prefix of it, so walking the two
    // together is a single pass: `next` is the run that may start here, and
    // `open` how many more lines the current one still covers.
    let mut next = 0usize;
    let mut open = 0usize;
    let mut shade = false;
    let mut rows: Vec<Html> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let gutter = match runs.get(next).filter(|run| run.start == i) {
            Some(run) => {
                next += 1;
                open = run.num_lines;
                shade = !shade;
                Gutter::Start(run)
            }
            // Inside a run the cell above spans down over this row; past the
            // last settled run there is nothing to say yet, but the column
            // still needs its cell or every row below shifts left.
            None if open > 0 => Gutter::Covered,
            None => Gutter::Unattributed,
        };
        let shaded = open > 0 && shade;
        open = open.saturating_sub(1);
        let n = i + 1;
        rows.push(blame_row(
            n,
            line,
            gutter,
            BlameRowLink {
                selected: selected.is_some_and(|s| s.contains(n)),
                shade: shaded,
                self_url,
                on_click,
            },
        ));
    }
    html! {
        <table class="blame-table">
            <tbody>{ rows }</tbody>
        </table>
    }
}

/// What a line's gutter cell is: the head of a run, a row the cell above spans
/// over, or a line the walk has not settled yet.
enum Gutter<'a> {
    Start(&'a BlameRun),
    Covered,
    Unattributed,
}

struct BlameRowLink<'a> {
    /// Whether this line falls inside the selected range.
    selected: bool,
    /// Whether this line belongs to a shaded run.
    shade: bool,
    /// The blame's own URL, which the line anchor is appended to.
    self_url: &'a str,
    /// The gutter's shared shift-click handler.
    on_click: &'a Callback<MouseEvent>,
}

/// One line: its run's commit in the gutter (only on the run's first line,
/// spanning the rest), its number, and its text.
fn blame_row(n: usize, line: &str, gutter: Gutter<'_>, link: BlameRowLink<'_>) -> Html {
    let BlameRowLink {
        selected,
        shade,
        self_url,
        on_click,
    } = link;
    let href = format!("{self_url}{}", LineRange::single(n).anchor());
    let id = format!("n{n}");
    html! {
        <tr id={id} class={classes!(shade.then_some("alt"), selected.then_some("hl"))}>
            { match gutter {
                Gutter::Start(run) => html! {
                    <td class="hashes" rowspan={run.num_lines.to_string()}>
                        <a class="oid" href={run.commit_url.clone()}
                           title={run.detail.clone().unwrap_or_else(|| run.full_hash.clone())}>
                            { run.short_hash.clone() }
                        </a>
                        if let Some(prev) = &run.prev_url {
                            { " " }
                            <a class="blame-prev" href={prev.clone()}
                               title="Blame the previous revision">{ "^" }</a>
                        }
                    </td>
                },
                Gutter::Covered => html! {},
                Gutter::Unattributed => html! { <td class="hashes"></td> },
            } }
            <td class="lno">
                <a href={href} data-n={n.to_string()} onclick={on_click.clone()}>{ n }</a>
            </td>
            <td class="code">{ line }</td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `BlameView` to a static HTML string via SSR, breaking adjacent
    /// tags onto their own lines. See `render::tag` for why we go through SSR
    /// and why indentation is omitted.
    fn render(props: BlameProps) -> String {
        let html = futures::executor::block_on(
            yew::ServerRenderer::<BlameView>::with_props(move || props)
                .hydratable(false)
                .render(),
        );
        html.replace("><", ">\n<")
    }

    fn run(start: usize, num_lines: usize, hash: &str, parent: Option<&str>) -> BlameRun {
        BlameRun {
            start,
            num_lines,
            commit: ObjectId::from_hex(hash.as_bytes()).expect("test hash"),
            short_hash: hash[..8].to_string(),
            full_hash: hash.to_string(),
            commit_url: format!("#!/commit/{hash}"),
            prev_url: parent.map(|p| format!("#!/blame/src/lib.rs?h={p}")),
            detail: None,
        }
    }

    /// The same run once its commit object has been read — what the gutter
    /// shows on hover after [`fill_details`] has been round.
    fn detailed(run: BlameRun) -> BlameRun {
        BlameRun {
            detail: Some(format!(
                "{}\nauthor  A U Thor <author@example.com> 2026-09-12 10:00:00 +00:00\n\
                 committer  C O Mitter <committer@example.com> 2026-09-12 10:00:00 +00:00\n\
                 \nFix the thing",
                run.full_hash,
            )),
            ..run
        }
    }

    const A: &str = "0123abcd0123abcd0123abcd0123abcd0123abcd";
    const B: &str = "89abcdef89abcdef89abcdef89abcdef89abcdef";
    const ROOT: &str = "ffffffff11111111ffffffff11111111ffffffff";

    fn props(content: BlameContent, runs: Vec<BlameRun>, pending: bool) -> BlameProps {
        selected_props(content, runs, pending, None)
    }

    fn selected_props(
        content: BlameContent,
        runs: Vec<BlameRun>,
        pending: bool,
        lines: Option<LineRange>,
    ) -> BlameProps {
        BlameProps {
            blob_id: "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".to_string(),
            content,
            runs,
            pending,
            self_url: "#!/blame/src/lib.rs".to_string(),
            source_url: "#!/tree/src/lib.rs".to_string(),
            lines,
        }
    }

    fn lines(n: usize) -> BlameContent {
        BlameContent::Lines((1..=n).map(|i| format!("line {i}")).collect())
    }

    /// The finished view: every line covered, each run's commit spanning its
    /// lines, and alternating runs shaded.
    #[test]
    fn test_blame_html() {
        insta::assert_snapshot!(render(props(
            lines(5),
            vec![
                run(0, 2, A, Some(B)),
                run(2, 1, B, Some(ROOT)),
                run(3, 2, ROOT, None),
            ],
            false,
        )));
    }

    /// A root commit's run has no previous revision, so no `^`.
    #[test]
    fn test_blame_html_root_run_has_no_previous_link() {
        let html = render(props(lines(1), vec![run(0, 1, ROOT, None)], false));
        assert!(!html.contains("blame-prev"), "{html}");
    }

    /// Mid-walk: the file is fully rendered while only part of it has been
    /// attributed, so the reader can already read the code.
    #[test]
    fn test_blame_html_partial() {
        insta::assert_snapshot!(render(props(lines(4), vec![run(0, 2, A, Some(B))], true,)));
    }

    /// Nothing attributed yet — every line renders, with an empty gutter.
    #[test]
    fn test_blame_html_nothing_attributed_yet() {
        insta::assert_snapshot!(render(props(lines(2), Vec::new(), true)));
    }

    #[test]
    fn test_blame_html_selection() {
        insta::assert_snapshot!(render(selected_props(
            lines(4),
            vec![run(0, 2, A, Some(B)), run(2, 2, B, None)],
            false,
            Some(LineRange { start: 2, end: 3 }),
        )));
    }

    /// The selection is what the URL named, not something clamped to the file:
    /// a stale `#n900` on a short file highlights nothing rather than dragging
    /// the mark onto an arbitrary row.
    #[test]
    fn test_blame_selection_past_end_of_file() {
        let html = render(selected_props(
            lines(3),
            vec![run(0, 3, A, None)],
            false,
            Some(LineRange {
                start: 900,
                end: 902,
            }),
        ));
        assert!(
            !html.contains(r#"class="hl""#),
            "selected a row that doesn't exist: {html}"
        );
    }

    /// Crossing to the source view keeps the line being read: a reader who
    /// followed "blame" from line 3 and goes back should land on line 3, not at
    /// the top of the file.
    #[test]
    fn test_blame_source_link_carries_the_selection() {
        let html = render(selected_props(
            lines(3),
            Vec::new(),
            false,
            Some(LineRange::single(3)),
        ));
        assert!(
            html.contains(r##"href="#!/tree/src/lib.rs#n3""##),
            "source link dropped the selection: {html}"
        );
    }

    /// Every line's link carries the blame's whole route. A bare `#n2` would
    /// name no route at all and land the reader on the summary page.
    #[test]
    fn test_blame_line_links_carry_the_route() {
        let html = render(props(lines(2), Vec::new(), true));
        assert!(
            html.contains(r##"href="#!/blame/src/lib.rs#n2""##),
            "line link dropped the route: {html}"
        );
    }

    /// Once the detail pass has been round, the gutter's tooltip is the
    /// commit's author, committer, dates and subject rather than the bare hash.
    #[test]
    fn test_blame_html_hover_detail() {
        insta::assert_snapshot!(render(props(
            lines(3),
            vec![detailed(run(0, 2, A, Some(B))), run(2, 1, B, None)],
            false,
        )));
    }

    /// A run whose commit object hasn't been read yet keeps the full hash as
    /// its tooltip rather than losing one.
    #[test]
    fn test_blame_hover_detail_falls_back_to_the_hash() {
        let html = render(props(lines(1), vec![run(0, 1, A, None)], false));
        assert!(
            html.contains(&format!(r#"title="{A}""#)),
            "lost the hash tooltip: {html}"
        );
    }

    /// Runs sharing a commit share its tooltip: the detail is built per
    /// commit, not per run.
    #[test]
    fn test_blame_hover_detail_is_shared_between_a_commit_s_runs() {
        let html = render(props(
            lines(3),
            vec![
                detailed(run(0, 1, A, Some(B))),
                run(1, 1, B, Some(ROOT)),
                detailed(run(2, 1, A, Some(B))),
            ],
            false,
        ));
        assert_eq!(
            html.matches("Fix the thing").count(),
            2,
            "both runs of the same commit should carry its detail: {html}"
        );
    }

    #[test]
    fn test_blame_html_binary() {
        insta::assert_snapshot!(render(props(
            BlameContent::Binary { bytes: 4096 },
            Vec::new(),
            false,
        )));
    }

    #[test]
    fn test_blame_html_too_many_lines() {
        insta::assert_snapshot!(render(props(
            BlameContent::TooManyLines { lines: 30_000 },
            Vec::new(),
            false,
        )));
    }

    /// A file the blob view would have refused as binary is refused here too,
    /// and one that is merely large is measured the same way.
    #[test]
    fn test_blame_content_classification() {
        assert!(matches!(
            blame_content(b"a\0b"),
            BlameContent::Binary { bytes: 3 }
        ));
        assert!(matches!(
            blame_content(&vec![b'x'; MAX_BLOB_BYTES + 1]),
            BlameContent::TooManyBytes { .. }
        ));
        assert!(matches!(
            blame_content("x\n".repeat(MAX_BLOB_LINES + 1).as_bytes()),
            BlameContent::TooManyLines { .. }
        ));
        match blame_content(b"one\ntwo\n") {
            BlameContent::Lines(lines) => assert_eq!(lines, vec!["one", "two"]),
            _ => panic!("expected lines"),
        }
    }
}
