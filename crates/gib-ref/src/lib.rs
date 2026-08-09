//! Types and parsers for git refs
//!
//! A git ref is a file which points to an object or to another ref. This crate
//! knows the *formats* — `HEAD`, loose ref files, `packed-refs`, and the
//! `info/refs` snapshot written by `git update-server-info` — but nothing
//! about where they live. Resolving a ref against a real repository, including
//! following symrefs and letting loose refs shadow packed ones, is the
//! facade's job.

#![deny(clippy::all)]

use accessory::Accessors;
use gib_hash::ObjectId;
use gib_parse::ParseResult;
use nom::{
    Parser,
    branch::alt,
    bytes::complete::{tag, take_till},
    character::complete::{char, newline, not_line_ending, space0},
    combinator::{all_consuming, opt},
    multi::many0,
    sequence::{delimited, preceded, terminated},
};

/// A ref listing did not parse.
#[derive(Debug, PartialEq, Eq)]
pub enum RefError {
    /// A `packed-refs` file was not in the expected format.
    MalformedPackedRefs,
    /// An `info/refs` file was not in the expected format.
    MalformedInfoRefs,
}

/// The name of a git ref
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum RefName {
    /// The head of a repo, which is called `HEAD` by git.
    Head,
    /// A non-HEAD ref such as `v1.7.0` or `origin/main`.
    Ref(Vec<u8>),
}

/// A ref resolved during bulk listing, as returned by `Repo::all_refs`
#[derive(Accessors, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefEntry {
    /// The object the ref points to directly
    #[access(get(cp))]
    pub target: ObjectId,

    /// For annotated tags, the commit the tag object points to, when the
    /// source (`info/refs` or `packed-refs`) recorded a peeled entry
    #[access(get(cp))]
    pub peeled: Option<ObjectId>,
}

impl RefEntry {
    /// The object ID to use when treating this ref as a commit: the peeled
    /// target if one was recorded, otherwise the direct target.
    pub fn commit_target(&self) -> ObjectId {
        self.peeled.unwrap_or(self.target)
    }
}

/// The contents of a git ref
#[derive(Accessors, Clone)]
pub struct Ref {
    /// The name of the ref
    #[access(get)]
    name: RefName,

    /// The target of the ref
    ///
    /// Refs can be either direct (pointing to an object) or symbolic (pointing
    /// to another ref).
    #[access(get)]
    target: RefTarget,
}

impl Ref {
    /// Build a ref from a name and the target it was found to have.
    ///
    /// Finding that target is the caller's job: it means reading a loose ref
    /// file, `packed-refs`, or `info/refs`, all of which need a filesystem.
    pub fn new(name: RefName, target: RefTarget) -> Self {
        Self { name, target }
    }
}

/// The target of a git ref
///
/// Refs can be either direct (pointing to an object) or symbolic (pointing to
/// another ref).
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum RefTarget {
    /// A direct ref, pointing to an object
    Direct(ObjectId),
    /// A symbolic ref, pointing to another ref
    Symbolic(RefName),
}

impl RefTarget {
    /// Parse the contents of a loose ref file (or `HEAD`): either a full object
    /// ID or `ref: refs/…`.
    pub fn parse_loose_ref(content: &[u8]) -> ParseResult<&[u8], Self> {
        all_consuming(terminated(not_line_ending, newline))
            .and_then(alt((
                ObjectId::parse.map(RefTarget::Direct),
                preceded(
                    tag("ref: refs/"),
                    take_till(|_| false)
                        .map(|name: &[u8]| RefTarget::Symbolic(RefName::Ref(name.to_vec()))),
                ),
            )))
            .parse(content)
    }
}

/// Parse a `packed-refs` file.
pub fn parse_packed_refs(packed_refs_data: &[u8]) -> Result<Vec<(RefName, RefEntry)>, RefError> {
    let parse_one_ref = (
        terminated(ObjectId::parse, char(' ')),
        delimited(
            tag("refs/"),
            not_line_ending.map(|name: &[u8]| RefName::Ref(name.to_vec())),
            newline,
        ),
        // A `^<oid>` line records the commit an annotated tag peels to.
        opt(delimited(char('^'), ObjectId::parse, newline)),
    )
        .map(|(target, name, peeled)| Some((name, RefEntry { target, peeled })));
    let parse_comment = (space0, char('#'), not_line_ending, opt(newline)).map(|_| None);
    let mut parser = all_consuming(many0(alt((parse_one_ref, parse_comment))));
    let (_, refs) = parser
        .parse(packed_refs_data)
        .map_err(|_| RefError::MalformedPackedRefs)?;
    Ok(refs.into_iter().flatten().collect())
}

