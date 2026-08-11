use crate::ObjectId;
use gib_parse::{ParseError, ParseResult, SubsliceRange};
use nom::{
    Parser,
    bytes::complete::{take, take_till, take_while1},
    character::complete::char,
    combinator::{all_consuming, map_opt},
    multi::many,
    sequence::terminated,
};
use std::{fmt::Debug, iter::FusedIterator, ops::Range};

/// The type of an entry in a tree
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
pub enum TreeEntryType {
    /// A non-executable file pointing to a blob
    File,
    /// An executable file pointing to a blob
    Executable,
    /// A symbolic link
    ///
    /// Symbolic links in git are encoded as a tree entry of type symlink
    /// pointing to a blob. The blob's content is the path of the symlink
    /// target.
    Symlink,
    /// A sub-tree, i.e. a subdirectory
    Tree,
    /// A pointer to a commit
    ///
    /// This is used for git submodules.
    Commit,
}

/// An entry in a tree object
///
/// It holds a reference to the data in the [`Tree`].
#[derive(Clone, PartialEq, Eq)]
pub struct TreeEntry<'a> {
    /// The name of the tree entry
    name: &'a [u8],

    /// The type of the tree entry
    entry_type: TreeEntryType,

    /// The [`ObjectId`] that the entry points to
    id: ObjectId,
}

impl<'a> TreeEntry<'a> {
    /// The name of the tree entry
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    /// The type of the tree entry
    pub fn entry_type(&self) -> TreeEntryType {
        self.entry_type
    }

    /// The [`ObjectId`] that the entry points to
    pub fn id(&self) -> ObjectId {
        self.id
    }
}

#[derive(Clone)]
struct RangeTreeEntry {
    name: Range<usize>,
    entry_type: TreeEntryType,
    id: ObjectId,
}

/// Interpret a tree entry's octal mode, the way git canonicalizes modes when
/// it reads a tree.
///
/// Only the five modes git writes today are truly canonical, but old (and
/// `--literally`-written) trees carry variations that every git command still
/// reads: group-writable `100664` from pre-2008 git, `100640`, or a
/// zero-padded `040000` for a subdirectory. Rejecting those would make the
/// whole tree — and every diff touching it — unreadable, so take the object
/// type from the high bits and normalize the permission bits away. Modes whose
/// high bits name no object type at all are still an error.
fn entry_type_from_mode(mode: &[u8]) -> Option<TreeEntryType> {
    let mut value: u32 = 0;
    for digit in mode {
        value = value
            .checked_mul(8)?
            .checked_add(char::from(*digit).to_digit(8)?)?;
    }
    // The octal file type (`S_IFMT`) and permission bits of stat(2).
    match value & 0o170000 {
        0o100000 => Some(if value & 0o100 == 0 {
            TreeEntryType::File
        } else {
            TreeEntryType::Executable
        }),
        0o120000 => Some(TreeEntryType::Symlink),
        0o040000 => Some(TreeEntryType::Tree),
        0o160000 => Some(TreeEntryType::Commit),
        _ => None,
    }
}

impl RangeTreeEntry {
    fn parser(body: &[u8]) -> impl Fn(&[u8]) -> ParseResult<&[u8], Self> {
        |input: &[u8]| {
            let entry_type_parser = map_opt(
                take_while1(|c: u8| c.is_ascii_digit()),
                entry_type_from_mode,
            );
            let mut p = (
                terminated(entry_type_parser, char(' ')),
                terminated(take_till(|c| c == b'\0'), char('\0')),
                take(20usize)
                    .map(|bytes| ObjectId::from_bytes(<[u8; 20]>::try_from(bytes).unwrap())),
            );
            let (rest, (entry_type, name, id)) = p.parse(input)?;
            Ok((
                rest,
                RangeTreeEntry {
                    name: body.subslice_range_stable(name).unwrap(),
                    entry_type,
                    id,
                },
            ))
        }
    }
}

/// An iterator over the entries in a tree object
pub struct TreeEntryIter<'a> {
    body: &'a [u8],
    entries: &'a [RangeTreeEntry],
    pos: usize,
}

