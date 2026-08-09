use crate::{
    error::{GResult, annotate_pack_object_error},
    file_system::{Directory, FileSystem, Offset},
    object::{ObjectId, ObjectIdPrefix, PrefixResolution},
    object_store::{RawObject, cache::IndexCache, loose::read_loose_object},
    repo::Repo,
};
use alloc::vec::Vec;
use gib_fs::CachingPageReader;
use gib_pack::{
    IndexedPackFile, find_object_in_pack_index, find_prefix_in_pack_index, form_deltified_chain,
    reconstruct_deltified_object_from_chain,
};

#[derive(Clone)]
pub(crate) struct PackName {
    pub(crate) index_filename: Vec<u8>,
    pub(crate) pack_filename: Vec<u8>,
}

impl PackName {
    pub(crate) fn new(filename: Vec<u8>) -> Option<Self> {
        let stripped = filename.strip_suffix(b".idx")?;
        let mut pack_filename = Vec::with_capacity(filename.len() + 1);
        pack_filename.extend_from_slice(stripped);
        pack_filename.extend_from_slice(b".pack");
        Some(Self {
            index_filename: filename,
            pack_filename,
        })
    }

    pub(crate) fn from_pack_filename(filename: Vec<u8>) -> Option<Self> {
        let stripped = filename.strip_suffix(b".pack")?;
        let mut index_filename = Vec::with_capacity(filename.len());
        index_filename.extend_from_slice(stripped);
        index_filename.extend_from_slice(b".idx");
        Some(Self {
            index_filename,
            pack_filename: filename,
        })
    }
}

pub(crate) async fn lookup<F: FileSystem>(
    repo: &Repo<F>,
    id: ObjectId,
) -> GResult<Option<RawObject>> {
    // Look in packs first, falling back to loose objects only on a miss. Most
    // objects are packed, so probing loose first would mean a guaranteed-404
    // request per lookup on a packed repo. A loose object (e.g. from a recent
    // push) is still found by the fallback, and since git objects are
    // content-addressed a packed copy is byte-identical to any loose copy, so
    // the order does not affect correctness.
    let pack_cache = &repo.index_cache;
    if let Some((mut indexed_pack, offset)) = find_packed_object(repo, pack_cache, id).await? {
        let (chain, object_type, final_object) = form_deltified_chain(&mut indexed_pack, offset)
            .await
            .map_err(annotate_pack_object_error(id))?;
        let body =
            reconstruct_deltified_object_from_chain(&mut indexed_pack, &chain, &final_object)
                .await
                .map_err(annotate_pack_object_error(id))?;
        return Ok(Some(RawObject { object_type, body }));
    }
    read_loose_object(repo, id).await
}

/// Expand an abbreviated object ID by searching every pack index.
///
/// Loose objects are not considered: finding one would mean knowing its full
/// ID already (they are stored at `objects/ab/cdef…`, and a directory listing
/// is not available over dumb HTTP — the transport this library exists to
/// serve). Packed objects are the overwhelming majority in any repository that
/// has been gc'd, and are the ones a published repository serves.
pub(crate) async fn resolve_prefix<F: FileSystem>(
    repo: &Repo<F>,
    prefix: &ObjectIdPrefix,
) -> GResult<PrefixResolution> {
    let mut resolution = PrefixResolution::NotFound;
    for pack in &repo.index_cache.indexes {
        let idx_file = repo.pack_dir.open_file(&pack.name.index_filename).await?;
        // Share the pack's persistent index page cache, as object lookups do:
        // the binary search walks the same pages they do.
        let mut idx_file = CachingPageReader::with_cache(idx_file, pack.idx_pages.clone());
        resolution =
            resolution.merge(find_prefix_in_pack_index(&pack.fanout, &mut idx_file, prefix).await?);
        if resolution == PrefixResolution::Ambiguous {
            // No later pack can un-ambiguate it, so stop reading.
            break;
        }
    }
    Ok(resolution)
}

