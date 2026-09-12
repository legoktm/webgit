//! Writing the bundle out: its header, and the packfile that follows it.
//!
//! A v2 bundle is a text header — the signature line, one line per ref, and a
//! blank line — with an ordinary packfile appended. Everything about the
//! container is byte-exact with git's, since git is what has to read it: the
//! signature, the `<oid> SP <refname> LF` lines, the pack's `PACK` magic,
//! version and object count, each object's type/size varint, and the SHA-1 of
//! the whole pack that closes it.
//!
//! Objects are deltified as git deltifies them: each one is offered the last
//! [`WINDOW`] objects written as possible bases, under the size and depth
//! heuristics of `try_delta` in `builtin/pack-objects.c`, and stored as an
//! `OFS_DELTA` against the best of them when that is smaller than storing it
//! whole. Without this a bundle is several times the size of the one
//! `git bundle create` writes, which for a file whose whole purpose is to be
//! downloaded is not a detail.

use gib_diff_delta::DeltaIndex;
use gib_object::{Object, ObjectId, ObjectType};
use miniz_oxide::deflate::compress_to_vec_zlib;
use sha1::{Digest, Sha1};
use std::collections::VecDeque;
use std::rc::Rc;

const SIGNATURE: &str = "# v2 git bundle\n";

/// How many previously-written objects an object may be deltified against.
const WINDOW: usize = 32;

/// The longest chain of deltas the writer will build — git's `--depth` default.
const MAX_DEPTH: u32 = 50;

/// How much object content the delta window may hold.
const WINDOW_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// git's pack type for an object stored as a delta against another object in
/// the same pack, named by how far back it sits.
const OFS_DELTA: u8 = 6;

/// The pack format this writes.
const PACK_VERSION: u32 = 2;

/// zlib level for each object's content.
const COMPRESSION_LEVEL: u8 = 6;

/// The largest bundle this will build, in bytes of packfile.
pub const MAX_BUNDLE_BYTES: usize = 256 * 1024 * 1024;

/// One line of the bundle's header: a ref, and the object it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRef {
    pub name: String,
    pub id: ObjectId,
}

impl BundleRef {
    /// A ref line for `name` pointing at `id`.
    pub fn new(name: impl Into<String>, id: ObjectId) -> Self {
        Self {
            name: name.into(),
            id,
        }
    }
}

/// A bundle being written, one object at a time.
pub struct BundleWriter {
    /// Bytes written but not yet handed out by [`take`](BundleWriter::take).
    out: Vec<u8>,
    /// The last few objects written, as candidate delta bases.
    window: VecDeque<Candidate>,
    /// What `window` holds, in bytes of object content.
    window_bytes: usize,
    /// SHA-1 over the packfile — from `PACK` to the last object, and not over
    /// the header lines above it, which are not part of the pack.
    hasher: Sha1,
    /// Pack bytes written so far, against [`MAX_BUNDLE_BYTES`].
    written: usize,
    /// How many objects have been appended, against the count in the header.
    appended: usize,
    /// How many the header promised.
    objects: usize,
}

impl BundleWriter {
    /// Open a bundle carrying `refs` and `objects` objects.
    pub fn new(refs: &[BundleRef], objects: usize) -> anyhow::Result<Self> {
        if refs.is_empty() {
            anyhow::bail!("a bundle must name at least one ref");
        }
        let mut out = Vec::from(SIGNATURE);
        for r in refs {
            // A space or a newline in a ref name would be read back as a
            // different (or a truncated) line, quietly producing a bundle that
            // says something other than what was asked for. git forbids both in
            // a ref name, so this can only be reached by a caller inventing
            // one.
            if r.name.is_empty() || r.name.contains([' ', '\n']) {
                anyhow::bail!("invalid ref name in bundle: {:?}", r.name);
            }
            out.extend_from_slice(format!("{} {}\n", r.id, r.name).as_bytes());
        }
        // The blank line that ends the header; the pack starts on the next byte.
        out.push(b'\n');

        let count: u32 = objects
            .try_into()
            .map_err(|_| anyhow::anyhow!("too many objects for one pack: {objects}"))?;
        let mut writer = Self {
            out,
            window: VecDeque::new(),
            window_bytes: 0,
            hasher: Sha1::new(),
            written: 0,
            appended: 0,
            objects,
        };
        writer.pack(b"PACK");
        writer.pack(&PACK_VERSION.to_be_bytes());
        writer.pack(&count.to_be_bytes());
        Ok(writer)
    }

