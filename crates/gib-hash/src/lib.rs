//! Object IDs and abbreviations, the vocabulary every other `gib` crate shares.
//!
//! See `ARCHITECTURE.md` for what belongs here.

#![deny(clippy::all)]

use gib_parse::ParseResult;
use nom::{
    Parser, bytes::complete::take, character::complete::hex_digit0, combinator::all_consuming,
};

/// The ID of a git object
///
/// `gib` only supports SHA-1 repositories, so this is always 20 bytes or
/// 40 hex characters
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectId {
    /// The object ID as an array of bytes
    pub(crate) bytes: [u8; 20],
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut chars = [0u8; 40];
        hex::encode_to_slice(self.bytes, &mut chars).unwrap();
        write!(f, "{}", str::from_utf8(&chars).unwrap())
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ObjectId").field(&format!("{self}")).finish()
    }
}

impl ObjectId {
    /// The object ID as an array of bytes
    pub fn bytes(&self) -> &[u8; 20] {
        &self.bytes
    }

    /// Construct an [`ObjectId`] from an array of bytes.
    pub const fn from_bytes(id: [u8; 20]) -> Self {
        Self { bytes: id }
    }

    /// Construct an [`ObjectId`] from a hex (byte)string.
    ///
    /// Returns `None` if the provided string was not 40 hexadecimal characters.
    pub fn from_hex(s: &[u8]) -> Option<Self> {
        let (_, oid) = all_consuming(Self::parse).parse(s).ok()?;
        Some(oid)
    }

    /// The all-zero ID, which git uses to mean "no object" on one side of a
    /// diff.
    pub const fn zero() -> Self {
        Self { bytes: [0u8; 20] }
    }

    /// Parse a full hex object ID as part of a larger nom parser.
    pub fn parse(input: &[u8]) -> ParseResult<&[u8], Self> {
        take(40usize)
            .and_then(all_consuming(hex_digit0))
            .map_res(|hex_str| {
                let mut buf = [0u8; 20];
                hex::decode_to_slice(hex_str, &mut buf)?;
                Ok::<ObjectId, hex::FromHexError>(ObjectId::from_bytes(buf))
            })
            .parse(input)
    }
}

/// An abbreviated [`ObjectId`]: a hex prefix naming every object whose ID
/// starts with it.
///
/// Git lets an object be named by any prefix of its hash that is unambiguous,
/// and commit messages conventionally quote 7-12 character abbreviations.
/// `Repo::resolve_prefix` expands one back into a full [`ObjectId`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ObjectIdPrefix {
    /// The prefix's hex characters, packed two per byte and zero-padded to the
    /// width of a full ID, so it can be compared against one directly.
    bytes: [u8; 20],
    /// How many hex characters (nibbles) of `bytes` are significant.
    nibbles: usize,
}

impl ObjectIdPrefix {
    /// Parse an abbreviated object ID from a hex (byte)string.
    ///
    /// Returns `None` unless the input is 4 to 40 hexadecimal characters (of
    /// either case). Four is git's own minimum abbreviation length; anything
    /// shorter is not a useful name for an object, and a resolver would be
    /// searching only to report that it matched everything.
    pub fn from_hex(s: &[u8]) -> Option<Self> {
        if !(4..=40).contains(&s.len()) {
            return None;
        }
        let mut bytes = [0u8; 20];
        for (i, c) in s.iter().enumerate() {
            let nibble = (*c as char).to_digit(16)?;
            // The first character of a pair is the high nibble of its byte.
            bytes[i / 2] |= (nibble as u8) << if i % 2 == 0 { 4 } else { 0 };
        }
        Some(Self {
            bytes,
            nibbles: s.len(),
        })
    }

    /// The number of hex characters in the abbreviation.
    pub fn num_chars(&self) -> usize {
        self.nibbles
    }

    /// The prefix's first byte, which is fully determined because an
    /// abbreviation is at least four characters. Pack index lookups use it to
    /// pick the fanout bucket to search.
    pub fn first_byte(&self) -> u8 {
        self.bytes[0]
    }

    /// The lowest [`ObjectId`] this prefix covers: the prefix with every
    /// remaining nibble zero. Together with [`Self::last`] this bounds a range
    /// scan over an ordered collection of IDs.
    pub fn first(&self) -> ObjectId {
        ObjectId::from_bytes(self.bytes)
    }

    /// The highest [`ObjectId`] this prefix covers: the prefix with every
    /// remaining nibble `f`.
    pub fn last(&self) -> ObjectId {
        let mut bytes = self.bytes;
        let mut full_bytes = self.nibbles / 2;
        if self.nibbles % 2 == 1 {
            // The odd trailing character occupies only the high nibble of its
            // byte, so the low one is still free to grow.
            bytes[full_bytes] |= 0x0f;
            full_bytes += 1;
        }
        for byte in &mut bytes[full_bytes..] {
            *byte = 0xff;
        }
        ObjectId::from_bytes(bytes)
    }

