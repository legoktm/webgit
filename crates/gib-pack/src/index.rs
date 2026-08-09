use crate::{PackError, PackResult};
use gib_fs::{File, Offset};
use gib_hash::{ObjectId, ObjectIdPrefix, PrefixResolution};
use std::cmp::Ordering;

// Where the sorted table of object IDs starts: past the magic number, the
// version and the fanout table.
const IDS_OFFSET: Offset = Offset(
    4 // magic number
    + 4 // version
    + 256 * 4, // fanout
);

#[derive(Clone)]
pub struct FanoutTable {
    fanout: [u32; 256],
}

impl FanoutTable {
    pub async fn load<F: File>(file: &mut F) -> PackResult<Self> {
        let mut buf = [0u8; 4 + 4 + 256 * 4];
        let read_size = file.read_segment(Offset(0), &mut buf).await?;
        if read_size != buf.len() {
            return Err(PackError::CorruptIndexFile);
        }
        if buf[0..8] != [0xff, b't', b'O', b'c', 0, 0, 0, 2] {
            return Err(PackError::UnsupportedIndexVersion);
        }
        let mut fanout = [0u32; 256];
        for (entry_bytes, entry) in buf[8..].chunks(4).zip(fanout.iter_mut()) {
            *entry = u32::from_be_bytes(entry_bytes.try_into().unwrap());
        }
        Ok(Self { fanout })
    }

    pub fn entry(&self, prefix: u8) -> u32 {
        self.fanout[usize::from(prefix)]
    }

    pub fn total_objects(&self) -> u32 {
        *self.fanout.last().unwrap()
    }
}

#[derive(Clone)]
pub struct ShortOffsetTable {
    table: Vec<u8>,
}

impl ShortOffsetTable {
    pub fn offset_of_table(total_objects: u32) -> Offset {
        Offset(
            4 // header
            + 4 // version
            + 256 * 4 // fanout
            + u64::from(total_objects) * 20 // object IDs
            + u64::from(total_objects) * 4, // CRCs
        )
    }

    pub async fn load<F: File>(file: &mut F, total_objects: u32) -> PackResult<Self> {
        let table_size: usize = usize::try_from(total_objects).unwrap() * 4;
        let mut table = vec![0u8; table_size];
        let read_size = file
            .read_segment(Self::offset_of_table(total_objects), &mut table)
            .await?;
        if read_size < table_size {
            return Err(PackError::CorruptIndexFile);
        }
        Ok(Self { table })
    }

    pub fn entry(&self, object_idx: u32) -> u32 {
        let object_idx: usize = object_idx.try_into().unwrap();
        let entry_bytes = &self.table[(object_idx * 4)..((object_idx + 1) * 4)];
        u32::from_be_bytes(entry_bytes.try_into().unwrap())
    }
}

pub async fn find_object_in_pack_index<F: File>(
    fanout: &FanoutTable,
    offsets: Option<&ShortOffsetTable>,
    idx_file: &mut F,
    id: ObjectId,
) -> PackResult<Option<Offset>> {
    if let Some(obj_idx) = find_object_idx(fanout, idx_file, id).await? {
        let offset =
            get_obj_packfile_offset(offsets, idx_file, obj_idx, fanout.total_objects()).await?;
        Ok(Some(offset))
    } else {
        Ok(None)
    }
}

async fn find_object_idx<F: File>(
    fanout: &FanoutTable,
    idx_file: &mut F,
    id: ObjectId,
) -> PackResult<Option<u32>> {
    let (lower_bound, upper_bound) = fanout_bucket(fanout, id.bytes()[0]);

    let mut buf = [0u8; 20];
    let mut lower_idx = lower_bound; // inclusive
    let mut upper_idx = upper_bound; // exclusive
    let mut obj_idx: Option<u32> = None;
    while obj_idx.is_none() && lower_idx < upper_idx {
        let mid_idx: u32 = u32::midpoint(lower_idx, upper_idx);
        let mid_offset: Offset = IDS_OFFSET + u64::from(mid_idx) * 20;
        idx_file.read_segment(mid_offset, &mut buf).await?;
        match buf.cmp(id.bytes()) {
            Ordering::Equal => {
                obj_idx = Some(mid_idx);
            }
            Ordering::Less => {
                lower_idx = mid_idx + 1;
            }
            Ordering::Greater => {
                upper_idx = mid_idx;
            }
        }
    }
    Ok(obj_idx)
}

