//! Git's shell-style pattern matcher, ported from `wildmatch.c`.
//!
//! This is the matcher behind `.gitignore` and `.gitattributes` patterns, and
//! it is not `fnmatch`: `**` is a distinct wildcard from `*`, and in pathname
//! mode neither `*` nor `?` nor a character class will match a `/`. The
//! algorithm is the recursive backtracking one git uses, kept close to the C so
//! the two can be read side by side — including the two abort codes, which are
//! what stop a failed `*` from being retried at every position.
//!
//! Bytes rather than `str` throughout: git compares paths as bytes, and a
//! pattern is whatever the `.gitattributes` file happened to hold.
//!
//! Case folding is deliberately absent. git folds only when `core.ignoreCase`
//! is on, which is a property of the filesystem a repository was cloned onto;
//! reading a bare repository over HTTP there is no such filesystem, and the
//! tree's own byte-exact names are the only truth available.

/// How far a failed match should unwind, mirroring `wildmatch.c`'s internal
/// return values.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Outcome {
    Match,
    NoMatch,
    /// This position can never match, and neither can any later one: give up on
    /// the whole pattern rather than letting an enclosing `*` retry.
    AbortAll,
    /// As above, but only up to the nearest enclosing `**`, which may still
    /// match by consuming the `/` that stopped a single `*`.
    AbortToStarStar,
}

use Outcome::{AbortAll, AbortToStarStar, Match, NoMatch};

/// The characters that make a pattern more than a literal string.
fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// Does `text` match `pattern`?
///
/// With `pathname`, `/` is a boundary the single-character wildcards will not
/// cross — git's `WM_PATHNAME`, which is set for patterns that are matched
/// against a whole path and clear for those matched against a bare name.
pub(crate) fn wildmatch(pattern: &[u8], text: &[u8], pathname: bool) -> bool {
    dowild(pattern, text, pathname) == Match
}