    /// Where `id` sorts relative to the block of IDs this prefix covers:
    /// [`Ordering::Equal`](std::cmp::Ordering::Equal) means `id` *starts with*
    /// the prefix, not that it equals it. Since IDs are compared bytewise and
    /// the covered IDs form a contiguous run, this is a valid ordering to
    /// binary-search a sorted table with.
    pub fn compare(&self, id: &ObjectId) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let full_bytes = self.nibbles / 2;
        match id.bytes[..full_bytes].cmp(&self.bytes[..full_bytes]) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        if self.nibbles % 2 == 1 {
            return (id.bytes[full_bytes] >> 4).cmp(&(self.bytes[full_bytes] >> 4));
        }
        Ordering::Equal
    }

    /// Whether `id` starts with this prefix.
    pub fn matches(&self, id: &ObjectId) -> bool {
        self.compare(id) == std::cmp::Ordering::Equal
    }
}

impl std::fmt::Display for ObjectIdPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let full = format!("{}", self.first());
        write!(f, "{}", &full[..self.nibbles])
    }
}

/// What an abbreviated object ID resolved to.
///
/// Git refuses to guess between objects sharing an abbreviation, and so does
/// this: [`Ambiguous`](Self::Ambiguous) is a distinct outcome from a match, so
/// callers can report it rather than picking one arbitrarily.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrefixResolution {
    /// No known object has this prefix.
    NotFound,
    /// Exactly one object has this prefix.
    Found(ObjectId),
    /// Several objects have this prefix, so it names none of them.
    Ambiguous,
}

impl PrefixResolution {
    /// Combine the results of searching two sources (e.g. two packs). Two
    /// sources agreeing on the same ID is not ambiguity — objects are
    /// content-addressed, so the same ID in two packs is the same object.
    pub fn merge(self, other: Self) -> Self {
        use PrefixResolution::*;
        match (self, other) {
            (Ambiguous, _) | (_, Ambiguous) => Ambiguous,
            (NotFound, result) | (result, NotFound) => result,
            (Found(a), Found(b)) if a == b => Found(a),
            (Found(_), Found(_)) => Ambiguous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    #[test]
    fn parse_object_id_prefix() {
        // Too short (git's own minimum is 4), too long, or not hex.
        assert!(ObjectIdPrefix::from_hex(b"abc").is_none());
        assert!(ObjectIdPrefix::from_hex(&[b'a'; 41]).is_none());
        assert!(ObjectIdPrefix::from_hex(b"abcg").is_none());
        // Both cases are accepted, as for a full ID.
        assert_eq!(
            ObjectIdPrefix::from_hex(b"ABC1").unwrap(),
            ObjectIdPrefix::from_hex(b"abc1").unwrap()
        );
        let prefix = ObjectIdPrefix::from_hex(b"0123abc").unwrap();
        assert_eq!(prefix.num_chars(), 7);
        assert_eq!(format!("{prefix}"), "0123abc");
    }

    #[test]
    fn object_id_prefix_bounds() {
        // An even-length prefix ends on a byte boundary...
        let even = ObjectIdPrefix::from_hex(b"0123ab").unwrap();
        assert_eq!(
            even.first(),
            oid("0123ab0000000000000000000000000000000000")
        );
        assert_eq!(even.last(), oid("0123abffffffffffffffffffffffffffffffffff"));
        // ...an odd-length one leaves the low nibble of its last byte free.
        let odd = ObjectIdPrefix::from_hex(b"0123a").unwrap();
        assert_eq!(odd.first(), oid("0123a00000000000000000000000000000000000"));
        assert_eq!(odd.last(), oid("0123afffffffffffffffffffffffffffffffffff"));
        // A full-length abbreviation covers exactly one ID.
        let full = ObjectIdPrefix::from_hex(&[b'3'; 40]).unwrap();
        assert_eq!(full.first(), full.last());
    }

    #[test]
    fn object_id_prefix_compare() {
        use std::cmp::Ordering;
        let prefix = ObjectIdPrefix::from_hex(b"0123a").unwrap();
        assert!(prefix.matches(&oid("0123a0deadbeef00000000000000000000000000")));
        assert!(prefix.matches(&oid("0123afffffffffffffffffffffffffffffffffff")));
        // Differing inside the odd trailing nibble is still a miss, in the
        // right direction, so a binary search over sorted IDs converges.
        assert_eq!(
            prefix.compare(&oid("01239fffffffffffffffffffffffffffffffffff")),
            Ordering::Less
        );
        assert_eq!(
            prefix.compare(&oid("0123b00000000000000000000000000000000000")),
            Ordering::Greater
        );
        assert_eq!(
            prefix.compare(&oid("0122ffffffffffffffffffffffffffffffffffff")),
            Ordering::Less
        );
    }

    #[test]
    fn prefix_resolution_merge() {
        use PrefixResolution::*;
        let a = oid("0123abcd0123abcd0123abcd0123abcd0123abcd");
        let b = oid("0123abcdffffffffffffffffffffffffffffffff");
        assert_eq!(NotFound.merge(Found(a)), Found(a));
        assert_eq!(Found(a).merge(NotFound), Found(a));
        // The same object packed twice is one object, not an ambiguity.
        assert_eq!(Found(a).merge(Found(a)), Found(a));
        assert_eq!(Found(a).merge(Found(b)), Ambiguous);
        assert_eq!(Ambiguous.merge(NotFound), Ambiguous);
        assert_eq!(NotFound.merge(NotFound), NotFound);
    }
}
