//! Reading `.gitattributes`, and answering what one attribute says about a path.
//!
//! An attributes file is a list of patterns, each carrying assignments —
//! `attr`, `-attr`, `!attr`, `attr=value` — and the files stack: a directory's
//! own `.gitattributes` is consulted before its parent's, and within a file the
//! *last* matching line wins. [`Stack`] holds that chain and [`Stack::check`]
//! resolves one attribute against it, which is all any caller has needed so
//! far; the crate deliberately doesn't try to be git's whole attribute system.
//!
//! Patterns are the `.gitignore` ones, with the two differences
//! gitattributes(5) calls out: a negative pattern is an error rather than a
//! re-inclusion, and matching a directory does not extend to what is inside it
//! (`path/**` is how you say that). The matcher itself is a port of git's
//! `wildmatch`, so `**` is its own wildcard and `*` stops at a `/`.
//!
//! # What it doesn't do
//!
//! * **Macros.** A `[attr]name ...` line is parsed and then dropped, so an
//!   attribute set indirectly through one is reported as unspecified. git
//!   allows macros only in the root `.gitattributes`, `$GIT_DIR/info/attributes`
//!   and the global/system files.
//! * **Sources outside the tree.** `$GIT_DIR/info/attributes`, the global and
//!   system files, and git's built-in `[attr]binary` set are all invisible
//!   here; the caller supplies the files it has, which for a repository read
//!   over HTTP means the ones in the tree.
//! * **Case folding.** git folds when `core.ignoreCase` is set; see
//!   `wildmatch.rs` for why there is nothing sensible to fold against here.

#![deny(clippy::all)]

#[cfg(test)]
mod differential;
mod wildmatch;

use std::rc::Rc;
use wildmatch::wildmatch;

/// The name of the file this crate reads, in every directory of a tree.
pub const GITATTRIBUTES: &str = ".gitattributes";

/// What a lookup found for one attribute on one path.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State<'a> {
    /// `attr`: the attribute is on.
    Set,
    /// `-attr`: the attribute is explicitly off.
    Unset,
    /// `attr=value`.
    Value(&'a str),
    /// Either no line matched, or the matching line said `!attr`. git makes no
    /// distinction between the two once the lookup is over.
    Unspecified,
}

impl State<'_> {
    /// Whether the attribute is on, which is the question every boolean
    /// attribute (`export-ignore`, `binary`, …) is asking.
    pub fn is_set(&self) -> bool {
        matches!(self, State::Set)
    }
}

/// One attribute assignment on a line, before it is known which attribute a
/// caller will ask about.
#[derive(Debug, PartialEq, Eq)]
enum Setting {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

impl Setting {
    fn state(&self) -> State<'_> {
        match self {
            Setting::Set => State::Set,
            Setting::Unset => State::Unset,
            Setting::Unspecified => State::Unspecified,
            Setting::Value(v) => State::Value(v),
        }
    }
}

/// A pattern from an attributes file, with the two facts about it that decide
/// how it is matched.
#[derive(Debug)]
struct Pattern {
    /// The pattern, without a trailing `/` and without a leading one — the
    /// leading slash only ever meant "anchored", which is already implied by
    /// [`Pattern::anchored`].
    text: Vec<u8>,
    /// The pattern ended in `/`, so only a directory can match it.
    dir_only: bool,
    /// The pattern contains a `/`, so it is matched against the whole path
    /// relative to the file's directory rather than against a bare name.
    ///
    /// Decided before the leading slash is stripped, as git decides it: `/foo`
    /// is anchored to the file's own directory, where a bare `foo` matches at
    /// any depth below it.
    anchored: bool,
}

impl Pattern {
    fn parse(text: &[u8]) -> Self {
        let dir_only = text.last() == Some(&b'/');
        let text = if dir_only {
            &text[..text.len() - 1]
        } else {
            text
        };
        // Decided with the trailing slash already gone, so that `build/` is
        // still an unanchored name that matches at any depth.
        let anchored = text.contains(&b'/');
        let text = text.strip_prefix(b"/").unwrap_or(text);
        Pattern {
            text: text.to_vec(),
            dir_only,
            anchored,
        }
    }

    /// Does `path` — relative to the directory of the file this pattern came
    /// from, and without a trailing slash — match?
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored {
            wildmatch(&self.text, path.as_bytes(), true)
        } else {
            // An unanchored pattern is matched against the name alone, and
            // therefore at every depth below the file's directory.
            let name = path.rsplit('/').next().unwrap_or(path);
            wildmatch(&self.text, name.as_bytes(), false)
        }
    }
}

