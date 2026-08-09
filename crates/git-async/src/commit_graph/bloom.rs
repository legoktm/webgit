//! Changed-path Bloom filters from the commit-graph's `BDAT`/`BIDX` chunks.
//!
//! Git stores, per commit, a Bloom filter of the paths changed by that commit
//! relative to its first parent — including every parent-directory prefix, so a
//! query for a directory matches a commit that touched a file beneath it. The
//! filter answers "definitely did not change" or "maybe changed"; a "maybe" is
//! resolved by an actual tree diff, so false positives only cost time.
//!
//! See `gitformat-commit-graph(5)` and git's `bloom.c` for the reference.

/// The settings recorded in the 12-byte `BDAT` chunk header.
#[derive(Clone, Copy, Debug)]
pub struct BloomSettings {
    /// Changed-path filter version. Version 1 (git's historical default) hashes
    /// path bytes as *signed* `char`, sign-extending bytes ≥ 0x80; version 2
    /// treats them as unsigned. The two agree for ASCII paths.
    pub hash_version: u32,
    /// Number of hash positions probed per key (git default 7).
    pub num_hashes: u32,
    /// Bits allocated per stored path (git default 10); not needed to query.
    pub bits_per_entry: u32,
}

/// The two seeds git uses to derive a key's hash pair (`bloom.c`).
const SEED0: u32 = 0x293a_e76f;
const SEED1: u32 = 0x7e64_6e2c;

/// A 32-bit `MurmurHash3`, matching git's `murmur3_seeded_v2`.
///
/// When `sign_extend` is set (changed-path version 1) each path byte is
/// interpreted as a signed `char` before being widened to `u32`, reproducing
/// git's historical behaviour exactly; otherwise bytes are unsigned (version 2).
///
/// The casts here are deliberate bit-level reinterpretations matching C's
/// unsigned 32-bit arithmetic, so the sign/truncation lints are silenced.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn murmur3(mut seed: u32, data: &[u8], sign_extend: bool) -> u32 {
    const C1: u32 = 0xcc9e_2d51;
    const C2: u32 = 0x1b87_3593;
    const R1: u32 = 15;
    const R2: u32 = 13;
    const M: u32 = 5;
    const N: u32 = 0xe654_6b64;

    let widen = |b: u8| -> u32 {
        if sign_extend {
            i32::from(b as i8) as u32
        } else {
            u32::from(b)
        }
    };

    let len = data.len();
    let len4 = len / 4;
    for i in 0..len4 {
        let mut k = widen(data[4 * i])
            | (widen(data[4 * i + 1]) << 8)
            | (widen(data[4 * i + 2]) << 16)
            | (widen(data[4 * i + 3]) << 24);
        k = k.wrapping_mul(C1);
        k = k.rotate_left(R1);
        k = k.wrapping_mul(C2);
        seed ^= k;
        seed = seed.rotate_left(R2).wrapping_mul(M).wrapping_add(N);
    }

    let tail = &data[len4 * 4..];
    let mut k1: u32 = 0;
    if tail.len() >= 3 {
        k1 ^= widen(tail[2]) << 16;
    }
    if tail.len() >= 2 {
        k1 ^= widen(tail[1]) << 8;
    }
    if !tail.is_empty() {
        k1 ^= widen(tail[0]);
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(R1);
        k1 = k1.wrapping_mul(C2);
        seed ^= k1;
    }

    seed ^= len as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85eb_ca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2_ae35);
    seed ^= seed >> 16;
    seed
}

/// Whether `path` might be present in `filter` (i.e. the commit *might* have
/// changed it). Returns `true` ("maybe") for an empty filter — git writes a
/// zero-length filter when the change set is empty or too large to record, which
/// callers must resolve with a real diff. A `false` is a definitive "did not
/// change" and is always safe to trust.
pub fn path_maybe_changed(filter: &[u8], settings: &BloomSettings, path: &[u8]) -> bool {
    let modulus = (filter.len() as u64) * 8;
    if modulus == 0 {
        return true;
    }
    let sign_extend = settings.hash_version == 1;
    let hash0 = murmur3(SEED0, path, sign_extend);
    let hash1 = murmur3(SEED1, path, sign_extend);
    for i in 0..settings.num_hashes {
        let hash = hash0.wrapping_add(i.wrapping_mul(hash1));
        let bit = u64::from(hash) % modulus;
        let byte = usize::try_from(bit / 8).unwrap();
        if filter[byte] & (1u8 << (bit % 8)) == 0 {
            return false;
        }
    }
    true
}
