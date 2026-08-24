//! The knobs on a commit's diff, as the `?dt=`/`?context=`/`?ignorews=`/
//! `?ss=` query carries them.
//!
//! Parsing and serialising are exercised through the router, in
//! [`super`]'s tests: the query string is only ever read as part of a URL.

/// How much of a commit's diff to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum DiffMode {
    /// The diff itself, under the diffstat.
    #[default]
    Unified,
    /// The diffstat alone — cgit's `dt=2`, for reading which files a commit
    /// touched without paying for the diff.
    StatOnly,
}

/// The knobs on a commit's diff, as `?context=`, `?ignorews=`, `?dt=` and
/// `?ss=` carry them. [`Default`] is git's own default view, and the state in
/// which none of them appear in the URL.
///
/// The names and values are cgit's, so a link from a cgit instance lands on the
/// same view here — including `dt=1`, which cgit spells "side by side" as a
/// third diff type where this splits it out into its own flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct DiffView {
    /// Lines of context around each hunk. `None` is git's default of three,
    /// held apart from an explicit `context=3` only so the URL can stay clean.
    pub(crate) context: Option<usize>,
    /// Ignore whitespace-only changes (`ignorews=1`, git's `-w`).
    pub(crate) ignore_whitespace: bool,
    /// Which parts of the diff to render (`dt=`).
    pub(crate) mode: DiffMode,
    /// Lay the diff out in two columns (`ss=1`). Meaningless — and dropped from
    /// the URL — when [`mode`](DiffView::mode) hides the diff.
    pub(crate) side_by_side: bool,
}

/// The context widths the control offers, which are cgit's: every width up to
/// ten, then in fives. A URL may name any width in `1..=CONTEXT_MAX`; these are
/// only what the buttons step through.
pub(crate) const CONTEXT_CHOICES: &[usize] =
    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15, 20, 25, 30, 35, 40];

/// git's default, and the width that is left out of a URL.
const CONTEXT_DEFAULT: usize = 3;
/// The widest context a URL may ask for. cgit's control stops here too; the cap
/// matters because context is a per-hunk cost paid on every file in the commit.
const CONTEXT_MAX: usize = 40;

impl DiffView {
    /// The context width to diff at, with the unset case resolved to git's.
    pub(crate) fn context_lines(self) -> usize {
        self.context.unwrap_or(CONTEXT_DEFAULT)
    }

    /// The same settings in the form the diff machinery takes them.
    pub(crate) fn diff_options(self) -> gib_patch::DiffOptions {
        gib_patch::DiffOptions {
            context: self.context_lines(),
            whitespace: if self.ignore_whitespace {
                gib_patch::Whitespace::Ignore
            } else {
                gib_patch::Whitespace::Significant
            },
        }
    }

    /// Whether the diff body is rendered at all. Stat-only hides it, which is
    /// what makes `ss` meaningless in that mode.
    pub(crate) fn shows_diff(self) -> bool {
        self.mode == DiffMode::Unified
    }

    /// The query string for this view, `?` included, or empty when every
    /// setting is at its default.
    ///
    /// Parameter order and the rules for leaving one out are cgit's
    /// (`ui-shared.c`): a default is absent rather than spelled out, so the
    /// plain `#!/commit/<sha>` stays the URL of the ordinary view.
    pub(super) fn query(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mode == DiffMode::StatOnly {
            parts.push("dt=2".to_string());
        }
        if let Some(n) = self.context.filter(|&n| n != CONTEXT_DEFAULT) {
            parts.push(format!("context={n}"));
        }
        if self.ignore_whitespace {
            parts.push("ignorews=1".to_string());
        }
        // A hidden diff has no layout, so the flag would only be a setting the
        // reader cannot see the effect of.
        if self.side_by_side && self.shows_diff() {
            parts.push("ss=1".to_string());
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }

    /// Read the settings out of a commit URL's query string.
    pub(super) fn parse(query_string: &str) -> Self {
        let mut view = Self::default();
        for part in query_string.split('&') {
            if let Some(v) = part.strip_prefix("context=") {
                // Out-of-range and unparseable widths fall back to the default
                // rather than to an error page: a diff is still a diff.
                view.context = v.parse().ok().filter(|&n| (1..=CONTEXT_MAX).contains(&n));
            } else if let Some(v) = part.strip_prefix("ignorews=") {
                view.ignore_whitespace = v == "1";
            } else if let Some(v) = part.strip_prefix("ss=") {
                view.side_by_side = v == "1";
            } else if let Some(v) = part.strip_prefix("dt=") {
                match v {
                    // cgit spells side-by-side as a third diff type. Read it as
                    // one here so its links land on the view they name.
                    "1" => view.side_by_side = true,
                    "2" => view.mode = DiffMode::StatOnly,
                    _ => view.mode = DiffMode::Unified,
                }
            }
        }
        view
    }
}