/// The `[lower, upper)` range of index entries whose object ID starts with
/// `first_byte`, straight out of the fanout table.
fn fanout_bucket(fanout: &FanoutTable, first_byte: u8) -> (u32, u32) {
    let lower = if first_byte == 0 {
        0
    } else {
        fanout.entry(first_byte - 1)
    };
    (lower, fanout.entry(first_byte))
}

/// Read the object ID at `obj_idx` in the index's sorted ID table.
async fn read_object_id<F: File>(idx_file: &mut F, obj_idx: u32) -> PackResult<ObjectId> {
    let mut buf = [0u8; 20];
    idx_file
        .read_segment(IDS_OFFSET + u64::from(obj_idx) * 20, &mut buf)
        .await?;
    Ok(ObjectId::from_bytes(buf))
}

/// Expand an abbreviated object ID against one pack's index.
///
/// This is the same binary search as [`find_object_idx`], except that the
/// comparison only looks at the abbreviation's nibbles (so a whole run of IDs
/// compares equal) and the search deliberately never stops early: it converges
/// on the *first* ID that could match. Two more reads settle the outcome — the
/// candidate itself, and its successor, which matching too means the
/// abbreviation names more than one object.
///
/// Only the fanout bucket for the abbreviation's first byte is searched, which
/// is sound because an abbreviation is at least four characters long, so every
/// ID it covers shares that byte.
pub async fn find_prefix_in_pack_index<F: File>(
    fanout: &FanoutTable,
    idx_file: &mut F,
    prefix: &ObjectIdPrefix,
) -> PackResult<PrefixResolution> {
    let (lower_bound, upper_bound) = fanout_bucket(fanout, prefix.first_byte());

    // Partition point: the first entry that does not sort before the prefix.
    let mut lower_idx = lower_bound;
    let mut upper_idx = upper_bound;
    while lower_idx < upper_idx {
        let mid_idx = u32::midpoint(lower_idx, upper_idx);
        if prefix.compare(&read_object_id(idx_file, mid_idx).await?) == Ordering::Less {
            lower_idx = mid_idx + 1;
        } else {
            upper_idx = mid_idx;
        }
    }

    if lower_idx >= upper_bound {
        return Ok(PrefixResolution::NotFound);
    }
    let candidate = read_object_id(idx_file, lower_idx).await?;
    if !prefix.matches(&candidate) {
        return Ok(PrefixResolution::NotFound);
    }
    // Any further match must be the very next entry, and cannot lie beyond this
    // fanout bucket, whose entries all share the abbreviation's first byte.
    if lower_idx + 1 < upper_bound
        && prefix.matches(&read_object_id(idx_file, lower_idx + 1).await?)
    {
        return Ok(PrefixResolution::Ambiguous);
    }
    Ok(PrefixResolution::Found(candidate))
}

