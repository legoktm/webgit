//! Opening a test repository's pack directly, with no repository layer.
//!
//! This crate is handed already-open index and pack files in production, so
//! its tests have to do the opening themselves.

use crate::{FanoutTable, ShortOffsetTable};
use futures::executor::block_on;
use gib_fs::{CachingPageReader, Directory};
use gib_testkit::{TestRepo, TestRepoFile, get_pack_id};

/// A repository's single pack and index, open and ready to read.
pub(crate) struct OpenPack {
    pub fanout: FanoutTable,
    pub offsets: ShortOffsetTable,
    pub index: CachingPageReader<TestRepoFile>,
    pub pack: CachingPageReader<TestRepoFile>,
}

/// Open the one pack of a repository that has been `git gc`'d.
pub(crate) fn open_pack(test_repo: &TestRepo) -> OpenPack {
    let pack_id = String::from_utf8(get_pack_id(test_repo).unwrap()).unwrap();
    let git_dir = test_repo.git_dir();
    let pack_dir = block_on(async {
        let objects = git_dir.open_subdir(b"objects").await.unwrap();
        objects.open_subdir(b"pack").await.unwrap()
    });
    let mut idx_file =
        block_on(pack_dir.open_file(format!("pack-{pack_id}.idx").as_bytes())).unwrap();
    let pack_file =
        block_on(pack_dir.open_file(format!("pack-{pack_id}.pack").as_bytes())).unwrap();
    let fanout = block_on(FanoutTable::load(&mut idx_file)).unwrap();
    let offsets = block_on(ShortOffsetTable::load(
        &mut idx_file,
        fanout.total_objects(),
    ))
    .unwrap();
    OpenPack {
        fanout,
        offsets,
        index: CachingPageReader::new(idx_file),
        pack: CachingPageReader::new(pack_file),
    }
}
