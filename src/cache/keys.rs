//! The two key computations the cache does, kept free-standing and I/O-free
//! so they can be unit-tested off the browser — which is where every other
//! line in this module can only run.

use gib::object::{ObjectId, ObjectIdPrefix, PrefixResolution};
use std::collections::BTreeMap;

/// Resolve an abbreviated SHA against a map keyed by object ID. Sorted keys
/// mean the abbreviation covers one contiguous range, so the answer is the
/// first two entries of that range: none, exactly one, or more than one.
/// Free-standing and I/O-free so it can be unit-tested off the browser.
pub(super) fn resolve_prefix_in_map<T>(
    map: &BTreeMap<ObjectId, T>,
    prefix: &ObjectIdPrefix,
) -> PrefixResolution {
    let mut matches = map.range(prefix.first()..=prefix.last()).map(|(id, _)| *id);
    match (matches.next(), matches.next()) {
        (None, _) => PrefixResolution::NotFound,
        (Some(id), None) => PrefixResolution::Found(id),
        (Some(_), Some(_)) => PrefixResolution::Ambiguous,
    }
}

/// Inclusive key bounds selecting one repo's records in the graph store, whose
/// keys are `"{repo_url}::{oid}"`. The lower bound is the bare prefix; the upper
/// appends U+FFFF, which sorts above every character a hex OID can contain, so
/// the range covers exactly the keys that continue the prefix. IndexedDB compares
/// strings by code point, so this needs no knowledge of the OID length.
/// Free-standing and I/O-free so it can be unit-tested off the browser.
pub(super) fn graph_key_bounds(repo_url: &str) -> (String, String) {
    let prefix = format!("{repo_url}::");
    let upper = format!("{prefix}\u{ffff}");
    (prefix, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object ID written as a short hex string, right-padded with zeroes.
    fn oid(hex: &str) -> ObjectId {
        let mut padded = hex.as_bytes().to_vec();
        padded.resize(40, b'0');
        ObjectId::from_hex(&padded).unwrap()
    }

    fn prefix(hex: &str) -> ObjectIdPrefix {
        ObjectIdPrefix::from_hex(hex.as_bytes()).unwrap()
    }

    /// Stands in for the commit-graph map; only its keys matter here.
    fn map(ids: &[ObjectId]) -> BTreeMap<ObjectId, ()> {
        ids.iter().map(|id| (*id, ())).collect()
    }

    #[test]
    fn resolve_prefix_in_map_unique() {
        let map = map(&[oid("00"), oid("12ab34"), oid("12ff"), oid("ff")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab34")),
            PrefixResolution::Found(oid("12ab34"))
        );
        // An odd-length abbreviation covers half a byte's worth of IDs.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab3")),
            PrefixResolution::Found(oid("12ab34"))
        );
        // The first and last keys of the map.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("0000")),
            PrefixResolution::Found(oid("00"))
        );
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("ff00")),
            PrefixResolution::Found(oid("ff"))
        );
    }

    #[test]
    fn resolve_prefix_in_map_not_found() {
        let map = map(&[oid("00"), oid("12ab34"), oid("ff")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ac")),
            PrefixResolution::NotFound
        );
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("9999")),
            PrefixResolution::NotFound
        );
        assert_eq!(
            resolve_prefix_in_map(&BTreeMap::<ObjectId, ()>::new(), &prefix("12ab34")),
            PrefixResolution::NotFound
        );
    }

    #[test]
    fn resolve_prefix_in_map_ambiguous() {
        let map = map(&[oid("12ab34"), oid("12ab35"), oid("13")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab3")),
            PrefixResolution::Ambiguous
        );
        // The neighbouring `13…` key is outside the range and must not count.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab35")),
            PrefixResolution::Found(oid("12ab35"))
        );
    }

    #[test]
    fn graph_key_bounds_cover_one_repo() {
        let (lower, upper) = graph_key_bounds("https://example.org/a.git");
        let key = |repo: &str| format!("{repo}::{}", oid("12ab34"));
        let in_range = |k: &String| *k >= lower && *k <= upper;

        assert!(in_range(&key("https://example.org/a.git")));
        // A different repo, and one whose URL merely extends ours, are both out.
        assert!(!in_range(&key("https://example.org/b.git")));
        assert!(!in_range(&key("https://example.org/a.github")));
    }
}
