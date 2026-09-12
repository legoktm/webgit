use crate::{
    IndexedPackFile, ObjectResult, PackError, PackObjectError, PackResult,
    index::find_object_in_pack_index,
};
use gib_fs::{File, Offset};
use gib_hash::ObjectId;
use gib_object::{ObjectSize, ObjectType};
use miniz_oxide::inflate::{
    TINFLStatus,
    core::{
        DecompressorOxide, decompress,
        inflate_flags::{
            TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER,
            TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
        },
    },
};

/// How far back in the pack a delta's base sits, counted from the delta's own
/// header: the only way an `OFS_DELTA` names its base.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PackNegativeOffset(pub u64);

/// What a pack object's header says it is: an object of one of the four git
/// types, or a delta against a base the pack names one of two ways.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PackObjectType {
    /// An object stored whole.
    Base(ObjectType),
    /// A delta against an object earlier in this same pack.
    OffsetDelta {
        /// How far back the base sits from this object's header.
        base_offset_neg: PackNegativeOffset,
    },
    /// A delta against an object named by id, which a non-thin pack also holds.
    RefDelta {
        /// The id of the object this is a delta against.
        base_id: ObjectId,
    },
}

/// One object's location and decompressed size within a packfile.
#[derive(Debug)]
pub struct PackObject {
    pub body_offset: Offset,
    pub size: ObjectSize,
    // pub body_initial: Vec<u8>,
}

// Git uses three slightly different algorithms for encoding variable-width
// integers in different contexts within the packfile. This is not documented
// anywhere.

fn read_obj_type_size(buf: &[u8]) -> ObjectResult<(usize, PackObjectType, ObjectSize)> {
    // This algorithm is for reading the first part of the packfile object
    // header, which encodes the object type and size.
    let mut pos: usize = 0;
    let mut object_type: Option<PackObjectType> = None;
    let mut obj_size = ObjectSize(0);
    let mut done_accumulating_size = false;
    for buf_byte in buf {
        done_accumulating_size = (0b1000_0000 & *buf_byte) == 0;
        if pos == 0 {
            let obj_type_id = 0b0111_0000 & *buf_byte;
            object_type = Some(match obj_type_id {
                0b0001_0000 => PackObjectType::Base(ObjectType::Commit),
                0b0010_0000 => PackObjectType::Base(ObjectType::Tree),
                0b0011_0000 => PackObjectType::Base(ObjectType::Blob),
                0b0100_0000 => PackObjectType::Base(ObjectType::Tag),
                0b0110_0000 => PackObjectType::OffsetDelta {
                    base_offset_neg: PackNegativeOffset(0),
                },
                0b0111_0000 => PackObjectType::RefDelta {
                    base_id: ObjectId::from_bytes([0; 20]),
                },
                _ => return Err(PackObjectError::MalformedObject),
            });
            let size_bits = 0b0000_1111 & *buf_byte;
            obj_size.0 = size_bits.into();
        } else {
            let size_bits = 0b0111_1111 & *buf_byte;
            let shift: usize = 4 + 7 * (pos - 1);
            obj_size.0 += u64::from(size_bits) << shift;
        }
        pos += 1;
        if done_accumulating_size {
            break;
        }
    }
    // A varint whose continuation bit is set on the last byte available runs
    // off the end of what the caller handed over. From a file that is a short
    // read near EOF; from a bundle someone uploaded it is simply a broken
    // header, and either way there is no header here to return.
    if !done_accumulating_size {
        return Err(PackError::CorruptPackFile.into());
    }
    Ok((pos, object_type.unwrap(), obj_size))
}

fn read_delta_offset(buf: &[u8]) -> ObjectResult<(usize, PackNegativeOffset)> {
    // This algorithm is for reading the second part of the packfile object
    // header (in the case of an offset delta object), which encodes the
    // relative negative offset of the delta object's base object
    let mut bytes_read = 0;
    let mut offset = PackNegativeOffset(0);
    let mut done_accumulating_offset = false;
    for (buf_idx, buf_byte) in buf.iter().enumerate() {
        done_accumulating_offset = (0b1000_0000 & *buf_byte) == 0;
        if buf_idx != 0 {
            offset.0 += 1;
        }
        offset.0 <<= 7;
        offset.0 += u64::from(buf_byte & 0b0111_1111);
        bytes_read += 1;
        if done_accumulating_offset {
            break;
        }
    }
    if !done_accumulating_offset {
        return Err(PackError::CorruptPackFile.into());
    }
    Ok((bytes_read, offset))
}

