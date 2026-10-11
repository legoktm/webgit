use crate::{OdbResult, PackName};
use gib_fs::{Directory, File, PageCache, new_page_cache};
use gib_pack::{FanoutTable, ShortOffsetTable, validate_packfile_version};

/// The most of one pack's `.pack` file kept in memory between lookups.
const PACK_PAGE_CACHE_MAX: usize = 8 * 1024 * 1024;

/// A single pack's index metadata, plus page caches for its `.idx` and `.pack`
/// files that persist across object lookups, so neither the binary-search
/// reads nor objects packed next to ones already read are re-fetched.
#[derive(Clone)]
pub(crate) struct PackIndex {
    pub name: PackName,
    pub fanout: FanoutTable,
    pub offsets: Option<ShortOffsetTable>,
    pub idx_pages: PageCache,
    pub pack_pages: PageCache,
}

#[derive(Clone)]
pub(crate) struct IndexCache {
    pub indexes: Vec<PackIndex>,
}

impl IndexCache {
    pub async fn new<F: File, D: Directory<F>>(
        pack_dir: &D,
        pack_ids: Vec<PackName>,
        index_offset_cache_max: usize,
    ) -> OdbResult<Self> {
        let mut indexes = Vec::with_capacity(pack_ids.len());
        let mut objects_in_offset_cache: usize = 0;
        let max_objects_in_offset_cache = index_offset_cache_max / size_of::<u32>();
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
                pack_pages: PageCache::bounded(PACK_PAGE_CACHE_MAX),
            });
        }
        Ok(Self { indexes })
    }
}