fn dowild(p: &[u8], text: &[u8], pathname: bool) -> Outcome {
    let at = |s: &[u8], i: usize| -> u8 { s.get(i).copied().unwrap_or(0) };

    let mut pi = 0usize;
    let mut ti = 0usize;

    while pi < p.len() {
        let mut p_ch = p[pi];
        let mut t_ch = at(text, ti);
        if t_ch == 0 && p_ch != b'*' {
            return AbortAll;
        }
        match p_ch {
            b'\\' => {
                // A literal match with the character that follows; a pattern
                // ending in a lone backslash compares against the end of the
                // text, which the check above has already ruled out.
                pi += 1;
                if t_ch != at(p, pi) {
                    return NoMatch;
                }
            }
            b'?' => {
                if pathname && t_ch == b'/' {
                    return NoMatch;
                }
            }
            b'*' => {
                pi += 1;
                let match_slash;
                if at(p, pi) == b'*' {
                    // A run of two or more stars. Whether it spans `/` depends
                    // on the run standing alone as a path component: `a/**/b`
                    // does, `a**b` does not.
                    let first_extra = pi;
                    while at(p, pi) == b'*' {
                        pi += 1;
                    }
                    let starts_component = first_extra < 2 || p[first_extra - 2] == b'/';
                    let ends_component = pi >= p.len()
                        || at(p, pi) == b'/'
                        || (at(p, pi) == b'\\' && at(p, pi + 1) == b'/');
                    if !pathname {
                        match_slash = true;
                    } else if starts_component && ends_component {
                        // `foo/**/bar` has to match `foo/bar` as well as
                        // `foo/a/bar`, so try the pattern past the trailing
                        // slash against the text as it stands.
                        if at(p, pi) == b'/' && dowild(&p[pi + 1..], &text[ti..], pathname) == Match
                        {
                            return Match;
                        }
                        match_slash = true;
                    } else {
                        match_slash = false;
                    }
                } else {
                    match_slash = !pathname;
                }

                if pi >= p.len() {
                    // A trailing `**` takes everything left; a trailing `*`
                    // only what has no `/` in it.
                    if !match_slash && text[ti..].contains(&b'/') {
                        return AbortToStarStar;
                    }
                    return Match;
                } else if !match_slash && at(p, pi) == b'/' {
                    // A single star followed by a slash matches exactly the
                    // rest of this path component.
                    let Some(slash) = text[ti..].iter().position(|&c| c == b'/') else {
                        return AbortAll;
                    };
                    ti += slash;
                    // The slash itself is consumed by the loop's step below.
                    pi += 1;
                    ti += 1;
                    continue;
                }

                loop {
                    if t_ch == 0 {
                        break;
                    }
                    // Skip ahead to where the literal after the star could
                    // start: everything before it must belong to the star.
                    if !is_glob_special(at(p, pi)) {
                        p_ch = at(p, pi);
                        while {
                            t_ch = at(text, ti);
                            t_ch != 0 && (match_slash || t_ch != b'/')
                        } {
                            if t_ch == p_ch {
                                break;
                            }
                            ti += 1;
                        }
                        if t_ch != p_ch {
                            return if match_slash {
                                AbortAll
                            } else {
                                AbortToStarStar
                            };
                        }
                    }
                    let matched = dowild(&p[pi..], &text[ti..], pathname);
                    if matched != NoMatch {
                        if !match_slash || matched != AbortToStarStar {
                            return matched;
                        }
                    } else if !match_slash && t_ch == b'/' {
                        return AbortToStarStar;
                    }
                    ti += 1;
                    t_ch = at(text, ti);
                }
                return AbortAll;
            }
            b'[' => {
                pi += 1;
                p_ch = at(p, pi);
                let negated = p_ch == b'!' || p_ch == b'^';
                if negated {
                    pi += 1;
                    p_ch = at(p, pi);
                }
                let mut prev_ch = 0u8;
                let mut matched = false;
                // The first `]` is a literal member, so this is a do-while:
                // the terminator is only recognised from the second character.
                loop {
                    if p_ch == 0 {
                        return AbortAll;
                    }
                    if p_ch == b'\\' {
                        pi += 1;
                        p_ch = at(p, pi);
                        if p_ch == 0 {
                            return AbortAll;
                        }
                        if t_ch == p_ch {
                            matched = true;
                        }
                    } else if p_ch == b'-'
                        && prev_ch != 0
                        && at(p, pi + 1) != 0
                        && at(p, pi + 1) != b']'
                    {
                        pi += 1;
                        p_ch = at(p, pi);
                        if p_ch == b'\\' {
                            pi += 1;
                            p_ch = at(p, pi);
                            if p_ch == 0 {
                                return AbortAll;
                            }
                        }
                        if t_ch <= p_ch && t_ch >= prev_ch {
                            matched = true;
                        }
                        // So that the range's end can't start another range.
                        p_ch = 0;
                    } else if p_ch == b'[' && at(p, pi + 1) == b':' {
                        let start = pi + 2;
                        let mut end = start;
                        while at(p, end) != 0 && at(p, end) != b']' {
                            end += 1;
                        }
                        if at(p, end) == 0 {
                            return AbortAll;
                        }
                        if end > start && p[end - 1] == b':' {
                            match &p[start..end - 1] {
                                b"alnum" => matched |= t_ch.is_ascii_alphanumeric(),
                                b"alpha" => matched |= t_ch.is_ascii_alphabetic(),
                                b"blank" => matched |= t_ch == b' ' || t_ch == b'\t',
                                b"cntrl" => matched |= t_ch.is_ascii_control(),
                                b"digit" => matched |= t_ch.is_ascii_digit(),
                                b"graph" => matched |= t_ch.is_ascii_graphic(),
                                b"lower" => matched |= t_ch.is_ascii_lowercase(),
                                b"print" => matched |= t_ch.is_ascii() && !t_ch.is_ascii_control(),
                                b"punct" => matched |= t_ch.is_ascii_punctuation(),
                                b"space" => matched |= t_ch.is_ascii_whitespace() || t_ch == 0x0b,
                                b"upper" => matched |= t_ch.is_ascii_uppercase(),
                                b"xdigit" => matched |= t_ch.is_ascii_hexdigit(),
                                // A malformed class name is not a set to fall
                                // back on; git gives up on the pattern.
                                _ => return AbortAll,
                            }
                            pi = end;
                            p_ch = 0;
                        } else {
                            // No `:]` after all, so `[` was an ordinary member.
                            if t_ch == b'[' {
                                matched = true;
                            }
                            p_ch = b'[';
                        }
                    } else if t_ch == p_ch {
                        matched = true;
                    }
                    prev_ch = p_ch;
                    pi += 1;
                    p_ch = at(p, pi);
                    if p_ch == b']' {
                        break;
                    }
                }
                if matched == negated || (pathname && t_ch == b'/') {
                    return NoMatch;
                }
            }
            _ => {
                if t_ch != p_ch {
                    return NoMatch;
                }
            }
        }
        pi += 1;
        ti += 1;
    }

    if ti < text.len() { NoMatch } else { Match }
}