fn read_delta_expected_size(buf: &[u8]) -> ObjectResult<(usize, ObjectSize)> {
    // This algorithm is for reading the expected base object and un-deltified
    // object sizes, which form the header of the decompressed data stream in an
    // offset delta object.
    let mut bytes_read = 0;
    let mut size = ObjectSize(0);
    let mut done_accumulating_size = false;
    let mut shift = 0;
    for buf_byte in buf {
        done_accumulating_size = (0b1000_0000 & *buf_byte) == 0;
        size.0 += u64::from(buf_byte & 0b0111_1111) << shift;
        shift += 7;
        bytes_read += 1;
        if done_accumulating_size {
            break;
        }
    }
    if !done_accumulating_size {
        return Err(PackObjectError::MalformedDelta);
    }
    Ok((bytes_read, size))
}

/// Check that a `.pack` file is one this crate can read, before any object is
/// looked up in it.
pub async fn validate_packfile_version<F: File>(pack_file: &mut F) -> PackResult<()> {
    let mut buf = [0u8; 8];
    pack_file.read_segment(Offset(0), &mut buf).await?;
    if buf != [b'P', b'A', b'C', b'K', 0, 0, 0, 2] {
        return Err(PackError::UnsupportedPackVersion);
    }
    Ok(())
}

/// The longest an object header can be: a size varint for `u64::MAX`, and
/// then whichever of a delta's two ways of naming its base is longer.
pub const MAX_OBJECT_HEADER_LEN: usize = 10 + 20;

/// Read one object's header out of `buf`, which must begin at the object's
/// first byte: what the object is, how big the body stored for it is — for a
/// delta, the delta's own size rather than the size of what it rebuilds — and
/// how many bytes the header took, which is where its body begins.
///
/// Pure, because a pack is not always read the same way: this crate reaches
/// one through a `File` a page at a time, while `gib-bundle` reads an uploaded
/// one straight out of memory. Both need the same bytes decoded, so neither
/// decodes them itself.
pub fn parse_object_header(buf: &[u8]) -> ObjectResult<(PackObjectType, ObjectSize, usize)> {
    let (mut pos, mut object_type, obj_size) = read_obj_type_size(buf)?;

    match object_type {
        PackObjectType::Base(..) => {}
        PackObjectType::OffsetDelta {
            ref mut base_offset_neg,
        } => {
            let rest = buf.get(pos..).ok_or(PackError::CorruptPackFile)?;
            let (bytes_read, offset) = read_delta_offset(rest)?;
            *base_offset_neg = offset;
            pos += bytes_read;
        }
        PackObjectType::RefDelta { ref mut base_id } => {
            let bytes = buf.get(pos..(pos + 20)).ok_or(PackError::CorruptPackFile)?;
            *base_id = ObjectId::from_bytes(<[u8; 20]>::try_from(bytes).unwrap());
            pos += 20;
        }
    }

    Ok((object_type, obj_size, pos))
}

async fn read_pack_object_header<F: File>(
    pack_file: &mut F,
    offset: Offset,
) -> ObjectResult<(PackObjectType, PackObject)> {
    let mut buf = [0u8; MAX_OBJECT_HEADER_LEN];
    let eof_pos = pack_file.read_segment(offset, &mut buf).await?;
    // Only what the file actually held: past `eof_pos` the buffer is our own
    // zeroes, and a header parsed out of those is not one the pack contains.
    let (object_type, obj_size, pos) = parse_object_header(&buf[..eof_pos])?;
    // A header that runs to the last byte of the file has no body behind it.
    if pos >= eof_pos {
        return Err(PackError::CorruptPackFile.into());
    }

    Ok((
        object_type,
        PackObject {
            body_offset: Offset(offset.0 + (pos as u64)),
            size: obj_size,
        },
    ))
}

async fn read_pack_object_body<F: File>(
    pack_file: &mut F,
    object: &PackObject,
) -> ObjectResult<Vec<u8>> {
    // The compressed body is read sequentially, but the underlying file is
    // often backed by paged network fetches. Reading in one large chunk lets
    // those layers coalesce the read into a single request rather than one per
    // page. The decompressed size bounds the compressed size for all but tiny
    // objects, so size the buffer to it (with slack for zlib overhead) and cap
    // it so huge objects don't allocate unboundedly; the loop reads more if
    // needed.
    const MAX_BODY_READ: usize = 1 << 20;
    let object_size =
        usize::try_from(object.size.0).map_err(|_| PackObjectError::ObjectTooLarge)?;
    let mut pos = 0;
    let chunk_size = object_size.saturating_add(64).clamp(512, MAX_BODY_READ);
    let mut compressed_body_buf = vec![0u8; chunk_size];
    let mut body = vec![0u8; object_size];
    let mut state = Box::<DecompressorOxide>::default();
    let mut out_idx: usize = 0;
    loop {
        use TINFLStatus::*;
        pack_file
            .read_segment(
                object.body_offset + u64::try_from(pos).unwrap(),
                &mut compressed_body_buf,
            )
            .await?;
        let (status, input_read, output_written) = decompress(
            &mut state,
            &compressed_body_buf,
            &mut body,
            out_idx,
            TINFL_FLAG_HAS_MORE_INPUT
                | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
                | TINFL_FLAG_PARSE_ZLIB_HEADER,
        );
        pos += input_read;
        out_idx += output_written;
        match status {
            Done => break,
            NeedsMoreInput | HasMoreOutput => {}
            _ => {
                return Err(PackObjectError::Decompress(status));
            }
        }
    }
    Ok(body)
}