impl<'a> Iterator for TreeEntryIter<'a> {
    type Item = TreeEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.entries.get(self.pos)?;
        self.pos += 1;
        Some(TreeEntry {
            name: &self.body[entry.name.clone()],
            entry_type: entry.entry_type,
            id: entry.id,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            self.entries.len() - self.pos,
            Some(self.entries.len() - self.pos),
        )
    }
}

impl FusedIterator for TreeEntryIter<'_> {}
impl ExactSizeIterator for TreeEntryIter<'_> {}

/// A tree object
#[derive(Clone)]
pub struct Tree {
    /// The [`ObjectId`] of the tree
    id: ObjectId,

    /// The raw data in the object
    body: Vec<u8>,

    entries: Vec<RangeTreeEntry>,
}

impl PartialEq for Tree {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Tree {}
impl PartialOrd for Tree {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Tree {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl Tree {
    /// The [`ObjectId`] of the tree
    pub fn id(&self) -> ObjectId {
        self.id
    }

    /// The raw data in the object
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Get an iterator over the entries in the tree.
    pub fn entries(&self) -> TreeEntryIter<'_> {
        TreeEntryIter {
            body: self.body.as_slice(),
            entries: self.entries.as_slice(),
            pos: 0,
        }
    }

    pub(crate) fn parse(id: ObjectId, body: Vec<u8>) -> Result<Self, ParseError> {
        let (_, entries): (_, Vec<_>) =
            all_consuming(many(0.., RangeTreeEntry::parser(&body))).parse(&body)?;
        Ok(Self { id, body, entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gib_testkit::TestRepo;
    use hex_literal::hex;

    const ZERO_OID: ObjectId = ObjectId::from_bytes([0; 20]);

    #[test]
    fn parse_tree() {
        let mut data = Vec::new();
        data.extend_from_slice(b"40000 a-directory\0");
        data.extend_from_slice(&hex!("3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb"));
        data.extend_from_slice(b"100644 a-file\0");
        data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        data.extend_from_slice(b"120000 a-symlink\0");
        data.extend_from_slice(&hex!("7c35e066a9001b24677ae572214d292cebc55979"));
        data.extend_from_slice(b"100755 an-executable-file\0");
        data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        data.extend_from_slice(b"160000 a-commit\0");
        data.extend_from_slice(&hex!("91ca81cfccb6f88a34807e9810bb0be409f32d70"));
        let tree = Tree::parse(ZERO_OID, data).unwrap();
        let entries = tree.entries();
        assert_eq!(entries.len(), 5);
        let expected = [
            (
                TreeEntryType::Tree,
                ObjectId::from_bytes(hex!("3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb")),
                b"a-directory".as_slice(),
            ),
            (
                TreeEntryType::File,
                ObjectId::from_bytes(hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")),
                b"a-file".as_slice(),
            ),
            (
                TreeEntryType::Symlink,
                ObjectId::from_bytes(hex!("7c35e066a9001b24677ae572214d292cebc55979")),
                b"a-symlink".as_slice(),
            ),
            (
                TreeEntryType::Executable,
                ObjectId::from_bytes(hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")),
                b"an-executable-file".as_slice(),
            ),
            (
                TreeEntryType::Commit,
                ObjectId::from_bytes(hex!("91ca81cfccb6f88a34807e9810bb0be409f32d70")),
                b"a-commit".as_slice(),
            ),
        ];
        for (received, (entry_type, id, name)) in entries.zip(expected) {
            assert_eq!(received.entry_type(), entry_type);
            assert_eq!(received.id(), id);
            assert_eq!(received.name(), name);
        }
    }

    /// A single legacy mode must not cost us the rest of the tree: the whole
    /// object is parsed at once, so rejecting one entry loses every entry.
    #[test]
    fn parse_tree_with_legacy_modes() {
        let mut data = Vec::new();
        // Zero-padded, as `ls-tree` prints subdirectories.
        data.extend_from_slice(b"040000 a-directory\0");
        data.extend_from_slice(&hex!("3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb"));
        // Group-writable, as git wrote before it narrowed the modes it honors.
        data.extend_from_slice(b"100664 a-file\0");
        data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        data.extend_from_slice(b"100640 another-file\0");
        data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        data.extend_from_slice(b"100775 an-executable-file\0");
        data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        data.extend_from_slice(b"120777 a-symlink\0");
        data.extend_from_slice(&hex!("7c35e066a9001b24677ae572214d292cebc55979"));
        let tree = Tree::parse(ZERO_OID, data).unwrap();
        let types: Vec<TreeEntryType> = tree.entries().map(|entry| entry.entry_type()).collect();
        assert_eq!(
            types,
            [
                TreeEntryType::Tree,
                TreeEntryType::File,
                TreeEntryType::File,
                TreeEntryType::Executable,
                TreeEntryType::Symlink,
            ]
        );
    }

    /// Tolerating legacy permission bits is not tolerating anything: a mode
    /// whose high bits name no object type is still a broken tree.
    #[test]
    fn parse_tree_rejects_nonsense_modes() {
        for mode in [
            b"70000".as_slice(),    // No such file type.
            b"0".as_slice(),        // Ditto, and what a truncated mode looks like.
            b"100689".as_slice(),   // Not octal.
            b"10064400".as_slice(), // Shifted past the file type bits.
        ] {
            let mut data = Vec::new();
            data.extend_from_slice(mode);
            data.extend_from_slice(b" a-file\0");
            data.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
            assert!(
                Tree::parse(ZERO_OID, data).is_err(),
                "mode {} should not parse",
                str::from_utf8(mode).unwrap()
            );
        }
    }

    /// Legacy modes are worth accepting because git accepts them, so check the
    /// mapping against what the host's `git ls-tree` reports for a tree
    /// written with them.
    #[test]
    fn legacy_modes_match_ls_tree() {
        let test_repo = TestRepo::new().unwrap();
        let path = |name: &str| test_repo.location.path().join(name);
        // `--literally` is the only way to get a legacy mode into a tree: git's
        // own tree writers refuse to produce one. `hash-object` reads a path
        // rather than stdin, which `run_git` closes.
        let write_object = |object_type: &str, name: &str| {
            ObjectId::from_hex(
                test_repo
                    .run_git([
                        "hash-object",
                        "-t",
                        object_type,
                        "-w",
                        "--literally",
                        path(name).to_str().unwrap(),
                    ])
                    .unwrap()
                    .trim_ascii_end(),
            )
            .unwrap()
        };
        std::fs::write(path("empty"), b"").unwrap();
        let blob = write_object("blob", "empty");
        let subtree = write_object("tree", "empty");

        let mut body = Vec::new();
        // In tree order, which sorts a subdirectory as if its name ended in a
        // slash.
        for (mode, name, id) in [
            (b"040000".as_slice(), "a-directory", subtree),
            (b"100640".as_slice(), "a-group-readable-file", blob),
            (b"100664".as_slice(), "a-group-writable-file", blob),
            (b"120777".as_slice(), "a-symlink", blob),
            (b"100775".as_slice(), "an-executable-file", blob),
        ] {
            body.extend_from_slice(mode);
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(id.bytes());
        }
        std::fs::write(path("legacy-tree"), &body).unwrap();
        let id = write_object("tree", "legacy-tree");

        let tree = Tree::parse(
            id,
            test_repo
                .run_git(["cat-file", "tree", &id.to_string()])
                .unwrap(),
        )
        .unwrap();
        let actual: Vec<String> = tree
            .entries()
            .map(|entry| {
                let (mode, kind) = match entry.entry_type() {
                    TreeEntryType::File => ("100644", "blob"),
                    TreeEntryType::Executable => ("100755", "blob"),
                    TreeEntryType::Symlink => ("120000", "blob"),
                    TreeEntryType::Tree => ("040000", "tree"),
                    TreeEntryType::Commit => ("160000", "commit"),
                };
                format!(
                    "{mode} {kind} {}\t{}",
                    entry.id(),
                    str::from_utf8(entry.name()).unwrap()
                )
            })
            .collect();
        let expected =
            String::from_utf8(test_repo.run_git(["ls-tree", &id.to_string()]).unwrap()).unwrap();
        assert_eq!(actual, expected.lines().collect::<Vec<&str>>());
    }
}