#[cfg(test)]
mod tests {
    use super::wildmatch;

    /// git's own wildmatch corpus, lifted from `t/t3070-wildmatch.sh`.
    ///
    /// Each row is `(pattern, text, matches_with_pathname, matches_without)` —
    /// the first and third expectation columns of the `match` helper there,
    /// which are the two modes this port has (the other two are the case-folding
    /// variants it deliberately doesn't).
    ///
    /// It is worth having in full: the corpus is where every awkward case in
    /// the algorithm is written down — the `**` component rules, the abort
    /// codes' effect on backtracking, and the character-class edges that a
    /// hand-written test would never think to try.
    #[rustfmt::skip]
    const CORPUS: &[(&str, &str, bool, bool)] = &[
    ("foo", "foo", true, true),
    ("bar", "foo", false, false),
    ("", "", true, true),
    ("???", "foo", true, true),
    ("??", "foo", false, false),
    ("*", "foo", true, true),
    ("f*", "foo", true, true),
    ("*f", "foo", false, false),
    ("*foo*", "foo", true, true),
    ("*ob*a*r*", "foobar", true, true),
    ("*ab", "aaaaaaabababab", true, true),
    ("foo\\*", "foo*", true, true),
    ("foo\\*bar", "foobar", false, false),
    ("f\\\\oo", "f\\oo", true, true),
    ("foo\\", "foo\\", false, false),
    ("*[al]?", "ball", true, true),
    ("[ten]", "ten", false, false),
    ("**[!te]", "ten", true, true),
    ("**[!ten]", "ten", false, false),
    ("t[a-g]n", "ten", true, true),
    ("t[!a-g]n", "ten", false, false),
    ("t[!a-g]n", "ton", true, true),
    ("t[^a-g]n", "ton", true, true),
    ("a[]]b", "a]b", true, true),
    ("a[]-]b", "a-b", true, true),
    ("a[]-]b", "a]b", true, true),
    ("a[]-]b", "aab", false, false),
    ("a[]a-]b", "aab", true, true),
    ("]", "]", true, true),
    ("foo*bar", "foo/baz/bar", false, true),
    ("foo**bar", "foo/baz/bar", false, true),
    ("foo**bar", "foobazbar", true, true),
    ("foo/**/bar", "foo/baz/bar", true, true),
    ("foo/**/**/bar", "foo/baz/bar", true, false),
    ("foo/**/bar", "foo/b/a/z/bar", true, true),
    ("foo/**/**/bar", "foo/b/a/z/bar", true, true),
    ("foo/**/bar", "foo/bar", true, false),
    ("foo/**/**/bar", "foo/bar", true, false),
    ("foo?bar", "foo/bar", false, true),
    ("foo[/]bar", "foo/bar", false, true),
    ("foo[^a-z]bar", "foo/bar", false, true),
    ("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo/bar", false, true),
    ("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo-bar", true, true),
    ("**/foo", "foo", true, false),
    ("**/foo", "XXX/foo", true, true),
    ("**/foo", "bar/baz/foo", true, true),
    ("*/foo", "bar/baz/foo", false, true),
    ("**/bar*", "foo/bar/baz", false, true),
    ("**/bar/*", "deep/foo/bar/baz", true, true),
    ("**/bar/*", "deep/foo/bar/baz/", false, true),
    ("**/bar/**", "deep/foo/bar/baz/", true, true),
    ("**/bar/*", "deep/foo/bar", false, false),
    ("**/bar/**", "deep/foo/bar/", true, true),
    ("**/bar**", "foo/bar/baz", false, true),
    ("*/bar/**", "foo/bar/baz/x", true, true),
    ("*/bar/**", "deep/foo/bar/baz/x", false, true),
    ("**/bar/*/*", "deep/foo/bar/baz/x", true, true),
    ("a[c-c]st", "acrt", false, false),
    ("a[c-c]rt", "acrt", true, true),
    ("[!]-]", "]", false, false),
    ("[!]-]", "a", true, true),
    ("\\", "", false, false),
    ("\\", "\\", false, false),
    ("*/\\", "XXX/\\", false, false),
    ("*/\\\\", "XXX/\\", true, true),
    ("foo", "foo", true, true),
    ("@foo", "@foo", true, true),
    ("@foo", "foo", false, false),
    ("\\[ab]", "[ab]", true, true),
    ("[[]ab]", "[ab]", true, true),
    ("[[:]ab]", "[ab]", true, true),
    ("[[::]ab]", "[ab]", false, false),
    ("[[:digit]ab]", "[ab]", true, true),
    ("[\\[:]ab]", "[ab]", true, true),
    ("\\??\\?b", "?a?b", true, true),
    ("\\a\\b\\c", "abc", true, true),
    ("", "foo", false, false),
    ("**/t[o]", "foo/bar/baz/to", true, true),
    ("[[:alpha:]][[:digit:]][[:upper:]]", "a1B", true, true),
    ("[[:digit:][:upper:][:space:]]", "a", false, false),
    ("[[:digit:][:upper:][:space:]]", "A", true, true),
    ("[[:digit:][:upper:][:space:]]", "1", true, true),
    ("[[:digit:][:upper:][:spaci:]]", "1", false, false),
    ("[[:digit:][:upper:][:space:]]", " ", true, true),
    ("[[:digit:][:upper:][:space:]]", ".", false, false),
    ("[[:digit:][:punct:][:space:]]", ".", true, true),
    ("[[:xdigit:]]", "5", true, true),
    ("[[:xdigit:]]", "f", true, true),
    ("[[:xdigit:]]", "D", true, true),
    ("[[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:graph:][:lower:][:print:][:punct:][:space:][:upper:][:xdigit:]]", "_", true, true),
    ("[^[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:lower:][:space:][:upper:][:xdigit:]]", ".", true, true),
    ("[a-c[:digit:]x-z]", "5", true, true),
    ("[a-c[:digit:]x-z]", "b", true, true),
    ("[a-c[:digit:]x-z]", "y", true, true),
    ("[a-c[:digit:]x-z]", "q", false, false),
    ("[\\\\-^]", "]", true, true),
    ("[\\\\-^]", "[", false, false),
    ("[\\-_]", "-", true, true),
    ("[\\]]", "]", true, true),
    ("[\\]]", "\\]", false, false),
    ("[\\]]", "\\", false, false),
    ("a[]b", "ab", false, false),
    ("a[]b", "a[]b", false, false),
    ("ab[", "ab[", false, false),
    ("[!", "ab", false, false),
    ("[-", "ab", false, false),
    ("[-]", "-", true, true),
    ("[a-", "-", false, false),
    ("[!a-", "-", false, false),
    ("[--A]", "-", true, true),
    ("[--A]", "5", true, true),
    ("[ --]", " ", true, true),
    ("[ --]", "$", true, true),
    ("[ --]", "-", true, true),
    ("[ --]", "0", false, false),
    ("[---]", "-", true, true),
    ("[------]", "-", true, true),
    ("[a-e-n]", "j", false, false),
    ("[a-e-n]", "-", true, true),
    ("[!------]", "a", true, true),
    ("[]-a]", "[", false, false),
    ("[]-a]", "^", true, true),
    ("[!]-a]", "^", false, false),
    ("[!]-a]", "[", true, true),
    ("[a^bc]", "^", true, true),
    ("[a-]b]", "-b]", true, true),
    ("[\\]", "\\", false, false),
    ("[\\\\]", "\\", true, true),
    ("[!\\\\]", "\\", false, false),
    ("[A-\\\\]", "G", true, true),
    ("b*a", "aaabbb", false, false),
    ("*ba*", "aabcaa", false, false),
    ("[,]", ",", true, true),
    ("[\\\\,]", ",", true, true),
    ("[\\\\,]", "\\", true, true),
    ("[,-.]", "-", true, true),
    ("[,-.]", "+", false, false),
    ("[,-.]", "-.]", false, false),
    ("[\\1-\\3]", "2", true, true),
    ("[\\1-\\3]", "3", true, true),
    ("[\\1-\\3]", "4", false, false),
    ("[[-\\]]", "\\", true, true),
    ("[[-\\]]", "[", true, true),
    ("[[-\\]]", "]", true, true),
    ("[[-\\]]", "-", false, false),
    ("-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1", true, true),
    ("-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-X-70-iso8859-1", false, false),
    ("-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-/-70-iso8859-1", false, false),
    ("XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*", "XXX/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1", true, true),
    ("XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*", "XXX/adobe/courier/bold/o/normal//12/120/75/75/X/70/iso8859/1", false, false),
    ("**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt", true, true),
    ("**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txtz", false, false),
    ("*/*/*", "foo", false, false),
    ("*/*/*", "foo/bar", false, false),
    ("*/*/*", "foo/bba/arr", true, true),
    ("*/*/*", "foo/bb/aa/rr", false, true),
    ("**/**/**", "foo/bb/aa/rr", true, true),
    ("*X*i", "abcXdefXghi", true, true),
    ("*X*i", "ab/cXd/efXg/hi", false, true),
    ("*/*X*/*/*i", "ab/cXd/efXg/hi", true, true),
    ("**/*X*/**/*i", "ab/cXd/efXg/hi", true, true),
    ("fo", "foo", false, false),
    ("foo/bar", "foo/bar", true, true),
    ("foo/*", "foo/bar", true, true),
    ("foo/*", "foo/bba/arr", false, true),
    ("foo/**", "foo/bba/arr", true, true),
    ("foo*", "foo/bba/arr", false, true),
    ("foo**", "foo/bba/arr", false, true),
    ("foo/*arr", "foo/bba/arr", false, true),
    ("foo/**arr", "foo/bba/arr", false, true),
    ("foo/*z", "foo/bba/arr", false, false),
    ("foo/**z", "foo/bba/arr", false, false),
    ("foo?bar", "foo/bar", false, true),
    ("foo[/]bar", "foo/bar", false, true),
    ("foo[^a-z]bar", "foo/bar", false, true),
    ("*Xg*i", "ab/cXd/efXg/hi", false, true),
    ("[A-Z]", "a", false, false),
    ("[A-Z]", "A", true, true),
    ("[a-z]", "A", false, false),
    ("[a-z]", "a", true, true),
    ("[[:upper:]]", "a", false, false),
    ("[[:upper:]]", "A", true, true),
    ("[[:lower:]]", "A", false, false),
    ("[[:lower:]]", "a", true, true),
    ("[B-Za]", "A", false, false),
    ("[B-Za]", "a", true, true),
    ("[B-a]", "A", false, false),
    ("[B-a]", "a", true, true),
    ("[Z-y]", "z", false, false),
    ("[Z-y]", "Z", true, true),
    ];

    #[test]
    fn test_matches_git_wildmatch_corpus() {
        let mut failures = Vec::new();
        for &(pattern, text, with_pathname, without) in CORPUS {
            for (pathname, expected) in [(true, with_pathname), (false, without)] {
                let got = wildmatch(pattern.as_bytes(), text.as_bytes(), pathname);
                if got != expected {
                    failures.push(format!(
                        "wildmatch({pattern:?}, {text:?}, pathname={pathname}) \
                         was {got}, git says {expected}"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases disagree with git:\n{}",
            failures.len(),
            CORPUS.len() * 2,
            failures.join("\n")
        );
    }
}
