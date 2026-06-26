use crate::{
    error::{GResult, annotate_with_object_id},
    file_system::{Directory, FileSystem, Offset},
    object::ObjectId,
    object_store::{
        RawObject,
        cache::IndexCache,
        index::{FanoutTable, ShortOffsetTable, find_object_in_pack_index},
        loose::read_loose_object,
        pack::{form_deltified_chain, reconstruct_deltified_object_from_chain},
        page_read::CachingPageReader,
    },
    repo::Repo,
};
use alloc::vec::Vec;

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

pub(crate) struct IndexedPackFile<'f, F> {
    pub(crate) index: CachingPageReader<F>,
    pub(crate) fanout: &'f FanoutTable,
    pub(crate) offsets: Option<&'f ShortOffsetTable>,
    pub(crate) pack: CachingPageReader<F>,
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
            .map_err(annotate_with_object_id(id))?;
        let body =
            reconstruct_deltified_object_from_chain(&mut indexed_pack, &chain, &final_object)
                .await
                .map_err(annotate_with_object_id(id))?;
        return Ok(Some(RawObject { object_type, body }));
    }
    read_loose_object(repo, id).await
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