pub(crate) async fn find_packed_object<'p, F: FileSystem>(
    repo: &Repo<F>,
    pack_cache: &'p IndexCache,
    id: ObjectId,
) -> GResult<Option<(IndexedPackFile<'p, F::File>, Offset)>> {
    for pack in &pack_cache.indexes {
        let idx_file = repo.pack_dir.open_file(&pack.name.index_filename).await?;
        // Reuse the pack's persistent index page cache so binary-search reads
        // are shared across lookups; the pack body reader stays per-lookup
        // since body pages have little cross-lookup reuse.
        let mut idx_file = CachingPageReader::with_cache(idx_file, pack.idx_pages.clone());
        if let Some(offset) =
            find_object_in_pack_index(&pack.fanout, pack.offsets.as_ref(), &mut idx_file, id)
                .await?
        {
            let pack_file = repo.pack_dir.open_file(&pack.name.pack_filename).await?;
            return Ok(Some((
                IndexedPackFile {
                    fanout: &pack.fanout,
                    offsets: pack.offsets.as_ref(),
                    index: idx_file,
                    pack: CachingPageReader::new(pack_file),
                },
                offset,
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    //! These exercise `lookup` end to end — pack discovery, index search, and
    //! delta reconstruction — so they live here rather than in `gib-pack`,
    //! which knows nothing about repositories.

    use std::fs::{create_dir, rename};

    use crate::{
        object::{ObjectId, ObjectType},
        object_store::lookup::lookup,
        repo::RepoConfig,
        test::open_test_repo,
    };
    use futures::executor::block_on;
    use gib_testkit::{TestFileSystem, make_basic_repo, make_packfile_repo, make_similar_commits};
    use hex_literal::hex;

    #[test]
    fn read_non_deltified_commit() {
        let test_repo = make_packfile_repo().unwrap();
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"78dc5b70bd81aa46ec7dfce87a69826e354a916b").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Commit);
        let expected_body = b"tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb
author a user <an-email-address> 946684800 +0000
committer a user <an-email-address> 946684800 +0000

a commit
";
        assert_eq!(raw_object.body, expected_body);
    }

    #[test]
    fn read_non_deltified_blob() {
        let test_repo = make_packfile_repo().unwrap();
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Blob);
        assert_eq!(raw_object.body, b"");
    }

    #[test]
    fn read_non_deltified_tree() {
        let test_repo = make_packfile_repo().unwrap();
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Tree);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"100644 a-file\0");
        expected.extend_from_slice(&hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        assert_eq!(raw_object.body, expected);
    }

    #[test]
    fn read_non_deltified_tag() {
        let test_repo = make_packfile_repo().unwrap();
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"fbb9ae04dfa95dc527c1e6dde722f9048c5262ef").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Tag);
        assert_eq!(
            raw_object.body,
            b"object 78dc5b70bd81aa46ec7dfce87a69826e354a916b
type commit
tag a-fat-tag
tagger a user <an-email-address> 946684800 +0000

a tag
"
        );
    }

    #[test]
    fn reconstruct_chained_deltified_object() {
        let test_repo = make_basic_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"9cded1c631096bb2caf71e1f2e0765bf6420d040").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Tree);
        assert_eq!(raw_object.body, similar_commits_tree_body());
    }

    #[test]
    fn ref_delta() {
        let test_repo = make_packfile_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
        let objects_dir = test_repo.location.path().join(".git").join("objects");
        create_dir(objects_dir.join("pack-new")).unwrap();
        let mut git_process = test_repo
            .git_command()
            .current_dir(objects_dir.join("pack-new"))
            .args([
                "pack-objects",
                "--revs",
                "--no-reuse-delta",
                "--all",
                "pack-refdelta",
                // --delta-base-offset is off by default, which is what we want
            ])
            .spawn()
            .unwrap();
        assert!(git_process.wait().unwrap().success());
        rename(objects_dir.join("pack"), objects_dir.join("pack-old")).unwrap();
        rename(objects_dir.join("pack-new"), objects_dir.join("pack")).unwrap();
        // The earlier `git gc` left an objects/info/packs naming the old pack;
        // refresh it so pack discovery (which prefers the manifest) finds the
        // swapped-in pack.
        test_repo.run_git(["update-server-info"]).unwrap();
        assert!(test_repo.run_git(["rev-parse", "HEAD^"]).is_ok());
        let raw_object = block_on(lookup(
            &open_test_repo(&test_repo),
            ObjectId::from_hex(b"9cded1c631096bb2caf71e1f2e0765bf6420d040").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Tree);
        let expected = similar_commits_tree_body();
        assert_eq!(raw_object.body.len(), expected.len());
        assert_eq!(raw_object.body, expected);
    }

    /// The tree `make_similar_commits` leaves at HEAD: `a-file` plus `a`..`z`
    /// minus the two files it deletes.
    fn similar_commits_tree_body() -> Vec<u8> {
        const EMPTY_BLOB: [u8; 20] = hex!("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"100644 a\0");
        expected.extend_from_slice(&EMPTY_BLOB);
        expected.extend_from_slice(b"100644 a-file\0");
        expected.extend_from_slice(&EMPTY_BLOB);
        for c in b'b'..=b'z' {
            if c != b'm' && c != b't' {
                expected.extend_from_slice(b"100644 ");
                expected.push(c);
                expected.push(b'\0');
                expected.extend_from_slice(&EMPTY_BLOB);
            }
        }
        expected
    }

    #[test]
    fn read_object_no_offset_cache() {
        let test_repo = make_packfile_repo().unwrap();
        let repo = block_on(
            RepoConfig::default()
                .index_offset_cache_max(0)
                .open::<TestFileSystem>(test_repo.git_dir()),
        )
        .unwrap();
        let raw_object = block_on(lookup(
            &repo,
            ObjectId::from_hex(b"78dc5b70bd81aa46ec7dfce87a69826e354a916b").unwrap(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw_object.object_type, ObjectType::Commit);
        let expected_body = b"tree 3a4df67dd7fd7cb3ca82d9896dbdd28053d39bdb
author a user <an-email-address> 946684800 +0000
committer a user <an-email-address> 946684800 +0000

a commit
";
        assert_eq!(raw_object.body, expected_body);
    }
}
