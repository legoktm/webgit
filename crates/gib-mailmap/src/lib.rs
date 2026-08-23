//! Reading `.mailmap`, and rewriting a contact with what it says.
//!
//! A mailmap is a list of lines, each of which names the contact a commit
//! actually carries and the canonical one it should be shown as. git accepts
//! four shapes, all of them read by the same "name then `<email>`, maybe
//! twice" rule:
//!
//! ```text
//! Proper Name <commit@email.xx>
//! <proper@email.xx> <commit@email.xx>
//! Proper Name <proper@email.xx> <commit@email.xx>
//! Proper Name <proper@email.xx> Commit Name <commit@email.xx>
//! ```
//!
//! The last pair on the line is what a commit is matched on, and the first is
//! what it is shown as; with only one pair on the line, the match is on that
//! email and only the name is replaced. Matching is case-insensitive on the
//! email, and on the name too for the fourth shape. See [`Mailmap::map`] for
//! how the two interact.
//!
//! The crate does no IO: [`Mailmap::parse`] takes the bytes of the file, and
//! finding it is the caller's job. git reads the worktree `.mailmap`, plus
//! whatever `mailmap.file` and `mailmap.blob` point at, defaulting the latter
//! to `HEAD:.mailmap` in a bare repository — and a repository read over HTTP
//! has only that last one.

#![deny(clippy::all)]

#[cfg(test)]
mod differential;

use std::collections::BTreeMap;

/// The name of the file this crate reads, at the root of a tree.
pub const MAILMAP: &str = ".mailmap";

/// The canonical name and email one entry supplies. Either half can be absent,
/// in which case the contact keeps the one it came with.
#[derive(Debug, Default, PartialEq, Eq)]
struct Replacement {
    name: Option<Vec<u8>>,
    email: Option<Vec<u8>>,
}

/// Everything the file says about one commit email: what to do with a contact
/// carrying it, and — for lines that named a commit *name* as well — what to
/// do instead when the name matches too.
#[derive(Debug, Default)]
struct Entry {
    /// From the lines that matched on the email alone. Filled in a half at a
    /// time: a name-only line and an email-only line for the same address
    /// combine, which is what git's mutation of one entry per email amounts
    /// to.
    simple: Replacement,
    /// From the lines that matched on a name as well, keyed by that name,
    /// case-folded. A later line for the same name replaces the earlier one
    /// outright rather than combining with it.
    by_name: BTreeMap<Vec<u8>, Replacement>,
}

/// A parsed `.mailmap`.
///
/// Build one with [`Mailmap::parse`] and ask it about contacts with
/// [`Mailmap::map`]. An empty map (no file, or a file with nothing usable in
/// it) is the [`Default`], and maps every contact to itself.
#[derive(Debug, Default)]
pub struct Mailmap {
    /// Keyed by the commit email, case-folded.
    entries: BTreeMap<Vec<u8>, Entry>,
}