    /// Write one object into the pack: as a delta against a recently written
    /// object where that is smaller, and whole otherwise.
    pub fn append(&mut self, object_type: ObjectType, body: &[u8]) -> anyhow::Result<()> {
        let offset = self.written;
        let depth = match self.best_delta(object_type, body) {
            Some(delta) => {
                let mut header = Vec::new();
                // A delta's recorded size is the delta's own, not the size of
                // what it rebuilds — that lives inside the delta data.
                write_pack_header(&mut header, OFS_DELTA, delta.data.len());
                // How far back the base sits, which is what makes a pack
                // self-contained: no id to look up, just a distance.
                write_base_distance(&mut header, offset - delta.base_offset);
                self.finish_object(header, &delta.data)?;
                delta.depth
            }
            None => {
                let mut header = Vec::new();
                write_pack_header(&mut header, type_bits(object_type), body.len());
                self.finish_object(header, body)?;
                0
            }
        };

        self.remember(Candidate {
            offset,
            depth,
            object_type,
            body: Rc::from(body),
            index: BaseIndex::Unbuilt,
        });
        self.appended += 1;
        Ok(())
    }

    /// Deflate `payload` behind `header`, checking the size cap before either
    /// reaches the pack.
    fn finish_object(&mut self, header: Vec<u8>, payload: &[u8]) -> anyhow::Result<()> {
        let deflated = compress_to_vec_zlib(payload, COMPRESSION_LEVEL);
        if self.written + header.len() + deflated.len() > MAX_BUNDLE_BYTES {
            anyhow::bail!(
                "This bundle is over the {} MiB limit for bundles built in the browser. \
                 Clone the repository instead.",
                MAX_BUNDLE_BYTES / (1024 * 1024)
            );
        }
        self.pack(&header);
        self.pack(&deflated);
        Ok(())
    }

    /// The smallest delta the window can offer for `body`, or `None` when
    /// storing it whole is the better answer.
    ///
    /// This is `try_delta` from `builtin/pack-objects.c`, candidate by
    /// candidate: same type only, never past the depth limit, and every
    /// attempt bounded by a budget that starts at half the object (a delta
    /// bigger than that is not worth the indirection) and tightens to the best
    /// delta found so far, so each `create_delta` call either beats the
    /// incumbent or gives up early. The budget is also scaled by how deep the
    /// candidate already is, which is what stops a long chain from being
    /// extended for a marginal saving.
    ///
    /// Candidates are tried newest first, as git tries them: the ordering puts
    /// like objects next to each other, so the nearest is the likeliest match.
    fn best_delta(&mut self, object_type: ObjectType, body: &[u8]) -> Option<Delta> {
        let target_size = body.len();
        let mut best: Option<Delta> = None;
        for i in (0..self.window.len()).rev() {
            let candidate = &mut self.window[i];
            if candidate.object_type != object_type || candidate.depth >= MAX_DEPTH {
                continue;
            }
            // The budget, and the depth the target would have if the delta
            // found so far were the one kept. git's budget for a first delta is
            // half the object less the 20 bytes of an object id — below 40
            // bytes that underflows in C to "no limit", where this reads it as
            // what it means: an object that small is not worth deltifying.
            let (max_size, ref_depth) = match &best {
                None => (target_size.saturating_sub(40) / 2, 1),
                Some(best) => (best.data.len(), best.depth),
            };
            let max_size = max_size * (MAX_DEPTH - candidate.depth) as usize
                / (MAX_DEPTH - ref_depth + 1) as usize;
            if max_size == 0 {
                continue;
            }
            let source_size = candidate.body.len();
            // A base much smaller than the target has to be made up in
            // literals, and a target much smaller than the base was never
            // really a revision of it.
            if target_size.saturating_sub(source_size) >= max_size || target_size < source_size / 32
            {
                continue;
            }
            candidate.build_index();
            let BaseIndex::Built(index) = &candidate.index else {
                continue;
            };
            let Some(data) = index.delta(body, Some(max_size)) else {
                continue;
            };
            // An equally small delta against a deeper base is a worse deal:
            // same bytes, more work to read.
            if let Some(best) = &best
                && data.len() == best.data.len()
                && candidate.depth + 1 >= best.depth
            {
                continue;
            }
            best = Some(Delta {
                data,
                base_offset: candidate.offset,
                depth: candidate.depth + 1,
            });
        }
        best
    }

