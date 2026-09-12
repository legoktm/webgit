//! Binary deltas, by way of git's own encoder.
//!
//! A delta says how to rebuild one object out of another: copy a range of the
//! base, insert some literal bytes, repeat. It is what makes a packfile a
//! fraction of the size of the objects in it, and so what makes a bundle worth
//! downloading — undeltified, a bundle of this repository is about four times
//! the size of the one `git bundle create` writes.

#![deny(clippy::all)]

mod shim;

use std::ffi::{c_ulong, c_void};
use std::ptr::NonNull;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// The C surface
// ---------------------------------------------------------------------------
//
// These mirror `shim/delta.h`, which is the half of git's `delta.h` that
// `diff-delta.c` defines.

/// git's `struct delta_index`, which is opaque to us: it is allocated, passed
/// back and freed, and never inspected.
#[repr(C)]
struct RawDeltaIndex {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn create_delta_index(buf: *const c_void, bufsize: c_ulong) -> *mut RawDeltaIndex;
    fn free_delta_index(index: *mut RawDeltaIndex);
    fn sizeof_delta_index(index: *mut RawDeltaIndex) -> c_ulong;
    fn create_delta(
        index: *const RawDeltaIndex,
        trg_buf: *const c_void,
        trg_size: c_ulong,
        delta_size: *mut c_ulong,
        max_size: c_ulong,
    ) -> *mut c_void;
}

/// A base object, indexed so that deltas against it can be built.
///
/// Indexing costs a pass over the base, so a caller trying one base against
/// several targets should build this once and keep it — which is exactly what a
/// pack writer's delta window does.
///
/// # Why this owns its bytes
///
/// git's index does not copy the base: it stores pointers *into* it, and its
/// header says the buffer "must not be freed nor altered before
/// `free_delta_index()` is called". Holding the base in an [`Rc<[u8]>`] is what
/// discharges that. The allocation behind an `Rc` never moves, cloning one only
/// bumps a refcount, and `Rc<[u8]>` offers no way to mutate bytes another handle
/// can see — so for as long as this struct is alive, the pointers the C holds
/// stay valid and what they point at stays put. A borrow (`&'a [u8]`) would
/// express the lifetime just as well but makes the index and the bytes
/// impossible to store together, which is precisely what a delta window needs
/// to do.
///
/// [`Rc<[u8]>`]: std::rc::Rc
pub struct DeltaIndex {
    index: NonNull<RawDeltaIndex>,
    /// The indexed bytes, kept alive and unaltered for `index`'s sake. Read
    /// through [`base`](DeltaIndex::base); never mutated.
    base: Rc<[u8]>,
}

impl DeltaIndex {
    /// Index `base` so deltas can be built against it.
    ///
    /// `None` if the base is empty, or if it is too large for the encoder's
    /// 32-bit offsets — a delta's copy instruction cannot name a byte beyond
    /// 4 GiB, so there is nothing useful to build.
    ///
    /// A base too short to hold a single 16-byte block still indexes, and still
    /// yields deltas — they are simply all literal, and so larger than the
    /// object they encode. That is git's behaviour, and callers reject those on
    /// size like git does rather than by asking here.
    pub fn new(base: Rc<[u8]>) -> Option<Self> {
        let size: c_ulong = base.len().try_into().ok()?;
        // SAFETY: `base` is a live allocation of `size` bytes, and the `Rc`
        // moved into the struct below keeps it that way for as long as the
        // index exists.
        let index = unsafe { create_delta_index(base.as_ptr().cast(), size) };
        Some(DeltaIndex {
            index: NonNull::new(index)?,
            base,
        })
    }

    /// The bytes this index was built over.
    pub fn base(&self) -> &[u8] {
        &self.base
    }

    /// How much memory the index itself occupies, not counting the base.
    ///
    /// A delta window holds several of these at once, and on a large object the
    /// index is a real cost — git's own window accounting reads this for the
    /// same reason.
    pub fn memory_usage(&self) -> usize {
        // SAFETY: `self.index` came from `create_delta_index` and is live.
        unsafe { sizeof_delta_index(self.index.as_ptr()) as usize }
    }