impl Mailmap {
    /// Parse the bytes of a `.mailmap` file.
    ///
    /// Nothing here can fail: a line git cannot make sense of — no `<`, no
    /// closing `>`, an empty email where one is required — is dropped, as is
    /// a line whose first byte is `#`. Text after the last `>` of a line is
    /// ignored, which is what lets a `\r\n` file parse like a `\n` one.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut map = Mailmap::default();
        for line in bytes.split(|&b| b == b'\n') {
            map.read_line(line);
        }
        map
    }

    /// Whether the map has no entries, so [`map`](Self::map) can only ever
    /// answer with what it was given.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The canonical contact for `name` and `email`, which is `(name, email)`
    /// itself when the map has nothing to say about it.
    ///
    /// The email selects the entry, case-insensitively. If that entry also
    /// carries name-keyed replacements and the name matches one of them
    /// (case-insensitively again), that replacement wins; otherwise the
    /// entry's email-only replacement applies. Either half of a replacement
    /// can be absent, and an absent half leaves that half of the contact
    /// alone — so a `Proper Name <commit@email.xx>` line rewrites the name and
    /// keeps the address.
    pub fn map<'a>(&'a self, name: &'a [u8], email: &'a [u8]) -> (&'a [u8], &'a [u8]) {
        // Most repositories have no mailmap at all, and every contact they
        // display comes through here; answering those without case-folding
        // anything keeps that case free.
        if self.entries.is_empty() {
            return (name, email);
        }
        let Some(entry) = self.entries.get(fold(email).as_slice()) else {
            return (name, email);
        };
        let replacement = entry
            .by_name
            .get(fold(name).as_slice())
            .unwrap_or(&entry.simple);
        (
            replacement.name.as_deref().unwrap_or(name),
            replacement.email.as_deref().unwrap_or(email),
        )
    }

    /// Fold one line into the map, following git's `read_mailmap_line`.
    fn read_line(&mut self, line: &[u8]) {
        // Only at the very start of the line: git tests the first byte, so an
        // indented `#` is a (useless, and therefore dropped) contact, not a
        // comment.
        if line.first() == Some(&b'#') {
            return;
        }
        // A line with no usable first pair says nothing, however much text it
        // has on it.
        let Some(first) = parse_contact(line, false) else {
            return;
        };
        // The second pair is what the line matches on, when there is one. An
        // empty email is allowed there — `Proper Name <proper@email.xx> <>`
        // maps the contacts that carry no address at all.
        let second = (!first.rest.is_empty())
            .then(|| parse_contact(first.rest, true))
            .flatten();
        let (new_name, new_email, old_name, old_email) = match second {
            Some(second) => (first.name, Some(first.email), second.name, second.email),
            None => (first.name, None, None, first.email),
        };

        let entry = self.entries.entry(fold(old_email)).or_default();
        match old_name {
            Some(old_name) => {
                entry.by_name.insert(
                    fold(old_name),
                    Replacement {
                        name: new_name.map(<[u8]>::to_vec),
                        email: new_email.map(<[u8]>::to_vec),
                    },
                );
            }
            None => {
                if let Some(new_name) = new_name {
                    entry.simple.name = Some(new_name.to_vec());
                }
                if let Some(new_email) = new_email {
                    entry.simple.email = Some(new_email.to_vec());
                }
            }
        }
    }
}

/// One `Name <email>` read off the front of a line.
struct Contact<'a> {
    /// Absent when there was nothing but whitespace before the `<`.
    name: Option<&'a [u8]>,
    email: &'a [u8],
    /// Whatever followed the `>`, which is where a line's second contact is.
    rest: &'a [u8],
}

/// Read one `Name <email>` from the front of `buf`.
///
/// `allow_empty_email` is git's flag for the second contact on a line: the one
/// a commit is matched on may be `<>`, but the canonical one it is rewritten
/// to may not.
fn parse_contact(buf: &[u8], allow_empty_email: bool) -> Option<Contact<'_>> {
    let left = buf.iter().position(|&b| b == b'<')?;
    let right = left + 1 + buf[left + 1..].iter().position(|&b| b == b'>')?;
    if !allow_empty_email && right == left + 1 {
        return None;
    }
    let name = trim(&buf[..left]);
    Some(Contact {
        name: (!name.is_empty()).then_some(name),
        email: &buf[left + 1..right],
        rest: &buf[right + 1..],
    })
}

/// Whitespace as C's `isspace` sees it in the C locale, which is what git
/// trims names with.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn trim(mut buf: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = buf {
        if !is_space(*first) {
            break;
        }
        buf = rest;
    }
    while let [rest @ .., last] = buf {
        if !is_space(*last) {
            break;
        }
        buf = rest;
    }
    buf
}