/// Walk the delta chain from `start_offset` down to the base object it
/// ultimately derives from, collecting the deltas on the way.
pub async fn form_deltified_chain<F: File>(
    indexed_pack: &mut IndexedPackFile<'_, F>,
    start_offset: Offset,
) -> ObjectResult<(Vec<PackObject>, ObjectType, PackObject)> {
    let mut chain = Vec::new();
    let mut final_object: Option<(ObjectType, PackObject)> = None;
    let mut offset = start_offset;
    while final_object.is_none() {
        let (object_type, object) = read_pack_object_header(&mut indexed_pack.pack, offset).await?;
        match &object_type {
            PackObjectType::OffsetDelta { base_offset_neg } => {
                offset.0 -= base_offset_neg.0;
                chain.push(object);
            }
            PackObjectType::RefDelta { base_id } => {
                let base_offset = find_object_in_pack_index(
                    indexed_pack.fanout,
                    indexed_pack.offsets,
                    &mut indexed_pack.index,
                    *base_id,
                )
                .await?;
                offset = base_offset
                    .ok_or_else(|| PackObjectError::from(PackError::UnexpectedThinPack))?;
                chain.push(object);
            }
            PackObjectType::Base(base_type) => {
                final_object = Some((*base_type, object));
            }
        }
    }
    let (final_type, final_object) = final_object.unwrap();
    Ok((chain, final_type, final_object))
}

/// Rebuild an object from a delta and the object that delta was written
/// against.
///
/// A delta is a size header — the base's size and the result's, so a reader can
/// check it has the right base — followed by instructions that either copy a
/// run of bytes out of the base or append bytes carried in the delta itself.
///
/// Every read out of either buffer is checked rather than indexed: the deltas
/// this rebuilds come from packs fetched over the network and from bundles
/// someone uploaded, and a malformed one must be an error and not a panic that
/// takes the page down with it.
pub fn apply_delta(deltified: &[u8], base: &[u8]) -> ObjectResult<Vec<u8>> {
    let take = |pos: &mut usize, len: usize| -> ObjectResult<&[u8]> {
        let slice = deltified
            .get(*pos..(*pos + len))
            .ok_or(PackObjectError::MalformedDelta)?;
        *pos += len;
        Ok(slice)
    };

    let mut pos: usize = 0;
    let (bytes_read, base_object_size) = read_delta_expected_size(&deltified[pos..])?;
    pos += bytes_read;
    if base_object_size.0 != base.len() as u64 {
        return Err(PackObjectError::MalformedDelta);
    }
    let (bytes_read, reconstructed_body_size) = read_delta_expected_size(&deltified[pos..])?;
    pos += bytes_read;
    let mut reconstructed_body: Vec<u8> =
        Vec::with_capacity(usize::try_from(reconstructed_body_size.0).unwrap_or(0));
    while pos < deltified.len() {
        let mut instruction = deltified[pos];
        pos += 1;
        if instruction & 0b1000_0000 == 0 {
            // Append
            let size = usize::from(instruction & 0b0111_1111);
            reconstructed_body.extend_from_slice(take(&mut pos, size)?);
        } else {
            // Copy
            let mut offset = [0u8; 4];
            let mut size = [0u8; 4];
            for offset_byte in &mut offset {
                if instruction & 1 != 0 {
                    *offset_byte = take(&mut pos, 1)?[0];
                }
                instruction >>= 1;
            }
            for size_byte in &mut size[..3] {
                if instruction & 1 != 0 {
                    *size_byte = take(&mut pos, 1)?[0];
                }
                instruction >>= 1;
            }
            let offset = usize::try_from(u32::from_le_bytes(offset)).unwrap();
            let mut size = usize::try_from(u32::from_le_bytes(size)).unwrap();
            if size == 0 {
                size = 0x10000;
            }
            let run = base
                .get(offset..(offset + size))
                .ok_or(PackObjectError::MalformedDelta)?;
            reconstructed_body.extend_from_slice(run);
        }
    }
    // The delta said how big what it rebuilds is; if it isn't, the instructions
    // and the header disagree and neither can be trusted.
    if reconstructed_body_size.0 != reconstructed_body.len() as u64 {
        return Err(PackObjectError::MalformedDelta);
    }
    Ok(reconstructed_body)
}