    /// Build a delta that rebuilds `target` from this index's base.
    ///
    /// `max_size` is the budget: a delta larger than that is not built, and
    /// `None` comes back instead. It is how a caller says "storing the object
    /// whole would be better than this", and passing `Some(0)` means exactly
    /// that. `None` asks for no budget at all, which is rarely what a packer
    /// wants — a target with nothing in common with the base still produces a
    /// delta, made entirely of literals and so *larger* than the target itself.
    ///
    /// Also `None` if the target is empty, or too large for the encoder.
    pub fn delta(&self, target: &[u8], max_size: Option<usize>) -> Option<Vec<u8>> {
        // git spells "no budget" as a zero `max_size`, so a real budget of zero
        // has to be answered here rather than passed on as its opposite.
        let max_size = match max_size {
            Some(0) => return None,
            Some(max) => c_ulong::try_from(max).ok()?,
            None => 0,
        };
        let target_size: c_ulong = target.len().try_into().ok()?;
        let mut delta_size: c_ulong = 0;

        // SAFETY: `self.index` is live, `target` is `target_size` readable
        // bytes, and `delta_size` is a writable `c_ulong`.
        let delta = unsafe {
            create_delta(
                self.index.as_ptr(),
                target.as_ptr().cast(),
                target_size,
                &mut delta_size,
                max_size,
            )
        };
        let delta = NonNull::new(delta)?;

        // SAFETY: on success the encoder returns `delta_size` initialised bytes,
        // allocated through the shim — so it is ours to copy out and free.
        let out = unsafe {
            let bytes =
                std::slice::from_raw_parts(delta.as_ptr().cast::<u8>(), delta_size as usize)
                    .to_vec();
            shim::gib_delta_free(delta.as_ptr().cast());
            bytes
        };
        Some(out)
    }
}

impl Drop for DeltaIndex {
    fn drop(&mut self) {
        // SAFETY: `self.index` came from `create_delta_index`, is live, and is
        // dropped exactly once. `self.base` outlives this call — it is dropped
        // after, being a later field.
        unsafe { free_delta_index(self.index.as_ptr()) };
    }
}

