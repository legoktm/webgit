//! Differential tests for the pack reader, against `git verify-pack`.
//!
//! `verify-pack -v` prints git's own view of the index: every object's ID,
//! type, and offset within the packfile. That is exactly what this crate
//! computes, so the two tables can be compared entry for entry — and then each
//! object reconstructed and byte-compared with `git cat-file`.

use crate::{
    IndexedPackFile, find_object_in_pack_index, find_prefix_in_pack_index, form_deltified_chain,
    reconstruct_deltified_object_from_chain,
    test_support::{OpenPack, open_pack},
};
use futures::executor::block_on;
use gib_fs::Offset;
use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};
use gib_object::ObjectType;
use gib_testkit::{TestRepo, get_pack_id, make_basic_repo, make_similar_commits};
use std::collections::BTreeMap;

/// A repository packed into a single pack, deltified hard enough that most
/// objects are reached through a delta chain rather than stored whole.
fn packed_repo() -> TestRepo {
    let test_repo = make_basic_repo().unwrap();
    make_similar_commits(&test_repo).unwrap();
    let root = test_repo.location.path();
    // Near-identical files give the packer plenty to delta against. Each body
    // must still be *distinct*, or git stores one blob for the lot and there is
    // nothing to delta.
    for i in 0..60 {
        let mut body = format!("file {i}\n");
        for line in 0..40 {
            body.push_str(&format!("line {line} of file {}\n", (line + i) % 7));
        }
        std::fs::write(root.join(format!("similar{i}")), body).unwrap();
    }
    test_repo.run_git(["add", "--all"]).unwrap();
    test_repo
        .commit(
            "similar files",
            "a user",
            "an-email-address",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
    test_repo
        .run_git(["repack", "-a", "-d", "-f", "--depth=50", "--window=250"])
        .unwrap();
    test_repo
}

/// One `git verify-pack -v` object line.
struct VerifyEntry {
    object_type: ObjectType,
    offset: u64,
    /// Delta chain depth; 0 for objects stored whole.
    depth: u32,
}

/// Parse `git verify-pack -v`, whose object lines are
/// `<sha1> <type> <size> <size-in-pack> <offset> [<depth> <base-sha1>]`.
fn verify_pack(test_repo: &TestRepo) -> BTreeMap<ObjectId, VerifyEntry> {
    let pack_id = String::from_utf8(get_pack_id(test_repo).unwrap()).unwrap();
    let idx_path = format!(".git/objects/pack/pack-{pack_id}.idx");
    let output = test_repo.run_git(["verify-pack", "-v", &idx_path]).unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Trailing summary lines ("non delta: …", "chain length = …", the
            // pack's own path) have no 40-character hex first field.
            let id = ObjectId::from_hex(fields.first()?.as_bytes())?;
            let object_type = match fields[1] {
                "commit" => ObjectType::Commit,
                "tree" => ObjectType::Tree,
                "blob" => ObjectType::Blob,
                "tag" => ObjectType::Tag,
                other => panic!("unexpected object type {other}"),
            };
            Some((
                id,
                VerifyEntry {
                    object_type,
                    offset: fields[4].parse().unwrap(),
                    depth: fields.get(5).map_or(0, |d| d.parse().unwrap()),
                },
            ))
        })
        .collect()
}

#[test]
fn index_offsets_match_verify_pack() {
    let test_repo = packed_repo();
    let expected = verify_pack(&test_repo);
    assert!(expected.len() > 60);

    let OpenPack {
        fanout,
        offsets,
        mut index,
        ..
    } = open_pack(&test_repo);
    assert_eq!(
        usize::try_from(fanout.total_objects()).unwrap(),
        expected.len(),
        "object count"
    );
    for (id, entry) in &expected {
        let found = block_on(find_object_in_pack_index(
            &fanout,
            Some(&offsets),
            &mut index,
            *id,
        ))
        .unwrap();
        assert_eq!(found, Some(Offset(entry.offset)), "offset for {id}");
    }
}