/// Parse the `info/refs` file written by `git update-server-info`.
///
/// Each line is `<oid>\t<refname>`; a `<refname>^{}` line records the commit
/// the preceding annotated tag peels to.
pub fn parse_info_refs(data: &[u8]) -> Result<Vec<(RefName, RefEntry)>, RefError> {
    let parse_one_line = (
        terminated(ObjectId::parse, char('\t')),
        delimited(tag("refs/"), not_line_ending, newline),
    );
    let mut parser = all_consuming(many0(parse_one_line));
    let (_, lines) = parser
        .parse(data)
        .map_err(|_| RefError::MalformedInfoRefs)?;
    let mut refs: Vec<(RefName, RefEntry)> = Vec::new();
    for (oid, name) in lines {
        if let Some(base) = name.strip_suffix(b"^{}") {
            if let Some((last_name, last_entry)) = refs.last_mut()
                && *last_name == RefName::Ref(base.to_vec())
            {
                last_entry.peeled = Some(oid);
            }
            continue;
        }
        refs.push((
            RefName::Ref(name.to_vec()),
            RefEntry {
                target: oid,
                peeled: None,
            },
        ));
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn parse_direct_ref() {
        let content = b"6121d0b97779278fcc32cc8a02754e7c588d9c18\n";
        let (_, parsed) = RefTarget::parse_loose_ref(content).unwrap();
        assert_eq!(
            parsed,
            RefTarget::Direct(ObjectId::from_bytes(hex!(
                "6121d0b97779278fcc32cc8a02754e7c588d9c18"
            )))
        );
    }

    #[test]
    fn parse_symbolic_ref() {
        let content = b"ref: refs/heads/main\n";
        let (_, parsed) = RefTarget::parse_loose_ref(content).unwrap();
        assert_eq!(
            parsed,
            RefTarget::Symbolic(RefName::Ref(b"heads/main".to_vec()))
        );
    }

    #[test]
    fn parse_packed_refs_with_peeled() {
        let data = b"# pack-refs with: peeled fully-peeled sorted \n\
6121d0b97779278fcc32cc8a02754e7c588d9c18 refs/heads/main\n\
21810577ec46dcb1623e1a1c1e8fe55ed3151118 refs/tags/fat-tag\n\
^6121d0b97779278fcc32cc8a02754e7c588d9c18\n";
        let refs = parse_packed_refs(data).unwrap();
        assert_eq!(refs.len(), 2);
        let commit = ObjectId::from_bytes(hex!("6121d0b97779278fcc32cc8a02754e7c588d9c18"));
        let tag = ObjectId::from_bytes(hex!("21810577ec46dcb1623e1a1c1e8fe55ed3151118"));
        assert_eq!(refs[0].0, RefName::Ref(b"heads/main".to_vec()));
        assert_eq!(
            refs[0].1,
            RefEntry {
                target: commit,
                peeled: None
            }
        );
        assert_eq!(refs[1].0, RefName::Ref(b"tags/fat-tag".to_vec()));
        assert_eq!(
            refs[1].1,
            RefEntry {
                target: tag,
                peeled: Some(commit)
            }
        );
    }

    #[test]
    fn parse_info_refs_with_peeled() {
        let data = b"6121d0b97779278fcc32cc8a02754e7c588d9c18\trefs/heads/main\n\
21810577ec46dcb1623e1a1c1e8fe55ed3151118\trefs/tags/fat-tag\n\
6121d0b97779278fcc32cc8a02754e7c588d9c18\trefs/tags/fat-tag^{}\n";
        let refs = parse_info_refs(data).unwrap();
        assert_eq!(refs.len(), 2);
        let commit = ObjectId::from_bytes(hex!("6121d0b97779278fcc32cc8a02754e7c588d9c18"));
        let tag = ObjectId::from_bytes(hex!("21810577ec46dcb1623e1a1c1e8fe55ed3151118"));
        assert_eq!(refs[0].0, RefName::Ref(b"heads/main".to_vec()));
        assert_eq!(
            refs[0].1,
            RefEntry {
                target: commit,
                peeled: None
            }
        );
        assert_eq!(refs[1].0, RefName::Ref(b"tags/fat-tag".to_vec()));
        assert_eq!(
            refs[1].1,
            RefEntry {
                target: tag,
                peeled: Some(commit)
            }
        );
    }

    #[test]
    fn parse_info_refs_malformed() {
        assert_eq!(
            parse_info_refs(b"not a refs file\n"),
            Err(RefError::MalformedInfoRefs)
        );
    }
}

#[cfg(test)]
mod differential;