/// Apply a chain from [`form_deltified_chain`] to its base object, yielding
/// the requested object's decompressed body.
pub async fn reconstruct_deltified_object_from_chain<F: File>(
    indexed_pack: &mut IndexedPackFile<'_, F>,
    chain: &[PackObject],
    final_object: &PackObject,
) -> ObjectResult<Vec<u8>> {
    let chain_iter = chain.iter().rev();
    let mut reconstructed_body =
        read_pack_object_body(&mut indexed_pack.pack, final_object).await?;
    for pack_object in chain_iter {
        let pack_object_body = read_pack_object_body(&mut indexed_pack.pack, pack_object).await?;
        reconstructed_body = apply_delta(&pack_object_body, &reconstructed_body)?;
    }
    Ok(reconstructed_body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{OpenPack, open_pack};
    use futures::executor::block_on;
    use gib_testkit::{make_basic_repo, make_similar_commits};
    use hex_literal::hex;

    #[test]
    fn read_deltified_offset_object() {
        let test_repo = make_basic_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
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
        let id = ObjectId::from_hex(b"7ee3a2eb0ff69340e8a1c962a5b573de1cb9b1f6").unwrap();
        let offset = block_on(find_object_in_pack_index(
            &fanout,
            Some(&offsets),
            &mut indexed.index,
            id,
        ))
        .unwrap()
        .unwrap();
        let (object_type, pack_object) =
            block_on(read_pack_object_header(&mut indexed.pack, offset)).unwrap();
        assert_eq!(
            object_type,
            PackObjectType::OffsetDelta {
                base_offset_neg: PackNegativeOffset(128)
            }
        );
        let body = block_on(read_pack_object_body(&mut indexed.pack, &pack_object)).unwrap();
        assert_eq!(body, hex!("94 06 f7 05 b0 85 01 b3 a2 01 72 01"));
    }

    #[test]
    fn form_deltified_object_chain() {
        let test_repo = make_basic_repo().unwrap();
        make_similar_commits(&test_repo).unwrap();
        test_repo.run_git(["gc"]).unwrap();
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
        let id = ObjectId::from_hex(b"9cded1c631096bb2caf71e1f2e0765bf6420d040").unwrap();
        let offset = block_on(find_object_in_pack_index(
            &fanout,
            Some(&offsets),
            &mut indexed.index,
            id,
        ))
        .unwrap()
        .unwrap();
        let (chain, _, _) = block_on(form_deltified_chain(&mut indexed, offset)).unwrap();
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn reconstruct_one_object() {
        let mut base_object = vec![0u8; 128 * 1024];
        for (i, item) in base_object.iter_mut().enumerate() {
            *item = (i % u8::MAX as usize) as u8;
        }

        let mut deltified_object = Vec::new();
        let base_object_size_encoded: [u8; _] = [0b1000_0000, 0b1000_0000, 0b0000_1000]; // 128 * 1024
        assert_eq!(
            read_delta_expected_size(&base_object_size_encoded)
                .unwrap()
                .1,
            ObjectSize(128 * 1024)
        );
        let target_object_size_encoded: [u8; _] = [0b1000_1101, 0b1000_0000, 0b0000_0100]; // 10 + 3 + 0x10000
        assert_eq!(
            read_delta_expected_size(&target_object_size_encoded)
                .unwrap()
                .1,
            ObjectSize(10 + 3 + 0x10000)
        );

        deltified_object.extend_from_slice(&base_object_size_encoded);
        deltified_object.extend_from_slice(&target_object_size_encoded);

        // Small copy
        let offset_1: u32 = 65;
        let size_1: u32 = 10;
        let instruction_1: [u8; _] = [0b1001_0001, 65, 10];
        deltified_object.extend_from_slice(&instruction_1);

        // Append
        let instruction_2: [u8; _] = [0b0000_0011, 0xc0, 0xff, 0xee];
        deltified_object.extend_from_slice(&instruction_2);

        // Copy with special case size = 0 (interpeted as size = 0x10000)
        let offset_3: u32 = 0x10000;
        let instruction_3: [u8; _] = [0b1000_0100, 0x01];
        deltified_object.extend_from_slice(&instruction_3);

        let reconstructed = apply_delta(&deltified_object, &base_object).unwrap();

        assert_eq!(reconstructed.len(), 10 + 3 + 0x10000);
        let mut expected = Vec::new();
        expected
            .extend_from_slice(&base_object[(offset_1 as usize)..((offset_1 + size_1) as usize)]);
        expected.extend_from_slice(&[0xc0, 0xff, 0xee]);
        expected.extend_from_slice(&base_object[offset_3 as usize..(offset_3 + 0x10000) as usize]);
        assert_eq!(expected.len(), 10 + 3 + 0x10000);
        assert!(reconstructed == expected);
    }
}