/// The same lookup without the cached offset table, which makes the search read
/// the offsets out of the index file instead.
#[test]
fn index_offsets_match_without_cached_table() {
    let test_repo = packed_repo();
    let expected = verify_pack(&test_repo);
    let OpenPack {
        fanout, mut index, ..
    } = open_pack(&test_repo);
    for (id, entry) in &expected {
        let found = block_on(find_object_in_pack_index(&fanout, None, &mut index, *id)).unwrap();
        assert_eq!(found, Some(Offset(entry.offset)), "offset for {id}");
    }
}

/// An object ID that is not in the pack must not be reported as found, whatever
/// bucket it would fall into.
#[test]
fn absent_objects_are_not_found() {
    let test_repo = packed_repo();
    let OpenPack {
        fanout,
        offsets,
        mut index,
        ..
    } = open_pack(&test_repo);
    for byte in [0x00u8, 0x7f, 0xff] {
        let mut bytes = [byte; 20];
        bytes[19] = 0xee;
        let found = block_on(find_object_in_pack_index(
            &fanout,
            Some(&offsets),
            &mut index,
            ObjectId::from_bytes(bytes),
        ))
        .unwrap();
        assert_eq!(found, None);
    }
}

#[test]
fn reconstructed_objects_match_cat_file() {
    let test_repo = packed_repo();
    let expected = verify_pack(&test_repo);
    // The fixture must really be delta-heavy, or this only tests whole objects.
    assert!(
        expected.values().filter(|entry| entry.depth > 0).count() > 20,
        "expected a delta-heavy pack"
    );

    let OpenPack {
        fanout,
        offsets,
        index,
        pack,
    } = open_pack(&test_repo);
    let mut indexed = IndexedPackFile {
        fanout: &fanout,
        offsets: Some(&offsets),
        index,
        pack,
    };
    for (id, entry) in &expected {
        let type_name = match entry.object_type {
            ObjectType::Commit => "commit",
            ObjectType::Tree => "tree",
            ObjectType::Blob => "blob",
            ObjectType::Tag => "tag",
        };
        let (chain, object_type, final_object) =
            block_on(form_deltified_chain(&mut indexed, Offset(entry.offset))).unwrap();
        // The chain length is git's reported delta depth.
        assert_eq!(
            u32::try_from(chain.len()).unwrap(),
            entry.depth,
            "chain depth for {id}"
        );
        let body = block_on(reconstruct_deltified_object_from_chain(
            &mut indexed,
            &chain,
            &final_object,
        ))
        .unwrap();
        assert_eq!(object_type, entry.object_type, "type for {id}");
        assert_eq!(
            body,
            test_repo
                .run_git(["cat-file", type_name, &id.to_string()])
                .unwrap(),
            "body for {id}"
        );
    }
}

#[test]
fn prefix_search_matches_rev_parse() {
    let test_repo = packed_repo();
    let ids: Vec<ObjectId> = verify_pack(&test_repo).into_keys().collect();
    let OpenPack {
        fanout, mut index, ..
    } = open_pack(&test_repo);

    for id in ids.iter().step_by(7) {
        let hex = id.to_string();
        let prefix = &hex[..7];
        let candidates: Vec<ObjectId> = String::from_utf8(
            test_repo
                .run_git(["rev-parse", &format!("--disambiguate={prefix}")])
                .unwrap(),
        )
        .unwrap()
        .lines()
        .map(|line| ObjectId::from_hex(line.as_bytes()).unwrap())
        .collect();
        let expected = match candidates.len() {
            0 => PrefixResolution::NotFound,
            1 => PrefixResolution::Found(candidates[0]),
            _ => PrefixResolution::Ambiguous,
        };
        let parsed = ObjectIdPrefix::from_hex(prefix.as_bytes()).unwrap();
        assert_eq!(
            block_on(find_prefix_in_pack_index(&fanout, &mut index, &parsed)).unwrap(),
            expected,
            "prefix {prefix}"
        );
    }
}

#[test]
fn packfile_version_is_accepted() {
    let test_repo = packed_repo();
    let OpenPack { mut pack, .. } = open_pack(&test_repo);
    block_on(crate::validate_packfile_version(&mut pack)).unwrap();
}