async fn get_obj_packfile_offset<F: File>(
    offset_table: Option<&ShortOffsetTable>,
    idx_file: &mut F,
    obj_idx: u32,
    total_objects: u32,
) -> PackResult<Offset> {
    let packfile_offset_short = if let Some(offset_table) = offset_table {
        offset_table.entry(obj_idx)
    } else {
        let entry_offset =
            ShortOffsetTable::offset_of_table(total_objects) + u64::from(obj_idx) * 4;
        let mut buf = [0u8; 4];
        idx_file.read_segment(entry_offset, &mut buf).await?;
        u32::from_be_bytes(buf)
    };
    if packfile_offset_short & 0x8000_0000 != 0 {
        let fanout: Offset = Offset(0x8);
        let object_ids: Offset = fanout + 4 * 256;
        let crc_table: Offset = object_ids + u64::from(total_objects) * 20;
        let short_table: Offset = crc_table + u64::from(total_objects) * 4;
        let long_table_idx: u32 = packfile_offset_short & 0x7fff_ffff;
        let long_table: Offset = short_table + 4 * u64::from(total_objects);
        let long_entry: Offset = long_table + 8 * u64::from(long_table_idx);
        let mut buf = [0u8; 8];
        idx_file.read_segment(long_entry, &mut buf).await?;
        let packfile_offset_long = u64::from_be_bytes(buf);
        Ok(Offset(packfile_offset_long))
    } else {
        Ok(Offset(u64::from(packfile_offset_short)))
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use gib_testkit::{get_pack_id, make_basic_repo, make_file, make_packfile_repo};
    use hex_literal::hex;
    use rand_core::{Rng, SeedableRng};
    use rand_pcg::Pcg32;
    use std::io::Write;

    use super::*;

    #[test]
    fn test_find_object_idx() {
        let repo = make_packfile_repo().unwrap();
        let pack_id = get_pack_id(&repo).unwrap();
        let mut idx_file = repo.pack_idx_file(&pack_id);
        let fanout = block_on(FanoutTable::load(&mut idx_file)).unwrap();
        let obj_idx = block_on(find_object_idx(
            &fanout,
            &mut idx_file,
            ObjectId::from_bytes(hex!("78dc5b70bd81aa46ec7dfce87a69826e354a916b")),
        ))
        .unwrap();
        assert!(obj_idx.is_some());
        let null_obj_idx = block_on(find_object_idx(
            &fanout,
            &mut idx_file,
            ObjectId::from_bytes(hex!("0000000000000000000000000000000000000000")),
        ))
        .unwrap();
        assert_eq!(null_obj_idx, None);
        let similar_obj_idx = block_on(find_object_idx(
            &fanout,
            &mut idx_file,
            ObjectId::from_bytes(hex!("7800000000000000000000000000000000000000")),
        ))
        .unwrap();
        assert_eq!(similar_obj_idx, None);
    }

    #[test]
    fn test_get_obj_packfile_offset_normal() {
        let repo = make_packfile_repo().unwrap();
        let pack_id = get_pack_id(&repo).unwrap();
        let mut idx_file = repo.pack_idx_file(&pack_id);
        let fanout = block_on(FanoutTable::load(&mut idx_file)).unwrap();
        let offsets = block_on(ShortOffsetTable::load(
            &mut idx_file,
            fanout.total_objects(),
        ))
        .unwrap();
        let object_idx = block_on(find_object_idx(
            &fanout,
            &mut idx_file,
            ObjectId::from_bytes(hex!("78dc5b70bd81aa46ec7dfce87a69826e354a916b")),
        ))
        .unwrap()
        .unwrap();
        block_on(get_obj_packfile_offset(
            Some(&offsets),
            &mut idx_file,
            object_idx,
            fanout.total_objects(),
        ))
        .unwrap();
    }

    /// A `.idx` held in memory. Object IDs that share a prefix are what the
    /// prefix search's interesting cases are made of, and no real repository
    /// can be coaxed into producing a pair, so these tests build the index by
    /// hand instead of committing files into a test repo.
    struct MemIdx(Vec<u8>);

    impl File for MemIdx {
        async fn read_all(&mut self) -> Result<Vec<u8>, gib_fs::FileSystemError> {
            Ok(self.0.clone())
        }

        async fn read_segment(
            &mut self,
            offset: Offset,
            dest: &mut [u8],
        ) -> Result<usize, gib_fs::FileSystemError> {
            let start = usize::try_from(offset.0).unwrap().min(self.0.len());
            let len = dest.len().min(self.0.len() - start);
            dest[..len].copy_from_slice(&self.0[start..(start + len)]);
            Ok(len)
        }
    }

    /// An object ID written as a short hex string, right-padded with zeroes.
    fn oid(hex: &str) -> ObjectId {
        let mut padded = hex.as_bytes().to_vec();
        padded.resize(40, b'0');
        ObjectId::from_hex(&padded).unwrap()
    }

    /// Build a v2 pack index over `ids`, which must be sorted. Only the header,
    /// fanout and ID table are filled in; the CRC, offset and checksum trailers
    /// are zeroed, since the prefix search never reads them.
    fn make_idx(ids: &[ObjectId]) -> MemIdx {
        let mut data = vec![0xff, b't', b'O', b'c', 0, 0, 0, 2];
        for bucket in 0..=255u8 {
            let cumulative = ids.iter().filter(|id| id.bytes()[0] <= bucket).count();
            data.extend_from_slice(&u32::try_from(cumulative).unwrap().to_be_bytes());
        }
        for id in ids {
            data.extend_from_slice(id.bytes());
        }
        data.resize(data.len() + ids.len() * 8 + 40, 0);
        MemIdx(data)
    }

    /// Every ID sharing a first byte lives in one fanout bucket, and one ID has
    /// the lowest possible first byte and another the highest, so the searches
    /// below cover both ends of the table as well as its middle.
    fn colliding_idx() -> (FanoutTable, MemIdx) {
        let ids = [
            oid("00"),
            oid("12ab34"),
            oid("12ab35"),
            oid("12ff"),
            oid("13aa"),
            oid("ff"),
        ];
        let mut idx = make_idx(&ids);
        let fanout = block_on(FanoutTable::load(&mut idx)).unwrap();
        (fanout, idx)
    }

    #[test]
    fn test_find_prefix_unique() {
        let (fanout, mut idx) = colliding_idx();
        let mut resolve = |hex: &str| {
            let prefix = ObjectIdPrefix::from_hex(hex.as_bytes()).unwrap();
            block_on(find_prefix_in_pack_index(&fanout, &mut idx, &prefix)).unwrap()
        };
        assert_eq!(resolve("12ab34"), PrefixResolution::Found(oid("12ab34")));
        // The first and last entries of the whole table.
        assert_eq!(resolve("0000"), PrefixResolution::Found(oid("00")));
        assert_eq!(resolve("ff00"), PrefixResolution::Found(oid("ff")));
        // The last entry of its bucket: the ID that follows it in the table is
        // in the next bucket, and must not be mistaken for a second match.
        assert_eq!(resolve("12ff"), PrefixResolution::Found(oid("12ff")));
    }

    #[test]
    fn test_find_prefix_not_found() {
        let (fanout, mut idx) = colliding_idx();
        let mut resolve = |hex: &str| {
            let prefix = ObjectIdPrefix::from_hex(hex.as_bytes()).unwrap();
            block_on(find_prefix_in_pack_index(&fanout, &mut idx, &prefix)).unwrap()
        };
        // Nothing in an occupied bucket, nothing in an empty one, and nothing
        // just past the last entry of a bucket.
        assert_eq!(resolve("12ac"), PrefixResolution::NotFound);
        assert_eq!(resolve("9999"), PrefixResolution::NotFound);
        assert_eq!(resolve("12ffff01"), PrefixResolution::NotFound);
    }

    #[test]
    fn test_find_prefix_ambiguous() {
        let (fanout, mut idx) = colliding_idx();
        let mut resolve = |hex: &str| {
            let prefix = ObjectIdPrefix::from_hex(hex.as_bytes()).unwrap();
            block_on(find_prefix_in_pack_index(&fanout, &mut idx, &prefix)).unwrap()
        };
        // `12ab34…` and `12ab35…` differ only in their tenth nibble, so any
        // shorter abbreviation names both.
        assert_eq!(resolve("12ab3"), PrefixResolution::Ambiguous);
        assert_eq!(resolve("12ab"), PrefixResolution::Ambiguous);
    }

    #[test]
    fn test_find_prefix_in_real_index() {
        let repo = make_packfile_repo().unwrap();
        let pack_id = get_pack_id(&repo).unwrap();
        let mut idx_file = repo.pack_idx_file(&pack_id);
        let fanout = block_on(FanoutTable::load(&mut idx_file)).unwrap();
        let head = ObjectId::from_bytes(hex!("78dc5b70bd81aa46ec7dfce87a69826e354a916b"));
        let prefix = ObjectIdPrefix::from_hex(b"78dc5b7").unwrap();
        assert_eq!(
            block_on(find_prefix_in_pack_index(&fanout, &mut idx_file, &prefix)).unwrap(),
            PrefixResolution::Found(head)
        );
        // A full-length abbreviation resolves to itself, and an unknown one to
        // nothing.
        let full = ObjectIdPrefix::from_hex(b"78dc5b70bd81aa46ec7dfce87a69826e354a916b").unwrap();
        assert_eq!(
            block_on(find_prefix_in_pack_index(&fanout, &mut idx_file, &full)).unwrap(),
            PrefixResolution::Found(head)
        );
        let missing = ObjectIdPrefix::from_hex(b"78dc5b8").unwrap();
        assert_eq!(
            block_on(find_prefix_in_pack_index(&fanout, &mut idx_file, &missing)).unwrap(),
            PrefixResolution::NotFound
        );
    }

    #[ignore = "takes a long time and requires many GiB of disk space"]
    #[test]
    fn test_get_obj_packfile_offset_huge() {
        const MEGABYTE: usize = 1024 * 1024;
        let repo = make_basic_repo().unwrap();
        let mut buf = vec![0u8; MEGABYTE];
        let mut rng = Pcg32::seed_from_u64(0);

        let mut huge_file_1 = make_file(&repo, "a-huge-file").unwrap();
        for _ in 0..2048 {
            rng.fill_bytes(&mut buf);
            huge_file_1.write_all(&buf).unwrap();
        }
        huge_file_1.flush().unwrap();
        let mut huge_file_2 = make_file(&repo, "another-huge-file").unwrap();
        for _ in 0..2048 {
            rng.fill_bytes(&mut buf);
            huge_file_2.write_all(&buf).unwrap();
        }
        huge_file_2.flush().unwrap();

        let metadata_1 = huge_file_1.metadata().unwrap();
        assert_eq!(metadata_1.len(), 2048 * MEGABYTE as u64);
        let metadata_2 = huge_file_1.metadata().unwrap();
        assert_eq!(metadata_2.len(), 2048 * MEGABYTE as u64);
        repo.run_git(["add", "a-huge-file"]).unwrap();
        repo.run_git(["add", "another-huge-file"]).unwrap();

        repo.commit(
            "another commit",
            "a user",
            "an-email-address",
            "2000-01-01T00:00:00Z",
        )
        .unwrap();
        let head_id = repo
            .run_git(["rev-parse", "HEAD"])
            .unwrap()
            .trim_ascii_end()
            .to_vec();
        assert_eq!(head_id, b"7e352726d6addfb0da5e3990393975188c5625ab");
        let expected_blob_id_another_huge_file =
            ObjectId::from_bytes(hex!("ead5be8e71f3cb2e585e14436087fd84119dd354"));
        repo.run_git(["gc"]).unwrap();
        let pack_file_id = get_pack_id(&repo).unwrap();
        let mut idx_file = repo.pack_idx_file(&pack_file_id);
        let fanout = block_on(FanoutTable::load(&mut idx_file)).unwrap();
        let offsets = block_on(ShortOffsetTable::load(
            &mut idx_file,
            fanout.total_objects(),
        ))
        .unwrap();
        let object_offset = block_on(find_object_idx(
            &fanout,
            &mut idx_file,
            expected_blob_id_another_huge_file,
        ))
        .unwrap()
        .unwrap();
        let pack_offset = block_on(get_obj_packfile_offset(
            Some(&offsets),
            &mut idx_file,
            object_offset,
            fanout.total_objects(),
        ))
        .unwrap();
        assert!(pack_offset.0 >= 0x8000_0000);
    }
}