/// One line of an attributes file: a pattern and everything it assigns.
#[derive(Debug)]
struct Line {
    pattern: Pattern,
    settings: Vec<(String, Setting)>,
}

/// A parsed `.gitattributes` file.
#[derive(Debug, Default)]
pub struct AttributesFile {
    lines: Vec<Line>,
}

impl AttributesFile {
    /// Parse a file's bytes. Anything unparseable is dropped, line by line,
    /// which is what git does with it too — an attributes file is advisory, and
    /// a bad line in one is not worth failing a whole operation over.
    ///
    /// Lines are decoded as UTF-8, lossily. Patterns are matched as bytes, so
    /// this only affects a pattern whose bytes were not UTF-8 to begin with.
    pub fn parse(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        AttributesFile {
            lines: text.lines().filter_map(parse_line).collect(),
        }
    }

    /// Whether the file holds no usable lines, so that a caller can leave it
    /// off a stack rather than searching it for every path.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// What this file alone says about `attr` for `path`, or `None` if no line
    /// matched — which is the caller's cue to ask the next file out.
    ///
    /// Lines are scanned in reverse because the last matching line wins.
    fn lookup(&self, path: &str, is_dir: bool, attr: &str) -> Option<&Setting> {
        self.lines.iter().rev().find_map(|line| {
            if !line.pattern.matches(path, is_dir) {
                return None;
            }
            line.settings
                .iter()
                .find(|(name, _)| name == attr)
                .map(|(_, setting)| setting)
        })
    }
}

/// Parse one line into a pattern and its assignments, or nothing if the line
/// is blank, a comment, a macro definition, or malformed.
fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim_start_matches([' ', '\t', '\r']);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (pattern, rest) = split_pattern(line)?;
    // TODO: macros. `[attr]name a b -c` defines `name` as shorthand for the
    // attributes after it, which git then expands wherever `name` is set. Until
    // that is implemented the definition is dropped, so an attribute reached
    // only through a macro reads as unspecified rather than as whatever the
    // macro sets it to.
    if pattern.starts_with("[attr]") {
        return None;
    }
    // git warns and drops the line: a `.gitattributes` pattern cannot
    // re-include, and `\!` is how a literal `!` is written.
    if pattern.starts_with('!') {
        return None;
    }

    let mut settings = Vec::new();
    for token in rest.split_ascii_whitespace() {
        // git rejects the whole line when any assignment on it is malformed,
        // rather than keeping the assignments it understood.
        settings.push(parse_setting(token)?);
    }

    Some(Line {
        pattern: Pattern::parse(pattern.as_bytes()),
        settings,
    })
}

/// Split a line into its pattern and the assignments after it.
///
/// A pattern that needs to contain whitespace is written in git's C-style
/// quoting, which is the only reason this is more than a `split_once`.
fn split_pattern(line: &str) -> Option<(String, &str)> {
    if let Some(quoted) = line.strip_prefix('"') {
        let (pattern, rest) = unquote(quoted)?;
        return Some((pattern, rest));
    }
    let end = line.find([' ', '\t']).unwrap_or(line.len());
    Some((line[..end].to_string(), &line[end..]))
}

/// Read a C-quoted string, up to its closing quote, and return it with what
/// followed. `None` if the quoting is malformed, which git treats as the quote
/// having been an ordinary character.
fn unquote(quoted: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = quoted.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &quoted[i + 1..])),
            '\\' => {
                let (_, escape) = chars.next()?;
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'a' => out.push('\x07'),
                    'b' => out.push('\x08'),
                    'f' => out.push('\x0c'),
                    'v' => out.push('\x0b'),
                    '0'..='7' => {
                        // Three octal digits, git's own escape for a byte that
                        // isn't printable.
                        let mut value = escape as u32 - '0' as u32;
                        for _ in 0..2 {
                            let (_, digit) = chars.next()?;
                            value = value * 8 + digit.to_digit(8)?;
                        }
                        out.push(char::from_u32(value)?);
                    }
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }
    None
}

/// Parse one `attr`, `-attr`, `!attr` or `attr=value` token.
fn parse_setting(token: &str) -> Option<(String, Setting)> {
    let (name, setting) = if let Some(name) = token.strip_prefix('-') {
        (name, Setting::Unset)
    } else if let Some(name) = token.strip_prefix('!') {
        (name, Setting::Unspecified)
    } else if let Some((name, value)) = token.split_once('=') {
        (name, Setting::Value(value.to_string()))
    } else {
        (token, Setting::Set)
    };
    if !name_valid(name) {
        return None;
    }
    Some((name.to_string(), setting))
}

