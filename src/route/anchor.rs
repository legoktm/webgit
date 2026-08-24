//! The `#n<A>[-n<B>]` line anchor a tree or blame URL may carry.

/// A selected run of lines in a blob, as named by the `#n<A>[-n<B>]` suffix on
/// a tree URL. Inclusive at both ends; a single line is a range whose ends are
/// equal, so the view has one shape to render rather than two.
///
/// Line numbers are 1-based, matching what the gutter shows. `start <= end` is
/// an invariant established on the way in, since a hand-written `#n10-n5` names
/// the same lines as `#n5-n10` and should not silently select nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    /// The range covering the single line `n`.
    pub(crate) fn single(n: usize) -> Self {
        LineRange { start: n, end: n }
    }

    /// The range spanning `a` and `b`, in either order.
    pub(crate) fn spanning(a: usize, b: usize) -> Self {
        LineRange {
            start: a.min(b),
            end: a.max(b),
        }
    }

    pub(crate) fn contains(self, n: usize) -> bool {
        self.start <= n && n <= self.end
    }

    /// The `#n…` suffix naming this range, as appended to a tree URL. A
    /// single-line range writes the short form, so an ordinary click produces
    /// the `#n5` that cgit and every existing webgit link already use.
    pub(crate) fn anchor(self) -> String {
        if self.start == self.end {
            format!("#n{}", self.start)
        } else {
            format!("#n{}-n{}", self.start, self.end)
        }
    }
}

/// Split a trailing `#n<A>[-n<B>]` line anchor off `hash`, returning the route
/// part and the range it named.
pub(crate) fn split_line_anchor(hash: &str) -> (&str, Option<LineRange>) {
    // Skip the fragment's own leading '#' so it is never taken as the separator.
    let Some(i) = hash
        .get(1..)
        .and_then(|rest| rest.rfind('#'))
        .map(|i| i + 1)
    else {
        return (hash, None);
    };
    match parse_line_anchor(&hash[i..]) {
        Some(range) => (&hash[..i], Some(range)),
        None => (hash, None),
    }
}

/// Parse `#n<A>` or `#n<A>-n<B>` into the range it names, or `None` if `s` is
/// not one of those.
fn parse_line_anchor(s: &str) -> Option<LineRange> {
    let body = s.strip_prefix("#n")?;
    match body.split_once("-n") {
        None => Some(LineRange::single(parse_line_number(body)?)),
        Some((a, b)) => Some(LineRange::spanning(
            parse_line_number(a)?,
            parse_line_number(b)?,
        )),
    }
}

fn parse_line_number(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::encode_path;

    #[test]
    fn test_split_line_anchor_single() {
        assert_eq!(
            split_line_anchor("#!/tree/src/lib.rs#n5"),
            ("#!/tree/src/lib.rs", Some(LineRange::single(5)))
        );
    }

    #[test]
    fn test_split_line_anchor_range() {
        assert_eq!(
            split_line_anchor("#!/tree/src/lib.rs#n5-n10"),
            ("#!/tree/src/lib.rs", Some(LineRange { start: 5, end: 10 }))
        );
    }

    /// The anchor comes off after the query, which is where a `?h=` leaves it.
    #[test]
    fn test_split_line_anchor_after_query() {
        assert_eq!(
            split_line_anchor("#!/tree/src/lib.rs?h=v1.0#n5-n10"),
            (
                "#!/tree/src/lib.rs?h=v1.0",
                Some(LineRange { start: 5, end: 10 })
            )
        );
    }

    /// Reversed ends name the same lines rather than an empty selection.
    #[test]
    fn test_split_line_anchor_reversed_is_ordered() {
        assert_eq!(
            split_line_anchor("#!/tree/f#n10-n5").1,
            Some(LineRange { start: 5, end: 10 })
        );
    }

    /// A hash with no anchor is returned whole, and the fragment's own leading
    /// '#' is never mistaken for the separator.
    #[test]
    fn test_split_line_anchor_absent() {
        assert_eq!(
            split_line_anchor("#!/tree/src/lib.rs"),
            ("#!/tree/src/lib.rs", None)
        );
        assert_eq!(split_line_anchor("#n5"), ("#n5", None));
        assert_eq!(split_line_anchor(""), ("", None));
        assert_eq!(split_line_anchor("#"), ("#", None));
    }

    /// A suffix that isn't a well-formed anchor stays part of the route string,
    /// rather than being silently discarded as if it had been one.
    #[test]
    fn test_split_line_anchor_malformed_is_left_alone() {
        for hash in [
            "#!/tree/f#n",
            "#!/tree/f#n0",
            "#!/tree/f#nope",
            "#!/tree/f#n5-",
            "#!/tree/f#n5-10",
            "#!/tree/f#n5-n",
            "#!/tree/f#n-5",
            "#!/tree/f#n 5",
            "#!/tree/f#n+5",
            "#!/tree/f#5",
        ] {
            assert_eq!(split_line_anchor(hash), (hash, None), "for {hash}");
        }
    }

    /// A `#` inside a path or ref is percent-encoded, so it can never be read
    /// as the anchor separator.
    #[test]
    fn test_split_line_anchor_encoded_hash_in_path() {
        let hash = format!("#!/tree/{}#n5", encode_path("a#n1/b.rs"));
        assert_eq!(
            split_line_anchor(&hash),
            ("#!/tree/a%23n1/b.rs", Some(LineRange::single(5)))
        );
    }

    /// The anchor a range writes is the one that parses back to it, with a
    /// single line taking the short `#n5` form.
    #[test]
    fn test_line_range_anchor_round_trips() {
        for range in [
            LineRange::single(1),
            LineRange::single(42),
            LineRange { start: 5, end: 10 },
        ] {
            let hash = format!("#!/tree/f{}", range.anchor());
            assert_eq!(split_line_anchor(&hash), ("#!/tree/f", Some(range)));
        }
        assert_eq!(LineRange::single(5).anchor(), "#n5");
        assert_eq!(LineRange { start: 5, end: 10 }.anchor(), "#n5-n10");
    }

    #[test]
    fn test_line_range_contains() {
        let range = LineRange { start: 5, end: 10 };
        assert!(!range.contains(4));
        assert!(range.contains(5));
        assert!(range.contains(7));
        assert!(range.contains(10));
        assert!(!range.contains(11));
    }
}