impl std::fmt::Debug for DeltaIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaIndex")
            .field("base_len", &self.base.len())
            .field("memory_usage", &self.memory_usage())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(base: &[u8]) -> DeltaIndex {
        DeltaIndex::new(Rc::from(base)).expect("an index")
    }

    /// Apply a delta the way a pack reader does, so the encoder's output is
    /// checked by the only thing that really matters: what it rebuilds.
    ///
    /// Deliberately an independent implementation rather than git's own
    /// `patch-delta.c` — two halves of the same C agreeing with each other
    /// would say nothing about whether the bytes are a valid delta.
    fn apply(base: &[u8], delta: &[u8]) -> Vec<u8> {
        let mut pos = 0;
        let mut read_size = || {
            let mut size = 0usize;
            let mut shift = 0;
            loop {
                let byte = delta[pos];
                pos += 1;
                size |= ((byte & 0x7f) as usize) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    return size;
                }
            }
        };
        let base_size = read_size();
        let target_size = read_size();
        assert_eq!(base_size, base.len(), "delta names a different base size");

        let mut out = Vec::with_capacity(target_size);
        while pos < delta.len() {
            let instruction = delta[pos];
            pos += 1;
            if instruction & 0x80 == 0 {
                // Insert: the opcode is the literal run's length.
                let len = instruction as usize;
                assert!(len > 0, "a zero-length insert is not a valid instruction");
                out.extend_from_slice(&delta[pos..pos + len]);
                pos += len;
            } else {
                let mut offset = 0usize;
                for (shift, bit) in [(0, 0x01), (8, 0x02), (16, 0x04), (24, 0x08)] {
                    if instruction & bit != 0 {
                        offset |= (delta[pos] as usize) << shift;
                        pos += 1;
                    }
                }
                let mut size = 0usize;
                for (shift, bit) in [(0, 0x10), (8, 0x20), (16, 0x40)] {
                    if instruction & bit != 0 {
                        size |= (delta[pos] as usize) << shift;
                        pos += 1;
                    }
                }
                if size == 0 {
                    size = 0x10000;
                }
                out.extend_from_slice(&base[offset..offset + size]);
            }
        }
        assert_eq!(out.len(), target_size, "delta rebuilt the wrong length");
        out
    }

    /// Encode and apply, returning the delta so a test can also say how big it
    /// was. Panics if the delta does not rebuild the target.
    fn roundtrip(base: &[u8], target: &[u8]) -> Vec<u8> {
        let delta = index(base).delta(target, None).expect("a delta");
        assert_eq!(apply(base, &delta), target, "the delta rebuilt wrong bytes");
        delta
    }

    /// The bytes git's encoder produces for a known input, pinned.
    ///
    /// Everything else here would still pass if the build silently picked up a
    /// different encoder, or if the shim's allocator handed back memory that
    /// merely happened to round-trip. This is the one test that says *this*
    /// encoder: the expected bytes were taken from `diff-delta.c` compiled and
    /// run outside this crate.
    #[test]
    fn matches_git_byte_for_byte() {
        let base = b"the quick brown fox jumps over the lazy dog\n";
        let target = b"the quick brown fox jumps over the lazy cat\n";

        let delta = index(base).delta(target, None).expect("a delta");
        assert_eq!(
            delta,
            &[
                // 44, 44: the base's size, then the target's.
                0x2c, 0x2c, //
                // Copy: 0x80 marks it, 0x10 says one size byte follows and no
                // offset byte does, so 40 bytes from offset 0 — up to "lazy ".
                0x90, 0x28, //
                // Insert the 4 bytes that differ: "cat\n".
                0x04, 0x63, 0x61, 0x74, 0x0a,
            ][..],
        );
        assert_eq!(apply(base, &delta), target);
    }

    /// A line inserted into a file: the delta is a copy, the new line, and
    /// another copy — a fraction of the size of the file itself.
    #[test]
    fn a_small_edit_makes_a_small_delta() {
        let base: String = (0..400).map(|i| format!("line {i}\n")).collect();
        let mut target = base.clone();
        target.insert_str(base.len() / 2, "an inserted line\n");

        let delta = roundtrip(base.as_bytes(), target.as_bytes());
        assert!(
            delta.len() < target.len() / 20,
            "delta of {} bytes for a {}-byte file is not a saving",
            delta.len(),
            target.len()
        );
    }

    /// The cases the instruction encoding has to get right, checked by
    /// rebuilding: an unchanged copy, a prefix, an append, a match longer than
    /// one copy instruction can name, and two buffers with nothing in common.
    #[test]
    fn deltas_rebuild_their_targets() {
        let base: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();

        roundtrip(&base, &base);
        roundtrip(&base, &base[..40_000]);
        let mut appended = base.clone();
        appended.extend_from_slice(b"a tail that was not there before\n");
        roundtrip(&base, &appended);
        // Longer than one copy instruction's 64KB, so the copy is split.
        assert!(base.len() > 0x10000);
        // Nothing in common: still rebuilds, out of literals.
        let unrelated: Vec<u8> = (0..5_000u32).map(|i| (i % 7 + 100) as u8).collect();
        roundtrip(&base, &unrelated);
    }

    /// A delta over budget is refused rather than returned: the budget is how a
    /// caller says storing the object whole would be better.
    #[test]
    fn a_delta_over_budget_is_refused() {
        let base: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let unrelated: Vec<u8> = (0..50_000u32).map(|i| (i % 13 + 3) as u8).collect();
        let index = index(&base);

        assert!(index.delta(&unrelated, Some(100)).is_none());
        // A budget of zero is a budget, not git's "unlimited" sentinel.
        assert!(index.delta(&unrelated, Some(0)).is_none());
        // And with no budget the same target does produce one.
        assert!(index.delta(&unrelated, None).is_some());
    }

    /// A repetitive base puts every block in one hash bucket, which is what the
    /// encoder's bucket culling is for; the delta still has to rebuild.
    #[test]
    fn a_degenerate_base_still_deltifies() {
        let base = vec![b'x'; 200_000];
        let mut target = base.clone();
        target.extend_from_slice(b"tail\n");
        roundtrip(&base, &target);
    }

    /// Empty inputs have no delta to build, and must not reach the C as a null
    /// or zero-length buffer.
    #[test]
    fn empty_inputs_are_refused() {
        assert!(DeltaIndex::new(Rc::from(&b""[..])).is_none());
        assert!(index(b"a base").delta(b"", None).is_none());
    }

    /// A base shorter than the 16-byte Rabin window indexes, but has no block
    /// to match against, so its deltas are all literal and bigger than the
    /// target. git behaves this way; a caller's budget is what rejects them.
    #[test]
    fn a_tiny_base_indexes_but_cannot_match() {
        let index = index(b"tiny\n");
        let target = b"a target with nothing to match\n";

        let delta = index.delta(target, None).expect("a delta, all literals");
        assert!(delta.len() > target.len(), "somehow matched something");
        assert_eq!(apply(b"tiny\n", &delta), target);
        // Which a caller's real budget declines.
        assert!(index.delta(target, Some(target.len() / 2)).is_none());
    }

    /// The index keeps the base alive on its own: the caller's handle can go
    /// away and the encoder still has valid bytes to point at.
    #[test]
    fn the_index_owns_its_base() {
        let base: Rc<[u8]> = Rc::from(&b"a base that outlives its caller's handle\n"[..]);
        let index = DeltaIndex::new(Rc::clone(&base)).unwrap();
        drop(base);

        let target = b"a base that outlives its caller's handle, extended\n";
        assert_eq!(
            apply(index.base(), &index.delta(target, None).unwrap()),
            target
        );
    }
}