/// An attribute name is `[-A-Za-z0-9_.]`, may not start with `-`, and may not
/// take a `builtin_` name, which git reserves for its own.
fn name_valid(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with("builtin_")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_'))
}

/// The chain of attributes files covering one directory: its own first, then
/// its parent's, and so on out to the root of whatever the caller is walking.
///
/// Cheap to clone — a directory hands its own stack to each of its children,
/// and the files themselves are shared rather than copied.
#[derive(Debug, Default, Clone)]
pub struct Stack(Option<Rc<Node>>);

#[derive(Debug)]
struct Node {
    file: AttributesFile,
    /// How many bytes of a path belong to this file's directory, so that
    /// `path[base..]` is the path this file's patterns are matched against.
    base: usize,
    parent: Option<Rc<Node>>,
}

impl Stack {
    /// An empty stack: no file has been read yet, so nothing is specified.
    pub fn new() -> Self {
        Stack(None)
    }

    /// The stack that applies inside `dir`, given the attributes file found
    /// there. `dir` is the directory's path as the caller spells it, and every
    /// path passed to [`check`] afterwards must be spelled the same way.
    ///
    /// [`check`]: Stack::check
    pub fn push(&self, dir: &str, file: AttributesFile) -> Stack {
        Stack(Some(Rc::new(Node {
            file,
            base: if dir.is_empty() { 0 } else { dir.len() + 1 },
            parent: self.0.clone(),
        })))
    }