/// The form a name or email is keyed and looked up by. git compares with
/// `strcasecmp`, which folds ASCII and nothing else.
fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contact `name <email>` maps to, in the shape `git check-mailmap`
    /// prints it.
    fn map_to(map: &Mailmap, name: &str, email: &str) -> String {
        let (name, email) = map.map(name.as_bytes(), email.as_bytes());
        format!(
            "{} <{}>",
            String::from_utf8_lossy(name),
            String::from_utf8_lossy(email)
        )
    }

    #[test]
    fn test_name_only_line_keeps_the_address() {
        let map = Mailmap::parse(b"Proper Name <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <commit@example.org>"
        );
    }

    #[test]
    fn test_email_only_line_keeps_the_name() {
        let map = Mailmap::parse(b"<proper@example.org> <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Commit Name <proper@example.org>"
        );
    }

    #[test]
    fn test_both_halves_replaced() {
        let map = Mailmap::parse(b"Proper Name <proper@example.org> <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_name_keyed_entry_only_matches_that_name() {
        let map =
            Mailmap::parse(b"Proper Name <proper@example.org> Commit Name <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
        // No entry for the email alone, so a different name is left as it is.
        assert_eq!(
            map_to(&map, "Someone Else", "commit@example.org"),
            "Someone Else <commit@example.org>"
        );
    }

    #[test]
    fn test_name_keyed_entry_wins_over_the_email_only_one() {
        let map = Mailmap::parse(
            b"Plain Name <plain@example.org> <commit@example.org>\n\
              Special Name <special@example.org> Commit Name <commit@example.org>\n",
        );
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Special Name <special@example.org>"
        );
        assert_eq!(
            map_to(&map, "Anyone Else", "commit@example.org"),
            "Plain Name <plain@example.org>"
        );
    }

    #[test]
    fn test_lookup_folds_case_on_both_halves() {
        let map =
            Mailmap::parse(b"Proper Name <proper@example.org> Commit Name <Commit@Example.ORG>\n");
        assert_eq!(
            map_to(&map, "COMMIT NAME", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_a_bare_address_maps_nothing() {
        // An entry exists for the address, but it supplies neither half.
        let map = Mailmap::parse(b"<commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Commit Name <commit@example.org>"
        );
    }

    #[test]
    fn test_two_lines_for_one_address_combine() {
        // Neither line alone rewrites both halves; together they do, because
        // each fills in the half it names.
        let map = Mailmap::parse(
            b"Proper Name <commit@example.org>\n\
              <proper@example.org> <commit@example.org>\n",
        );
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_a_later_name_keyed_line_replaces_the_earlier_one() {
        // Unlike the email-only entries above, these do not combine: the
        // second line's (absent) name is what applies.
        let map = Mailmap::parse(
            b"Proper Name <proper@example.org> Commit Name <commit@example.org>\n\
              <other@example.org> Commit Name <commit@example.org>\n",
        );
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Commit Name <other@example.org>"
        );
    }

    #[test]
    fn test_unmapped_contacts_are_left_alone() {
        let map = Mailmap::parse(b"Proper Name <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Someone", "elsewhere@example.org"),
            "Someone <elsewhere@example.org>"
        );
    }

    #[test]
    fn test_comments_and_junk_are_dropped() {
        let map = Mailmap::parse(
            b"# Proper Name <commit@example.org>\n\
              \n\
              no angle brackets here\n\
              Unclosed <commit@example.org\n\
              <> <commit@example.org>\n",
        );
        assert!(map.is_empty());
    }

    #[test]
    fn test_an_indented_comment_is_not_a_comment() {
        // git looks at the first byte of the line, so this is a contact whose
        // name happens to start with a `#`.
        let map = Mailmap::parse(b" # Proper Name <commit@example.org>\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "# Proper Name <commit@example.org>"
        );
    }

    #[test]
    fn test_names_are_trimmed_and_trailing_text_ignored() {
        let map =
            Mailmap::parse(b"\tProper Name \t<proper@example.org> <commit@example.org> junk\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_crlf_line_endings() {
        // The `\r` lands in the text after the last `>`, which is ignored.
        let map = Mailmap::parse(b"Proper Name <proper@example.org> <commit@example.org>\r\n");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_an_empty_address_can_be_mapped() {
        // The second pair is the only place `<>` is allowed.
        let map = Mailmap::parse(b"Proper Name <proper@example.org> <>\n");
        assert_eq!(
            map_to(&map, "Commit Name", ""),
            "Proper Name <proper@example.org>"
        );
    }

    #[test]
    fn test_a_missing_final_newline_still_parses() {
        let map = Mailmap::parse(b"Proper Name <commit@example.org>");
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            "Proper Name <commit@example.org>"
        );
    }

    #[test]
    fn test_long_lines_are_not_split() {
        // git reads the file in 1024-byte chunks, so it would see this as two
        // lines: a first one ending mid-name with no `<` in it at all, and a
        // second holding the rest. We read the whole line, and map on it.
        let name = "N".repeat(2000);
        let map = Mailmap::parse(format!("{name} <commit@example.org>\n").as_bytes());
        assert_eq!(
            map_to(&map, "Commit Name", "commit@example.org"),
            format!("{name} <commit@example.org>")
        );
    }
}