    /// Keep `candidate` as a possible base for the objects after it, dropping
    /// whatever no longer fits in the window.
    fn remember(&mut self, candidate: Candidate) {
        self.window_bytes += candidate.body.len();
        self.window.push_back(candidate);
        while self.window.len() > WINDOW
            || (self.window_bytes > WINDOW_MEMORY_BYTES && self.window.len() > 1)
        {
            let Some(dropped) = self.window.pop_front() else {
                break;
            };
            self.window_bytes -= dropped.body.len();
        }
    }

    /// Write one object, taking its type and bytes from the object itself.
    ///
    /// The bodies are the ones the object was parsed from, so what is packed
    /// hashes back to the id it was fetched under — an object round-trips
    /// through `gib-object` unchanged.
    pub fn append_object(&mut self, object: &Object) -> anyhow::Result<()> {
        let (object_type, body) = match object {
            Object::Commit(c) => (ObjectType::Commit, c.body()),
            Object::Tree(t) => (ObjectType::Tree, t.body()),
            Object::Tag(t) => (ObjectType::Tag, t.body()),
            Object::Blob(b) => (ObjectType::Blob, b.data()),
        };
        self.append(object_type, body)
    }

    /// How much is waiting to be taken.
    pub fn pending(&self) -> usize {
        self.out.len()
    }

    /// Hand back everything written since the last [`take`](BundleWriter::take).
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Close the pack with its checksum and hand back the last piece.
    pub fn finish(mut self) -> anyhow::Result<Vec<u8>> {
        if self.appended != self.objects {
            anyhow::bail!(
                "bundle header promised {} objects but {} were written",
                self.objects,
                self.appended
            );
        }
        let checksum = self.hasher.finalize();
        self.out.extend_from_slice(&checksum);
        Ok(self.out)
    }

    /// Write bytes that are part of the packfile, and so of its checksum.
    fn pack(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.out.extend_from_slice(bytes);
        self.written += bytes.len();
    }
}

/// One object already in the pack, offered as a delta base to the objects that
/// follow it.
struct Candidate {
    /// Where its header sits in the pack, which is how a delta names it.
    offset: usize,
    /// How many deltas a reader must apply to rebuild it — one more than its
    /// own base's depth, or zero if it was written whole.
    depth: u32,
    object_type: ObjectType,
    /// The object's bytes. An `Rc` because [`DeltaIndex`] keeps a handle to
    /// them — git's index points into the base rather than copying it — and
    /// this way the window and the index share one allocation.
    body: Rc<[u8]>,
    index: BaseIndex,
}

/// A candidate's block index, built the first time it is actually tried as a
/// base. Most objects in a window are never a base for most of the objects
/// after them (a different type, or too far off in size), and indexing one
/// costs a pass over its bytes.
enum BaseIndex {
    Unbuilt,
    /// Nothing can be indexed here — an empty object.
    Unindexable,
    Built(DeltaIndex),
}

impl Candidate {
    /// Index this object for matching, if it hasn't been already.
    fn build_index(&mut self) {
        if matches!(self.index, BaseIndex::Unbuilt) {
            self.index = match DeltaIndex::new(Rc::clone(&self.body)) {
                Some(index) => BaseIndex::Built(index),
                None => BaseIndex::Unindexable,
            };
        }
    }
}

/// The best delta found for an object: the bytes, the base it rebuilds from,
/// and the depth it would give the object.
struct Delta {
    data: Vec<u8>,
    base_offset: usize,
    depth: u32,
}

/// The bit git packs an object's type into, for a whole (undeltified) object.
fn type_bits(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    }
}