    /// What the stack says about `attr` for `path`.
    ///
    /// `path` is a full path in the caller's spelling, without a trailing slash
    /// even for a directory — `is_dir` is what tells a `build/` pattern that it
    /// has found its directory.
    ///
    /// The innermost file wins, and only falls through to the next one out when
    /// none of its lines matched: an explicit `!attr` is an answer, not a
    /// silence, and stops the search exactly as `attr` would.
    pub fn check(&self, path: &str, is_dir: bool, attr: &str) -> State<'_> {
        let mut node = self.0.as_deref();
        while let Some(current) = node {
            // A path shorter than the base cannot be inside this directory,
            // and would panic the slice; a caller that keeps its stack and its
            // paths in step never sees it.
            if let Some(relative) = path.get(current.base..)
                && let Some(setting) = current.file.lookup(relative, is_dir, attr)
            {
                return setting.state();
            }
            node = current.parent.as_deref();
        }
        State::Unspecified
    }

    /// Whether nothing has been pushed onto the stack, so a caller can skip the
    /// lookup entirely for a tree that has no attributes files in it.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(contents: &str) -> AttributesFile {
        AttributesFile::parse(contents.as_bytes())
    }

    fn root(contents: &str) -> Stack {
        Stack::new().push("", file(contents))
    }

    /// The four things a line can say about an attribute, and the two that a
    /// lookup cannot tell apart afterwards.
    #[test]
    fn test_states() {
        let stack = root("a set\nb -unset\nc !unspecified\nd valued=x\n");
        assert_eq!(stack.check("a", false, "set"), State::Set);
        assert_eq!(stack.check("b", false, "unset"), State::Unset);
        assert_eq!(stack.check("c", false, "unspecified"), State::Unspecified);
        assert_eq!(stack.check("d", false, "valued"), State::Value("x"));
        assert_eq!(
            stack.check("a", false, "never-mentioned"),
            State::Unspecified,
            "an attribute no line assigns is unspecified, like an explicit !attr"
        );
    }

    /// Within one file the last matching line wins, which is how a broad rule
    /// is narrowed by the lines under it.
    #[test]
    fn test_last_line_wins() {
        let stack = root("*.log export-ignore\nkeep.log -export-ignore\n");
        assert!(stack.check("a.log", false, "export-ignore").is_set());
        assert_eq!(
            stack.check("keep.log", false, "export-ignore"),
            State::Unset
        );
    }

    /// A directory's own file is consulted before its parent's, and an explicit
    /// answer there stops the search — including `!attr`, which says "nothing
    /// is specified here" rather than "keep looking".
    #[test]
    fn test_inner_file_wins() {
        let outer = root("*.txt export-ignore\n*.md export-ignore\n");
        let inner = outer.push("sub", file("*.txt -export-ignore\n*.md !export-ignore\n"));

        assert!(
            outer.check("a.txt", false, "export-ignore").is_set(),
            "the outer file still applies where the inner one is silent"
        );
        assert_eq!(
            inner.check("sub/a.txt", false, "export-ignore"),
            State::Unset
        );
        assert_eq!(
            inner.check("sub/a.md", false, "export-ignore"),
            State::Unspecified,
            "!attr in the inner file shadows the outer file's assignment"
        );
        assert!(
            inner.check("sub/a.rs", false, "export-ignore") == State::Unspecified,
            "and a path the inner file says nothing about falls through to the outer"
        );
    }

    /// A pattern with no slash matches a bare name at any depth below the file
    /// that declared it; one with a slash is anchored to that directory.
    #[test]
    fn test_anchoring() {
        let stack = root("loose export-ignore\n/anchored export-ignore\ndir/inner export-ignore\n");
        assert!(stack.check("loose", false, "export-ignore").is_set());
        assert!(
            stack
                .check("deep/down/loose", false, "export-ignore")
                .is_set()
        );
        assert!(stack.check("anchored", false, "export-ignore").is_set());
        assert_eq!(
            stack.check("deep/anchored", false, "export-ignore"),
            State::Unspecified,
            "a leading slash pins the pattern to the file's own directory"
        );
        assert!(stack.check("dir/inner", false, "export-ignore").is_set());
        assert_eq!(
            stack.check("deep/dir/inner", false, "export-ignore"),
            State::Unspecified,
            "a pattern containing a slash is anchored even without a leading one"
        );
    }

    /// A trailing slash means only a directory can match — the case
    /// `git check-attr` has no way to ask about, and the one `export-ignore`
    /// leans on hardest.
    #[test]
    fn test_directory_only_patterns() {
        let stack = root("build/ export-ignore\nvendor export-ignore\n");
        assert!(stack.check("build", true, "export-ignore").is_set());
        assert_eq!(
            stack.check("build", false, "export-ignore"),
            State::Unspecified,
            "a file named `build` is not what `build/` asked for"
        );
        assert_eq!(
            stack.check("build/a.txt", false, "export-ignore"),
            State::Unspecified,
            "matching a directory does not extend to what is inside it"
        );
        assert!(
            stack.check("vendor", true, "export-ignore").is_set(),
            "a pattern without a trailing slash matches a directory too"
        );
        assert!(stack.check("vendor", false, "export-ignore").is_set());
    }

    /// `**` spans directories where `*` stops at the first slash.
    #[test]
    fn test_wildcards_across_directories() {
        let stack = root("a/**/x export-ignore\nb/*/y export-ignore\n");
        assert!(stack.check("a/x", false, "export-ignore").is_set());
        assert!(stack.check("a/one/two/x", false, "export-ignore").is_set());
        assert!(stack.check("b/one/y", false, "export-ignore").is_set());
        assert_eq!(
            stack.check("b/one/two/y", false, "export-ignore"),
            State::Unspecified
        );
    }

    /// Blank lines, comments, and a pattern quoted because it has a space in it.
    #[test]
    fn test_parsing_oddities() {
        let stack = root(
            "\n   \n# a comment\n  *.txt export-ignore\n\"two words.md\" export-ignore\n\
             !negative export-ignore\ninvalid.md export-ignore bad*name\n",
        );
        assert!(stack.check("a.txt", false, "export-ignore").is_set());
        assert!(stack.check("two words.md", false, "export-ignore").is_set());
        assert_eq!(
            stack.check("negative", false, "export-ignore"),
            State::Unspecified,
            "a negative pattern is an error in an attributes file, and its line is dropped"
        );
        assert_eq!(
            stack.check("invalid.md", false, "export-ignore"),
            State::Unspecified,
            "one unparseable assignment drops the whole line, as it does in git, \
             rather than keeping the assignments either side of it"
        );
    }

    /// A `[attr]` line defines a macro, which this crate parses and then drops.
    /// git would report `export-ignore` as set for `a.macro` here; see the
    /// crate docs for why that gap is left open.
    #[test]
    fn test_macros_are_not_expanded() {
        let stack = root("[attr]mymacro export-ignore\n*.macro mymacro\n");
        assert_eq!(
            stack.check("a.macro", false, "export-ignore"),
            State::Unspecified
        );
        assert!(
            stack.check("a.macro", false, "mymacro").is_set(),
            "the attribute named by the macro is still set on the path itself"
        );
    }

    /// An empty stack, and a file with nothing usable in it, answer without
    /// having to be special-cased by callers.
    #[test]
    fn test_empty() {
        assert!(Stack::new().is_empty());
        assert_eq!(
            Stack::new().check("a.txt", false, "export-ignore"),
            State::Unspecified
        );
        assert!(file("# nothing but a comment\n").is_empty());
        assert!(!file("*.txt export-ignore\n").is_empty());
    }
}
