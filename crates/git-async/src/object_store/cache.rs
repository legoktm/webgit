use crate::{
    error::GResult,
    file_system::{Directory, File},
    object_store::lookup::PackName,
    repo::RepoConfig,
};
use alloc::vec::Vec;
use gib_fs::{PageCache, new_page_cache};
use gib_pack::{FanoutTable, ShortOffsetTable, validate_packfile_version};

/// A single pack's index metadata, plus a page cache for its `.idx` file that
/// persists across object lookups so the binary-search reads are not re-fetched
/// every time.
#[derive(Clone)]
pub(crate) struct PackIndex {
    pub name: PackName,
    pub fanout: FanoutTable,
    pub offsets: Option<ShortOffsetTable>,
    pub idx_pages: PageCache,
}

#[derive(Clone)]
pub(crate) struct IndexCache {
    pub indexes: Vec<PackIndex>,
}

impl IndexCache {
    pub async fn new<F: File, D: Directory<F>>(
        pack_dir: &D,
        pack_ids: Vec<PackName>,
        config: &RepoConfig,
    ) -> GResult<Self> {
        let mut indexes = Vec::with_capacity(pack_ids.len());
        let mut objects_in_offset_cache: usize = 0;
        let max_objects_in_offset_cache = config.index_offset_cache_max / size_of::<u32>();
        for pack_id in pack_ids {
            let mut file = pack_dir.open_file(&pack_id.index_filename).await?;
            let fanout = FanoutTable::load(&mut file).await?;
            let index_objects: usize = fanout.total_objects().try_into().unwrap();
            let offset_table =
                if objects_in_offset_cache + index_objects < max_objects_in_offset_cache {
                    objects_in_offset_cache += index_objects;
                    Some(ShortOffsetTable::load(&mut file, fanout.total_objects()).await?)
                } else {
                    None
                };
            let mut pack_file = pack_dir.open_file(&pack_id.pack_filename).await?;
            validate_packfile_version(&mut pack_file).await?;
            indexes.push(PackIndex {
                name: pack_id,
                fanout,
                offsets: offset_table,
                idx_pages: new_page_cache(),
            });
        }
        Ok(Self { indexes })
    }
}