/// Write an object's pack header: its type and the size of what follows the
/// header, in git's little-endian-first varint.
///
/// The first byte carries the type in bits 6–4 and the low four bits of the
/// size; each further byte carries seven more size bits, least significant
/// first, with the top bit set on every byte but the last. `type_bits` is the
/// raw pack type, so a delta passes [`OFS_DELTA`] rather than an
/// [`ObjectType`].
fn write_pack_header(out: &mut Vec<u8>, type_bits: u8, size: usize) {
    let mut byte = (type_bits << 4) | (size & 0x0f) as u8;
    let mut rest = size >> 4;
    while rest > 0 {
        out.push(byte | 0x80);
        byte = (rest & 0x7f) as u8;
        rest >>= 7;
    }
    out.push(byte);
}

/// Write how far back a delta's base sits, in the *other* varint git uses —
/// most significant group first, and with one subtracted from each group after
/// the last so that no distance has two encodings.
///
/// Deliberately not [`write_pack_header`]'s encoding, though both are called
/// varints and both live in a pack object's header, one after the other. See
/// `write_reused_pack_verbatim` in git's `pack-objects.c`, and `gib-pack`'s
/// `read_delta_offset`, which reads what this writes.
fn write_base_distance(out: &mut Vec<u8>, mut distance: usize) {
    let mut buf = [0u8; 10];
    let mut pos = buf.len() - 1;
    buf[pos] = (distance & 127) as u8;
    loop {
        distance >>= 7;
        if distance == 0 {
            break;
        }
        distance -= 1;
        pos -= 1;
        buf[pos] = 128 | (distance & 127) as u8;
    }
    out.extend_from_slice(&buf[pos..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::inflate::decompress_to_vec_zlib;

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    const A: &str = "5f7418b29afb886f7fb7f9b74d95b0ef7127235c";
    const B: &str = "7fc60ba8cd73219c1294198f2b7a179ae4cf0c27";

    /// The whole bundle, written in one go.
    fn write(refs: &[BundleRef], objects: &[(ObjectType, &[u8])]) -> Vec<u8> {
        let mut writer = BundleWriter::new(refs, objects.len()).unwrap();
        for (object_type, body) in objects {
            writer.append(*object_type, body).unwrap();
        }
        let mut out = writer.take();
        out.extend_from_slice(&writer.finish().unwrap());
        out
    }

    /// The header is the part git reads as text: signature, a line per ref, and
    /// a blank line before the pack.
    #[test]
    fn writes_the_v2_header() {
        let bundle = write(
            &[
                BundleRef::new("refs/tags/v1.0.0", oid(A)),
                BundleRef::new("HEAD", oid(B)),
            ],
            &[(ObjectType::Blob, b"hi\n")],
        );

        // The header ends at the blank line; everything after it is pack bytes,
        // which are not text at all.
        let end = bundle.windows(2).position(|w| w == b"\n\n").unwrap();
        let header = std::str::from_utf8(&bundle[..end]).unwrap();
        assert_eq!(
            header,
            format!("# v2 git bundle\n{A} refs/tags/v1.0.0\n{B} HEAD")
        );
    }

    /// The pack's own header: magic, version, and the object count that has to
    /// be known before anything is written.
    #[test]
    fn writes_the_pack_header() {
        let bundle = write(
            &[BundleRef::new("HEAD", oid(A))],
            &[(ObjectType::Blob, b"one\n"), (ObjectType::Blob, b"two\n")],
        );
        let pack = &bundle[bundle.windows(4).position(|w| w == b"PACK").unwrap()..];

        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(u32::from_be_bytes(pack[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(pack[8..12].try_into().unwrap()), 2);
    }

    /// The pack ends with the SHA-1 of everything in it, starting at `PACK` —
    /// the bundle's header lines are not part of the pack and not hashed.
    #[test]
    fn closes_the_pack_with_its_own_checksum() {
        let bundle = write(
            &[BundleRef::new("HEAD", oid(A))],
            &[(ObjectType::Blob, b"x")],
        );
        let pack_start = bundle.windows(4).position(|w| w == b"PACK").unwrap();
        let (pack, checksum) = bundle[pack_start..].split_at(bundle.len() - pack_start - 20);

        assert_eq!(checksum, &Sha1::digest(pack)[..]);
    }

    /// One object, read back the way git reads it: type and size out of the
    /// varint, content out of the zlib stream that follows.
    #[test]
    fn writes_an_object_git_can_read_back() {
        let body = b"blob body, long enough to need two size bytes in the header\n";
        let bundle = write(
            &[BundleRef::new("HEAD", oid(A))],
            &[(ObjectType::Blob, body)],
        );
        let pack_start = bundle.windows(4).position(|w| w == b"PACK").unwrap();
        let object = &bundle[pack_start + 12..];

        // Type 3 (blob), and a size of 60 spread over two bytes.
        assert_eq!(object[0] >> 4 & 0b111, 3);
        assert_eq!(object[0] & 0x80, 0x80, "the size continues");
        let size = (object[0] & 0x0f) as usize | ((object[1] & 0x7f) as usize) << 4;
        assert_eq!(size, body.len());
        assert_eq!(object[1] & 0x80, 0, "the size ends");

        let content = decompress_to_vec_zlib(&object[2..]).unwrap();
        assert_eq!(content, body);
    }

    /// The size varint, at the boundaries where it grows a byte.
    #[test]
    fn encodes_the_type_and_size_varint() {
        let mut out = Vec::new();
        write_pack_header(&mut out, type_bits(ObjectType::Commit), 0);
        assert_eq!(out, vec![0b0001_0000]);

        // The largest size the first byte holds on its own.
        out.clear();
        write_pack_header(&mut out, type_bits(ObjectType::Tree), 15);
        assert_eq!(out, vec![0b0010_1111]);

        // One more, and a second byte carries the rest.
        out.clear();
        write_pack_header(&mut out, type_bits(ObjectType::Blob), 16);
        assert_eq!(out, vec![0b1011_0000, 0b0000_0001]);

        // Three bytes: 4 bits, then 7, then 7 — 2^11 sets the topmost of them.
        out.clear();
        write_pack_header(&mut out, type_bits(ObjectType::Tag), 1 << 11);
        assert_eq!(out, vec![0b1100_0000, 0b1000_0000, 0b0000_0001]);
    }

    /// A bundle nothing can be fetched from is a mistake worth naming.
    #[test]
    fn refuses_a_bundle_with_no_refs() {
        assert!(BundleWriter::new(&[], 1).is_err());
    }

    /// A ref name that would break the header's one-line-per-ref format.
    #[test]
    fn refuses_a_ref_name_that_would_corrupt_the_header() {
        for name in ["", "refs/heads/a b", "refs/heads/a\nb"] {
            assert!(
                BundleWriter::new(&[BundleRef::new(name, oid(A))], 0).is_err(),
                "accepted {name:?}"
            );
        }
    }

    /// The count in the pack header is what a reader trusts; a writer that
    /// didn't fill it says so rather than handing over a corrupt pack.
    #[test]
    fn refuses_to_finish_short_of_the_promised_objects() {
        let mut writer = BundleWriter::new(&[BundleRef::new("HEAD", oid(A))], 2).unwrap();
        writer.append(ObjectType::Blob, b"one\n").unwrap();
        let err = writer.finish().expect_err("a short pack is refused");
        assert!(err.to_string().contains("promised 2 objects"), "{err}");
    }

    /// Taking a piece hands it over exactly once, and what is left still ends
    /// the file correctly.
    #[test]
    fn take_hands_each_byte_out_once() {
        let mut writer = BundleWriter::new(&[BundleRef::new("HEAD", oid(A))], 2).unwrap();
        writer.append(ObjectType::Blob, b"one\n").unwrap();
        let first = writer.take();
        assert!(writer.pending() == 0);
        writer.append(ObjectType::Blob, b"two\n").unwrap();

        let mut whole = first;
        whole.extend_from_slice(&writer.finish().unwrap());
        assert_eq!(
            whole,
            write(
                &[BundleRef::new("HEAD", oid(A))],
                &[(ObjectType::Blob, b"one\n"), (ObjectType::Blob, b"two\n")]
            )
        );
    }
}
